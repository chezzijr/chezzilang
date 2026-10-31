// checker::proto — split out of checker/mod.rs. `super::*` == the `checker` module.
// Protocol hoisting/embedding, satisfies, receiver refinement, hashability.

use super::*;
use std::collections::{HashMap, HashSet};

/// **The intrinsic-grant ↔ runtime-arm pairing table (W6-3's structural ratchet).**
///
/// One row per `(protocol, method, receiver-kind)` a built-in is granted conformance to
/// *intrinsically* — i.e. with NO user method behind it — by the [`Checker::grant_intrinsic`]
/// early-outs in [`Checker::satisfies_args_d`]. A row is a PROMISE that **an erased generic body under
/// that bound** (`fn f[T: Eq](x: T)` calling `x.eq(y)`) may CALL that method on that receiver kind, so
/// each row MUST be **callable at runtime** or the program type-checks and then faults `has no method`
/// (bug-hunt wave-6 W6-3, the check-OK-then-run-fault class).
///
/// **The erased bound is the whole of the promise — a row does NOT make the method callable on a bare
/// PROTOCOL-TYPED receiver.** `s.eq(t)` where `s: Shape` is `type Shape has no method 'eq'`, and that
/// rejection is correct: it is the object-safety rule (a protocol-typed value only offers the methods
/// its own protocol declares), not a gap in this table. The distinction matters for the
/// `("Eq", "eq", "protocol")` row, whose receiver KIND is itself a protocol: what it grants is
/// `T = <some protocol>` satisfying `Eq` inside an erased `[T: Eq]` body, where the concrete witness
/// settles the answer at runtime. An earlier wording said "an erased generic body (or a protocol-typed
/// value)", which overstated the row by exactly that second half.
///
/// **Keyed on the receiver KIND, not just the protocol** — that is the axis W6-3 actually failed on:
/// `compare`/`str` *were* paired, but their `Vm::do_method_call` interceptions were type-gated
/// narrower than the checker's grant set, so boxed scalars fell through. A
/// protocol-keyed table cannot express, and so cannot enforce, that obligation. The kind string is
/// [`Checker::intrinsic_recv_kind`]'s classification of the granted `Ty` (`"?"` for an unclassified
/// one, which matches no row and therefore trips the assert).
///
/// Which runtime site SERVES a row is deliberately not recorded — the three that do are the
/// pre-dispatch interceptions at the top of `Vm::do_method_call` (`compare`/`str` on a scalar, `iter`
/// on a collection, `next` on a struct-with-next), `Vm::core_method` (`Error.message`), and the
/// miss-only `Vm::intrinsic_proto_method` (everything else). The ratchet asserts CALLABILITY, which is
/// the invariant; pinning the site would just be a second thing to keep in sync.
///
/// The pairing is enforced three ways, so a NEW grant cannot silently skip its runtime arm:
/// * a bare `return Ok(())` in `satisfies_args_d` does not COMPILE — the success type is [`Grant`],
///   whose only constructors are [`Checker::grant_intrinsic`] (which consults this table) and
///   [`Grant::no_intrinsic_method`] (documented for the arms that grant no callable method);
/// * `grant_intrinsic` `debug_assert`s that the `(protocol, kind)` it is granting has a row here (or
///   in [`INTRINSIC_UNPAIRED`]) — so widening a grant to one more TYPE fails the suite;
/// * `vm::tests::intrinsic_grants_all_have_vm_arms` type-checks AND runs a generated probe per row on
///   the VM, so a row must be genuinely granted *and* genuinely callable.
///
/// Many-to-many on purpose: `IndexSet` contributes two methods, and `Index`/`IndexSet` share `index`.
/// `"struct"` is a coarsening — the `Iterable`/`Iterator` grants are only for a struct with `iter`/`next`
/// — so the probe receiver for that kind is one struct that satisfies both at once.
///
/// **Stays reserved-protocol-only.** A built-in witnessing a USER protocol (TICKET-024, W8-32,
/// `Checker::satisfies_native`) is a DIFFERENT kind of grant — it goes through
/// [`Grant::no_intrinsic_method`], not this table, because a user protocol's bare name can never be
/// a row key here (every row key is a std protocol name, see
/// `is_std_protocol`), and the method behind that grant is a REAL native method with a runtime arm already, not
/// a promise this ratchet needs to police.
pub const INTRINSIC_PROTO_METHODS: &[(&str, &str, RecvKind)] = &[
    // Comparable — int/float/str scalars.
    ("Comparable", "compare", RecvKind::Int),
    ("Comparable", "compare", RecvKind::Float),
    ("Comparable", "compare", RecvKind::Str),
    ("Comparable", "compare", RecvKind::Bytes),
    ("Comparable", "compare", RecvKind::ByteArray),
    // A tuple / List is Comparable exactly when every element type is (TICKET-146).
    ("Comparable", "compare", RecvKind::Tuple),
    ("Comparable", "compare", RecvKind::List),
    // Eq — D1: EVERY receiver kind whose `==` this table can key a row on except `nil` (not
    // spellable as a value). Most rows are the structural derive (`Vm::values_equal`): the four
    // scalars are all here because `==` is defined on `bool` too, unlike `Comparable`;
    // `option`/`result` land on `Obj::Enum` at runtime, same as `enum`. `func` is the one exception —
    // a function value's `==` is IDENTITY (two loads of the same top-level `fn`/nested `fn` def are
    // equal, two calls to a factory are not — W7-54), not a structural walk, but it is still the same
    // `values_equal_guarded` worker `==` uses, so `.eq()` can never disagree with `==`. `func` covers
    // every `Ty::Func`: a user closure or free fn, and a builtin fn value (`ord`/`chr`/`panic`/
    // `print`), which compares by the `Obj::Builtin` identity rule. `protocol` is a SECOND
    // exception, and a deliberately open one: a protocol-typed (existential) value's `==` DEFERS to
    // its concrete witness at runtime, exactly as Go's interface `==` defers to the dynamic type and
    // panics if that type is uncomparable — so this row grants the CHECK and the witness settles the
    // ANSWER (W7-52). TICKET-053: that runtime defer is now only reached for a witness whose OWN `eq`
    // has no unmet `where` bound — `Checker::assignable`'s protocol arm refuses the erasure itself
    // when one is unmet, so this row's promise now covers only the erased `[T: Eq]` body / a satisfied
    // witness's `==`, never an unsatisfied witness's (that never reaches the runtime at all). No other
    // protocol row exists for `"protocol"`:
    // `[T: Stringable]`/`[T: Hashable]`/… at `T = <protocol>` all still reject, because `Eq` is the one
    // protocol whose grant means exactly what `==` already accepts, and `==` already accepts a
    // protocol-typed operand (`may_be_equal`'s `(Protocol, Protocol)`/`(Protocol, concrete)` arms).
    ("Eq", "eq", RecvKind::Int),
    ("Eq", "eq", RecvKind::Float),
    ("Eq", "eq", RecvKind::Str),
    ("Eq", "eq", RecvKind::Bool),
    ("Eq", "eq", RecvKind::Bytes),
    ("Eq", "eq", RecvKind::ByteArray),
    ("Eq", "eq", RecvKind::List),
    ("Eq", "eq", RecvKind::Set),
    ("Eq", "eq", RecvKind::Map),
    ("Eq", "eq", RecvKind::Tuple),
    ("Eq", "eq", RecvKind::Struct),
    ("Eq", "eq", RecvKind::Enum),
    ("Eq", "eq", RecvKind::Option),
    ("Eq", "eq", RecvKind::Result),
    ("Eq", "eq", RecvKind::Func),
    ("Eq", "eq", RecvKind::Protocol),
    ("Eq", "eq", RecvKind::Channel),
    ("Eq", "eq", RecvKind::Shared),
    ("Eq", "eq", RecvKind::RwShared),
    ("Eq", "eq", RecvKind::Atomic),
    ("Eq", "eq", RecvKind::Cursor),
    // Stringable — all four scalars.
    ("Stringable", "str", RecvKind::Int),
    ("Stringable", "str", RecvKind::Float),
    ("Stringable", "str", RecvKind::Bool),
    ("Stringable", "str", RecvKind::Str),
    ("Stringable", "str", RecvKind::Bytes),
    ("Stringable", "str", RecvKind::ByteArray),
    ("Stringable", "str", RecvKind::List),
    ("Stringable", "str", RecvKind::Map),
    ("Stringable", "str", RecvKind::Set),
    ("Stringable", "str", RecvKind::Tuple),
    ("Stringable", "str", RecvKind::Struct),
    ("Stringable", "str", RecvKind::Cursor),
    ("Stringable", "str", RecvKind::Enum),
    ("Stringable", "str", RecvKind::Option),
    ("Stringable", "str", RecvKind::Result),
    ("Stringable", "str", RecvKind::Func),
    ("Stringable", "str", RecvKind::Protocol),
    ("Stringable", "str", RecvKind::Channel),
    ("Stringable", "str", RecvKind::Shared),
    ("Stringable", "str", RecvKind::RwShared),
    ("Stringable", "str", RecvKind::Atomic),
    // Hashable — the scalar key types. A struct or enum needs its own `hash` and `eq` (TICKET-280).
    ("Hashable", "hash", RecvKind::Int),
    ("Hashable", "hash", RecvKind::Str),
    ("Hashable", "hash", RecvKind::Bytes),
    ("Hashable", "hash", RecvKind::Bool),
    // A tuple is Hashable exactly when every element type is (TICKET-161; CPython's rule).
    ("Hashable", "hash", RecvKind::Tuple),
    // Error — `str`'s message is itself (Go model).
    ("Error", "message", RecvKind::Str),
    // PathLike (W7-8) — the three byte-ish scalars a path can be spelled as. None of them HAS an
    // `as_path` method, so the grant is the only seam; `path.Path` conforms structurally instead.
    ("PathLike", "as_path", RecvKind::Str),
    ("PathLike", "as_path", RecvKind::Bytes),
    ("PathLike", "as_path", RecvKind::ByteArray),
    // Contains — one verdict per builtin container (`builtin_contains_verdict`); the bytes rows serve
    // `Contains[int]`, `Contains[bytes]` and `Contains[bytearray]`.
    ("Contains", "contains", RecvKind::List),
    ("Contains", "contains", RecvKind::Set),
    ("Contains", "contains", RecvKind::Map),
    ("Contains", "contains", RecvKind::Str),
    ("Contains", "contains", RecvKind::Bytes),
    ("Contains", "contains", RecvKind::ByteArray),
    // Iterable — everything `iterable_elem` accepts.
    ("Iterable", "iter", RecvKind::List),
    ("Iterable", "iter", RecvKind::Set),
    ("Iterable", "iter", RecvKind::Map),
    ("Iterable", "iter", RecvKind::Str),
    ("Iterable", "iter", RecvKind::Bytes),
    ("Iterable", "iter", RecvKind::ByteArray),
    ("Iterable", "iter", RecvKind::Struct),
    ("Iterable", "iter", RecvKind::Channel),
    ("Iterable", "iter", RecvKind::Cursor),
    // Iterator — only a struct/cursor holds the position `next` needs (W6-3b: a raw collection does
    // NOT satisfy `Iterator`, only `Iterable`).
    ("Iterator", "next", RecvKind::Struct),
    ("Iterator", "next", RecvKind::Cursor),
    // Index / IndexSet / Slice — the built-in containers `index_kv`/`slice_result` accept
    // (`IndexSet` excludes the immutable `str`/`bytes`; `Slice` excludes `map`).
    ("Index", "index", RecvKind::List),
    ("Index", "index", RecvKind::Map),
    ("Index", "index", RecvKind::Str),
    ("Index", "index", RecvKind::Bytes),
    ("Index", "index", RecvKind::ByteArray),
    ("IndexSet", "index", RecvKind::List),
    ("IndexSet", "index", RecvKind::Map),
    ("IndexSet", "index", RecvKind::ByteArray),
    ("IndexSet", "set_index", RecvKind::List),
    ("IndexSet", "set_index", RecvKind::Map),
    ("IndexSet", "set_index", RecvKind::ByteArray),
    ("Slice", "slice", RecvKind::List),
    ("Slice", "slice", RecvKind::Str),
    ("Slice", "slice", RecvKind::Bytes),
    ("Slice", "slice", RecvKind::ByteArray),
    // The operator protocols — int/float natively.
    ("Add", "add", RecvKind::Int),
    ("Add", "add", RecvKind::Float),
    ("Add", "add", RecvKind::Str),
    ("Add", "add", RecvKind::Bytes),
    ("Add", "add", RecvKind::ByteArray),
    ("Add", "add", RecvKind::List),
    ("Sub", "sub", RecvKind::Int),
    ("Sub", "sub", RecvKind::Float),
    ("Sub", "sub", RecvKind::Set),
    ("Mul", "mul", RecvKind::Int),
    ("Mul", "mul", RecvKind::Float),
    ("Div", "div", RecvKind::Int),
    ("Div", "div", RecvKind::Float),
    ("Mod", "mod", RecvKind::Int),
    ("Mod", "mod", RecvKind::Float),
    ("Neg", "neg", RecvKind::Int),
    ("Neg", "neg", RecvKind::Float),
    ("BitAnd", "bitand", RecvKind::Int),
    ("BitAnd", "bitand", RecvKind::Set),
    ("BitOr", "bitor", RecvKind::Int),
    ("BitOr", "bitor", RecvKind::Set),
    ("BitXor", "bitxor", RecvKind::Int),
    ("BitXor", "bitxor", RecvKind::Set),
    ("Shl", "shl", RecvKind::Int),
    ("Shr", "shr", RecvKind::Int),
];

