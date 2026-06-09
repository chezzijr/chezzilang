//! B3.0 — the wire-format airlock value (`WireValue`).
//!
//! A `WireValue` is a serialized, owned mirror of the **sendable** value set (`concurrency.md` §7).
//! It is the form a value takes while crossing a task airlock: `Vm::to_wire` serializes a heap
//! `Value` into it (read-only, no allocation), and `Vm::from_wire` reconstructs a `Value` from it
//! into the destination heap. In B3.0 the destination is the *same* heap and behavior is
//! byte-identical to the old `deep_clone`; under B3.1+ the cores move to `Arc<…Core>` and under
//! B3.3 a `WireValue` is what actually crosses an OS-thread boundary (hence the owned, `GcRef`-free
//! data arms — the only `GcRef` is the by-reference `Handle` arm, which B3.1 replaces with the
//! shared `Arc` core).

use super::value::GcRef;

/// A `Send`-able serialization of a sendable [`Value`](super::value::Value).
///
/// Data arms (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`) own their contents recursively. Immutable
/// / by-reference objects (`Str`, callables, modules, and `Channel`/`Shared`/`Executor` handles)
/// cross as [`WireValue::Handle`] — the existing heap handle in B3.0 (same heap), becoming an
/// `Arc<…Core>` at B3.1. `Map`/`Set` carry their cached `u64` hashes so reconstruction never
/// re-hashes (byte-identical iteration order + index — see [`super::heap::MapData`]).
#[derive(Debug, Clone)]
pub enum WireValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    List(Vec<WireValue>),
    Tuple(Vec<WireValue>),
    /// `(cached hash, key, value)` triples in insertion order.
    Map(Vec<(u64, WireValue, WireValue)>),
    /// `(cached hash, element)` pairs in insertion order.
    Set(Vec<(u64, WireValue)>),
    Struct {
        name: Box<str>,
        fields: Vec<(Box<str>, WireValue)>,
    },
    Enum {
        ty: Box<str>,
        variant: Box<str>,
        payload: Vec<WireValue>,
    },
    /// A by-reference object carried across the airlock as its existing heap handle (B3.0,
    /// single-thread / same heap). Covers `Str`, `Func`/`Closure`/`Module`/`Native`, and the
    /// `Channel`/`Shared`/`Executor` handles. B3.1 replaces the shared-core handles here with the
    /// `Arc<…Core>` itself so they can cross a real thread boundary.
    Handle(GcRef),
}
