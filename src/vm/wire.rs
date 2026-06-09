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

use super::core::{ChannelCore, ExecutorCore, SharedCore};
use super::value::GcRef;
use std::sync::Arc;

/// A `Send`-able serialization of a sendable [`Value`](super::value::Value).
///
/// Data arms (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`) own their contents recursively. Immutable
/// / by-reference objects (`Str`, callables, modules, and `Channel`/`Shared`/`Executor` handles)
/// cross as [`WireValue::Handle`] — the existing heap handle in B3.0 (same heap), becoming an
/// `Arc<…Core>` at B3.1. `Map`/`Set` carry their cached `u64` hashes so reconstruction never
/// re-hashes (byte-identical iteration order + index — see [`super::heap::MapData`]).
#[derive(Debug, Clone, Default)]
pub enum WireValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// The default wire value (an empty `Shared` box starts here before its first `set`).
    #[default]
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
    /// A by-reference object carried across the airlock as its existing heap handle (single-thread /
    /// same heap). Covers `Str`, `Func`/`Closure`/`Module`/`Native`. At B3.3 the handles whose object
    /// cannot cross an OS thread (`Module` with mutable globals, `Func`/`Closure`) gain real `Err`
    /// arms in `to_wire`; `Str` will instead cross by value (an owned-bytes arm). For now (B3.1) all
    /// of them stay same-heap handles.
    Handle(GcRef),
    /// A `Channel` handle crossing the airlock as its shared [`ChannelCore`] (B3.1). `from_wire`
    /// allocates a *fresh* heap handle wrapping this same `Arc`, so two tasks reach one mailbox — the
    /// identity the language observes lives in the `Arc`, not the `GcRef`.
    Channel(Arc<ChannelCore>),
    /// A `Shared` handle crossing the airlock as its shared [`SharedCore`] (B3.1). See [`Channel`].
    Shared(Arc<SharedCore>),
    /// An `Executor` handle crossing the airlock as its shared [`ExecutorCore`] (B3.1). See [`Channel`].
    Executor(Arc<ExecutorCore>),
}

impl WireValue {
    /// B3.2 — does this value graph carry a by-reference [`Handle`](WireValue::Handle), i.e. a
    /// heap-local `GcRef`? Such a value cannot cross into another heap as-is — the slot index is
    /// meaningless there — so the worker airlock ([`Vm::run_task_isolated`](super::Vm)) rejects it
    /// with a clean fault until B3.3 teaches `Str`/closures to cross by value. The shared-core arms
    /// (`Channel`/`Shared`/`Executor`) are cross-safe — they carry an `Arc`, not a `GcRef` — so they
    /// are *not* flagged (the `Str` handles a `Channel[str]` queues *inside* its core are a separate
    /// B3.3 concern, not part of this value's directly-crossed graph).
    pub fn has_handle(&self) -> bool {
        match self {
            WireValue::Handle(_) => true,
            WireValue::List(xs) | WireValue::Tuple(xs) | WireValue::Enum { payload: xs, .. } => {
                xs.iter().any(WireValue::has_handle)
            }
            WireValue::Map(es) => es.iter().any(|(_, k, v)| k.has_handle() || v.has_handle()),
            WireValue::Set(es) => es.iter().any(|(_, e)| e.has_handle()),
            WireValue::Struct { fields, .. } => fields.iter().any(|(_, v)| v.has_handle()),
            _ => false,
        }
    }
}
