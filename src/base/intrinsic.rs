//! base: the intrinsic protocol rows — which built-in receiver kind satisfies which std protocol
//! with no declaration behind it. The checker grants by them (`checker::proto`) and the VM's method
//! table answers by them (`base::method_table`): one copy, read by both.

use crate::base::hooks::Unhooked;

/// **The intrinsic-grant ↔ runtime-arm pairing table (W6-3's structural ratchet).**
///
/// One row per `(protocol, receiver-kind)` a built-in is granted conformance to
/// *intrinsically* — i.e. with NO user method behind it — by the `Checker::grant_intrinsic`
/// early-outs in `Checker::satisfies_args_d`. A row is a PROMISE that **an erased generic body under
/// that bound** (`fn f[T: Eq](x: T)` calling `x.eq(y)`) may CALL that method on that receiver kind, so
/// each row MUST be **callable at runtime** or the program type-checks and then faults `has no method`
/// (bug-hunt wave-6 W6-3, the check-OK-then-run-fault class).
///
/// **The erased bound is the whole of the promise — a row does NOT make the method callable on a bare
/// PROTOCOL-TYPED receiver.** `s.eq(t)` where `s: Shape` is `type Shape has no method 'eq'`, and that
/// rejection is correct: it is the object-safety rule (a protocol-typed value only offers the methods
/// its own protocol declares), not a gap in this table. The distinction matters for the
/// `("Eq", RecvKind::Protocol)` row, whose receiver KIND is itself a protocol: what it grants is
/// `T = <some protocol>` satisfying `Eq` inside an erased `[T: Eq]` body, where the concrete witness
/// settles the answer at runtime.
///
/// **Keyed on the receiver KIND, not just the protocol** — that is the axis W6-3 actually failed on:
/// `compare`/`str` *were* paired, but their `Vm::do_method_call` interceptions were type-gated
/// narrower than the checker's grant set, so boxed scalars fell through. A
/// protocol-keyed table cannot express, and so cannot enforce, that obligation. The kind string is
/// `Checker::intrinsic_recv_kind`'s classification of the granted `Ty` (`"?"` for an unclassified
/// one, which matches no row and therefore trips the assert).
///
/// A row of a protocol with no type parameters is also a METHOD of the receiver kind, callable
/// directly (`(5).str()`, wave 24 F4): `Checker::intrinsic_method` reads it.
///
/// The VM's method table reads these rows: each row puts the protocol's method in the row of
/// each runtime kind `RecvKind::method_rows` names (`base::method_table::MethodTable::build`), and
/// the kind's own entries (a native method, `iter` on a collection, `next` and `iter` on a cursor)
/// come before it. A row with no entry fails `tests/method_dispatch_grid.rs`.
///
/// The pairing is enforced three ways, so a NEW grant cannot silently skip its runtime arm:
/// * a bare `return Ok(())` in `satisfies_args_d` does not COMPILE — the success type is `Grant`,
///   whose only constructors are `Checker::grant_intrinsic` (which consults this table) and
///   `Grant::no_intrinsic_method` (documented for the arms that grant no callable method);
/// * `grant_intrinsic` `debug_assert`s that the `(protocol, kind)` it is granting has a row here (or
///   in `INTRINSIC_UNPAIRED`) — so widening a grant to one more TYPE fails the suite;
/// * `vm::tests::intrinsic_grants_all_have_vm_arms` type-checks AND runs a generated probe per row on
///   the VM, so a row must be genuinely granted *and* genuinely callable.
///
/// A row promises the one method its protocol's std decl declares itself (`hooks::protocol_method`);
/// an `IndexSet` row's `index` is the embedded `Index`'s, promised by that protocol's row.
/// `"struct"` is a coarsening — the `Iterable`/`Iterator` grants are only for a struct with `iter`/`next`
/// — so the probe receiver for that kind is one struct that satisfies both at once.
///
/// **Stays reserved-protocol-only.** A built-in witnessing a USER protocol (TICKET-024, W8-32,
/// `Checker::satisfies_native`) is a DIFFERENT kind of grant — it goes through
/// `Grant::no_intrinsic_method`, not this table, because a user protocol's bare name can never be
/// a row key here (every row key is a std protocol name, see
/// `is_std_protocol`), and the method behind that grant is a REAL native method with a runtime arm already, not
/// a promise this ratchet needs to police.
pub const INTRINSIC_PROTO_METHODS: &[(&str, RecvKind)] = &[
    // Comparable — int/float/bool/str scalars (`false` before `true`, Python and Rust `bool: Ord`).
    ("Comparable", RecvKind::Int),
    ("Comparable", RecvKind::Float),
    ("Comparable", RecvKind::Bool),
    ("Comparable", RecvKind::Str),
    ("Comparable", RecvKind::Bytes),
    ("Comparable", RecvKind::ByteArray),
    // A tuple / List is Comparable exactly when every element type is (TICKET-146).
    ("Comparable", RecvKind::Tuple),
    ("Comparable", RecvKind::List),
    // Eq — D1: EVERY receiver kind whose `==` this table can key a row on except `nil` (not
    // spellable as a value). Most rows are the structural derive (`Vm::values_equal`): the four
    // scalars are all here, as for `Comparable`;
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
    ("Eq", RecvKind::Int),
    ("Eq", RecvKind::Float),
    ("Eq", RecvKind::Str),
    ("Eq", RecvKind::Bool),
    ("Eq", RecvKind::Bytes),
    ("Eq", RecvKind::ByteArray),
    ("Eq", RecvKind::List),
    ("Eq", RecvKind::Set),
    ("Eq", RecvKind::Map),
    ("Eq", RecvKind::Tuple),
    ("Eq", RecvKind::Struct),
    ("Eq", RecvKind::Enum),
    ("Eq", RecvKind::Option),
    ("Eq", RecvKind::Result),
    ("Eq", RecvKind::Func),
    ("Eq", RecvKind::Protocol),
    ("Eq", RecvKind::Channel),
    ("Eq", RecvKind::Shared),
    ("Eq", RecvKind::RwShared),
    ("Eq", RecvKind::Atomic),
    ("Eq", RecvKind::Handle),
    ("Eq", RecvKind::Cursor),
    // Stringable — all four scalars.
    ("Stringable", RecvKind::Int),
    ("Stringable", RecvKind::Float),
    ("Stringable", RecvKind::Bool),
    ("Stringable", RecvKind::Str),
    ("Stringable", RecvKind::Bytes),
    ("Stringable", RecvKind::ByteArray),
    ("Stringable", RecvKind::List),
    ("Stringable", RecvKind::Map),
    ("Stringable", RecvKind::Set),
    ("Stringable", RecvKind::Tuple),
    ("Stringable", RecvKind::Struct),
    ("Stringable", RecvKind::Cursor),
    ("Stringable", RecvKind::Enum),
    ("Stringable", RecvKind::Option),
    ("Stringable", RecvKind::Result),
    ("Stringable", RecvKind::Func),
    ("Stringable", RecvKind::Protocol),
    ("Stringable", RecvKind::Channel),
    ("Stringable", RecvKind::Shared),
    ("Stringable", RecvKind::RwShared),
    ("Stringable", RecvKind::Atomic),
    ("Stringable", RecvKind::Handle),
    // Hashable — the scalar key types. A struct or enum needs its own `hash` and `eq` (TICKET-280).
    ("Hashable", RecvKind::Int),
    ("Hashable", RecvKind::Str),
    ("Hashable", RecvKind::Bytes),
    ("Hashable", RecvKind::Bool),
    // A tuple is Hashable exactly when every element type is (TICKET-161; CPython's rule).
    ("Hashable", RecvKind::Tuple),
    // Error — `str`'s `error()` is itself (Go model).
    (Unhooked::Error.protocol(), RecvKind::Str),
    // PathLike (W7-8) — the three byte-ish scalars a path can be spelled as. None of them HAS an
    // `as_path` method, so the grant is the only seam; `path.Path` conforms structurally instead.
    (Unhooked::PathLike.protocol(), RecvKind::Str),
    (Unhooked::PathLike.protocol(), RecvKind::Bytes),
    (Unhooked::PathLike.protocol(), RecvKind::ByteArray),
    // Contains — one verdict per builtin container (`builtin_contains_verdict`); the bytes rows serve
    // `Contains[int]`, `Contains[bytes]` and `Contains[bytearray]`.
    ("Contains", RecvKind::List),
    ("Contains", RecvKind::Set),
    ("Contains", RecvKind::Map),
    ("Contains", RecvKind::Str),
    ("Contains", RecvKind::Bytes),
    ("Contains", RecvKind::ByteArray),
    // Iterable — everything `iterable_elem` accepts.
    ("Iterable", RecvKind::List),
    ("Iterable", RecvKind::Set),
    ("Iterable", RecvKind::Map),
    ("Iterable", RecvKind::Str),
    ("Iterable", RecvKind::Bytes),
    ("Iterable", RecvKind::ByteArray),
    ("Iterable", RecvKind::Struct),
    ("Iterable", RecvKind::Channel),
    ("Iterable", RecvKind::Cursor),
    // Iterator — only a struct/cursor holds the position `next` needs (W6-3b: a raw collection does
    // NOT satisfy `Iterator`, only `Iterable`).
    ("Iterator", RecvKind::Struct),
    ("Iterator", RecvKind::Cursor),
    // Index / IndexSet / Slice — the built-in containers `index_kv`/`slice_result` accept
    // (`IndexSet` excludes the immutable `str`/`bytes`; `Slice` excludes `map`).
    ("Index", RecvKind::List),
    ("Index", RecvKind::Map),
    ("Index", RecvKind::Str),
    ("Index", RecvKind::Bytes),
    ("Index", RecvKind::ByteArray),
    ("IndexSet", RecvKind::List),
    ("IndexSet", RecvKind::Map),
    ("IndexSet", RecvKind::ByteArray),
    ("Slice", RecvKind::List),
    ("Slice", RecvKind::Str),
    ("Slice", RecvKind::Bytes),
    ("Slice", RecvKind::ByteArray),
    // The operator protocols — int/float natively.
    ("Add", RecvKind::Int),
    ("Add", RecvKind::Float),
    ("Add", RecvKind::Str),
    ("Add", RecvKind::Bytes),
    ("Add", RecvKind::ByteArray),
    ("Add", RecvKind::List),
    ("Sub", RecvKind::Int),
    ("Sub", RecvKind::Float),
    ("Sub", RecvKind::Set),
    ("Mul", RecvKind::Int),
    ("Mul", RecvKind::Float),
    ("Div", RecvKind::Int),
    ("Div", RecvKind::Float),
    ("Mod", RecvKind::Int),
    ("Mod", RecvKind::Float),
    ("Neg", RecvKind::Int),
    ("Neg", RecvKind::Float),
    ("BitAnd", RecvKind::Int),
    ("BitAnd", RecvKind::Set),
    ("BitOr", RecvKind::Int),
    ("BitOr", RecvKind::Set),
    ("BitXor", RecvKind::Int),
    ("BitXor", RecvKind::Set),
    ("Shl", RecvKind::Int),
    ("Shr", RecvKind::Int),
];