/// The one list of receiver kinds [`INTRINSIC_PROTO_METHODS`] is indexed by: the granted `Ty`'s
/// built-in shape, at the granularity the runtime dispatches on. [`RecvKind::ALL`] drives the VM
/// probe and the operator/bound grid; `recv_kind_all_lists_every_variant` pins it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecvKind {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    ByteArray,
    List,
    Map,
    Set,
    Tuple,
    Struct,
    Cursor,
    Enum,
    Option,
    Result,
    Func,
    Protocol,
    Channel,
    Shared,
    RwShared,
    Atomic,
}

impl RecvKind {
    #[cfg(any(test, feature = "test-api"))]
    pub const ALL: [RecvKind; 21] = [
        RecvKind::Int,
        RecvKind::Float,
        RecvKind::Bool,
        RecvKind::Str,
        RecvKind::Bytes,
        RecvKind::ByteArray,
        RecvKind::List,
        RecvKind::Map,
        RecvKind::Set,
        RecvKind::Tuple,
        RecvKind::Struct,
        RecvKind::Cursor,
        RecvKind::Enum,
        RecvKind::Option,
        RecvKind::Result,
        RecvKind::Func,
        RecvKind::Protocol,
        RecvKind::Channel,
        RecvKind::Shared,
        RecvKind::RwShared,
        RecvKind::Atomic,
    ];

