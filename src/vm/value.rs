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

/// A fully-decoded view of a [`Value`], regardless of its underlying representation. Phase 0 maps
/// 1:1 onto the enum variants; once `Value` becomes an 8-byte tagged `struct`, `view()` becomes the
/// canonical `match` seam so call sites read `match v.view() { ValueView::Int(..) => .. }` instead
/// of matching raw bits. `PartialEq` (not `Eq`/`Hash`) because it carries an `f64`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueView {
    Int(i64),
    Float(f64),
    Bool(bool),
    Nil,
    Obj(GcRef),
}

impl Value {
    #[inline]
    pub fn int(n: i64) -> Value {
        Value::Int(n)
    }
    /// **Phase-1 seam:** float construction will move onto the Vm (`Vm::box_float(f64) -> Value`)
    /// because the 8-byte tagged `Value` cannot hold an f64 inline — it must box it on the heap,
    /// which needs `&mut Vm`. `Value::float` is retained only for the future inlined-flonum path
    /// (immediate-representable floats), so keep it a pure `f64 -> Value` constructor here.
    #[inline]
    pub fn float(f: f64) -> Value {
        Value::Float(f)
    }
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value::Bool(b)
    }
    #[inline]
    pub fn nil() -> Value {
        Value::Nil
    }
    #[inline]
    pub fn obj(r: GcRef) -> Value {
        Value::Obj(r)
    }

    #[inline]
    pub fn is_int(self) -> bool {
        matches!(self, Value::Int(_))
    }
    #[inline]
    pub fn as_int(self) -> i64 {
        match self {
            Value::Int(n) => n,
            _ => panic!("as_int on non-int"),
        }
    }
    #[inline]
    pub fn is_obj(self) -> bool {
        matches!(self, Value::Obj(_))
    }
    #[inline]
    pub fn as_gcref(self) -> GcRef {
        match self {
            Value::Obj(r) => r,
            _ => panic!("as_gcref on non-obj"),
        }
    }
    #[inline]
    pub fn is_nil(self) -> bool {
        matches!(self, Value::Nil)
    }
    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        if let Value::Bool(b) = self {
            Some(b)
        } else {
            None
        }
    }

    #[inline]
    pub fn view(self) -> ValueView {
        match self {
            Value::Int(n) => ValueView::Int(n),
            Value::Float(f) => ValueView::Float(f),
            Value::Bool(b) => ValueView::Bool(b),
            Value::Nil => ValueView::Nil,
            Value::Obj(r) => ValueView::Obj(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_api_roundtrips() {
        assert!(matches!(Value::int(42).view(), ValueView::Int(42)));
        assert!(matches!(Value::bool(true).view(), ValueView::Bool(true)));
        assert!(matches!(Value::nil().view(), ValueView::Nil));
        assert!(matches!(
            Value::obj(GcRef(7)).view(),
            ValueView::Obj(GcRef(7))
        ));
        assert_eq!(Value::int(-5).as_int(), -5);
        assert!(Value::nil().is_nil());
    }
}