/// The one list of receiver kinds `INTRINSIC_PROTO_METHODS` is indexed by: the granted `Ty`'s
/// built-in shape, at the granularity the runtime dispatches on. `RecvKind::ALL` drives the VM
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
    /// A native handle `==` compares by its core (`Identity::Core` in `vm/heap.rs`): `Executor`,
    /// `Socket`, `Listener`, `Writer`, `Reader` and `ptr`.
    Handle,
}

impl RecvKind {
    pub const ALL: [RecvKind; 22] = [
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
        RecvKind::Handle,
    ];

    /// A kind with no element types and no declared methods: its row alone decides a protocol
    /// with no type parameters.
    pub fn is_scalar(self) -> bool {
        match self {
            RecvKind::Int
            | RecvKind::Float
            | RecvKind::Bool
            | RecvKind::Str
            | RecvKind::Bytes
            | RecvKind::ByteArray => true,
            RecvKind::List
            | RecvKind::Map
            | RecvKind::Set
            | RecvKind::Tuple
            | RecvKind::Struct
            | RecvKind::Cursor
            | RecvKind::Enum
            | RecvKind::Option
            | RecvKind::Result
            | RecvKind::Func
            | RecvKind::Protocol
            | RecvKind::Channel
            | RecvKind::Shared
            | RecvKind::RwShared
            | RecvKind::Atomic
            | RecvKind::Handle => false,
        }
    }

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
            RecvKind::Handle => "handle",
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
/// grant is added: registering a row here (instead of in `INTRINSIC_PROTO_METHODS`) is the only way to
/// ship a grant with no runtime arm, and `vm::tests::intrinsic_grants_all_have_vm_arms` then asserts the
/// row is still granted and STILL faults.
pub const INTRINSIC_UNPAIRED: &[(&str, RecvKind)] = &[];
