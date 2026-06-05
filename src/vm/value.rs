//! VM values: unboxed scalars + a handle (`GcRef`) into the GC heap for the reference types.
//!
//! `Value` is `Copy` and small, so the operand stack is a cheap `Vec<Value>`. The four scalars
//! (`Int`/`Float`/`Bool`/`Nil`) carry no heap cost; the six reference kinds (Str, List, Struct,
//! Enum, Func, Closure, Module) live in [`super::heap::Heap`] and are referred to by handle.
//!
//! NOTE: the derived `PartialEq` compares handles and treats `Int`/`Float` as distinct — it is
//! **not** the language `==`. Use `Vm::values_equal` for language equality.

/// A handle into the GC heap (`Heap::slots` index). `Copy`, so duplicating a `Value::Obj` aliases
/// the same heap object — preserving the interpreter's by-reference sharing for lists / structs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcRef(pub u32);

/// A runtime value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Obj(GcRef),
}