    pub fn name(self) -> &'static str {
        // Display below reads this; keep the match exhaustive.
        match self {
            RecvKind::Int => "int",
            RecvKind::Float => "float",
            RecvKind::Bool => "bool",
            RecvKind::Str => "str",
            RecvKind::Bytes => "bytes",
            RecvKind::ByteArray => "bytearray",
            RecvKind::List => "list",
            RecvKind::Map => "map",
            RecvKind::Set => "set",
            RecvKind::Tuple => "tuple",
            RecvKind::Struct => "struct",
            RecvKind::Cursor => "cursor",
            RecvKind::Enum => "enum",
            RecvKind::Option => "option",
            RecvKind::Result => "result",
            RecvKind::Func => "func",
            RecvKind::Protocol => "protocol",
            RecvKind::Channel => "channel",
            RecvKind::Shared => "shared",
            RecvKind::RwShared => "rwshared",
            RecvKind::Atomic => "atomic",
        }
    }
}

impl std::fmt::Display for RecvKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Intrinsic grants that have NO runtime arm and CANNOT get one — a KNOWN check-OK/run-fault, kept
/// here (rather than silently absent) so the carve-out is asserted instead of rotting.
///
/// **Currently none.** W6-3b retired the only entry: `Iterator[E]`'s `next` used to be granted to every
/// RAW collection (the grant was keyed on the old can-be-iterated predicate), but `next` is stateful
/// and a raw collection holds no cursor position, so it check-OK'd and then faulted. The fix was the
/// coherent one — narrow the checker grant to real cursors/generators/`next`-structs (see
/// `satisfies_args_d`'s `Iterator` arm); a raw collection satisfies only `Iterable`.
///
/// The const and both `vm::tests` loops over it stay so the ratchet RE-ARMS the moment a new unpairable
/// grant is added: registering a row here (instead of in [`INTRINSIC_PROTO_METHODS`]) is the only way to
/// ship a grant with no runtime arm, and `vm::tests::intrinsic_grants_all_have_vm_arms` then asserts the
/// row is still granted and STILL faults.
pub const INTRINSIC_UNPAIRED: &[(&str, &str, RecvKind)] = &[];

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
            Ty::Struct(n, a) if n == "Iterator" && a.len() == 1 => Some(RecvKind::Cursor),
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
            Ty::Param(_)
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

    /// The `self.structs` key a built-in's harvested `native struct` method table sits under, for
    /// the ONE receiver kinds a USER protocol may witness against (TICKET-024, W8-32) — `None` for
    /// everything else (handles, tuples, `Option`/`Result`, functions), which keeps falling to the
    /// catch-all message. The three scalars have no `self.structs` entry (`std/prelude.chz:29-31`
    /// declares them `native ctor` only), so routing them here anyway reaches an empty table and
    /// IMPROVES the message: "type int does not satisfy Sized (missing method 'len')" instead of
    /// the bare clause.
    pub(super) fn native_witness_key(ty: &Ty) -> Option<&'static str> {
        match ty.scalar() {
            Ty::List(_) => Some("List"),
            Ty::Map(..) => Some("Map"),
            Ty::Set(_) => Some("Set"),
            Ty::Str => Some("str"),
            Ty::Bytes => Some("bytes"),
            Ty::ByteArray => Some("bytearray"),
            Ty::Int => Some("int"),
            Ty::Float => Some("float"),
            Ty::Bool => Some("bool"),
            Ty::Nil
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
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// The BUILT-IN cursor shape — `Ty::Struct("Iterator", [E])`, what `.iter()` mints and a
    /// generator returns. It is a `Ty::Struct` but NOT a struct at runtime (`Obj::Iterator` /
    /// `Obj::Generator`), and its dispatch arms deliberately expose only `next`/`iter`, so a
    /// protocol grant keyed on `"struct"` would be check-OK-then-`has no method` for it.
    pub(super) fn is_cursor_ty(ty: &Ty) -> bool {
        matches!(ty, Ty::Struct(n, a) if n == "Iterator" && a.len() == 1)
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
                .any(|(p, _, k)| *p == protocol && Some(*k) == kind),
            "intrinsic conformance granted for ({protocol}, {kind:?}) with no row in \
             INTRINSIC_PROTO_METHODS — add the (protocol, method, kind) row AND make the method \
             callable at runtime (`Vm::intrinsic_proto_method`), or register the row in \
             INTRINSIC_UNPAIRED (W6-3)"
        );
        Ok(Grant::no_intrinsic_method())
    }

    /// The method requirements of a `protocol` declaration, resolved with `Self` and the protocol's
    /// own type params in scope. `hoist_protocol` and `harvest_protocol_stmt` both call it.
    pub(super) fn protocol_method_sigs(
        &mut self,
        methods: &[MethodSig],
        span: Span,
    ) -> Vec<(String, FnSig)> {
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
                // is load-bearing for `Convert`-style static-ctor protocols: it drives both the
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
                methods: sigs,
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
            for (mn, ms) in &pinfo.methods {
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
        if let Some((_, sig)) = pinfo.methods.iter().find(|(n, _)| n == method) {
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
        out.extend(pinfo.methods.iter().map(|(n, _)| n.clone()));
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
