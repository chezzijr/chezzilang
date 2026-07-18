//! VM values: an 8-byte int-favoring pointer-tagged word.
//!
//! `Value` is `Copy` and exactly one machine word (8 bytes), so the operand stack is a cheap
//! `Vec<Value>` with good cache density across every `Value`-holding container (stack, `List`,
//! `Struct.fields`, `Map`/`Set` entries, `Closure.captured`, `Iter.items`).
//!
//! ## Tag layout (single source of truth)
//! ```text
//! bit0 = 1          → Int   : (n << 1) | 1   ; recover (v as i64) >> 1 ; inline range ±2^62
//! bit0 = 0, low3 =
//!    000            → Obj   : (gcref as u64) << 3            ; recover (v >> 3) as u32
//!    010            → Float : (floatbox_gcref << 3) | 0b010  ; its OWN tag → `is_float` is heap-free
//!    100            → Immediate (payload-less singleton); discriminate bits 3-4:
//!                      0b00_100 → NIL, 0b01_100 → FALSE, 0b10_100 → TRUE
//!    110            → RESERVED (future immediates)
//! ```
//!
//! An int outside ±2^62 is heap-boxed as [`super::heap::Obj::BigInt`] (an **Obj**-tagged `Value`);
//! a float is heap-boxed as [`super::heap::Obj::FloatBox`] (a **Float**-tagged `Value`). Both are
//! GC leaves. Construct wide ints via [`super::Vm::make_int`] and floats via [`super::Vm::box_float`]
//! (they need the heap); read them back via [`super::Vm::int_of`] / [`super::Vm::float_of`].
//!
//! The canonical-representation invariant: a given i64 is inline iff in ±2^62, boxed otherwise —
//! never both — so an inline `Int` and a boxed `BigInt` are never value-equal and int equality can
//! compare exact i64 without unwrapping across kinds. Boxed floats are per-alloc handles, so
//! equality/hash MUST compare the unwrapped f64, never the handle.
//!
//! NOTE: the derived `PartialEq` compares the raw word — it is **not** the language `==` (two
//! independently-boxed equal floats have different words). Use `Vm::values_equal` for language `==`.

/// A handle into the GC heap (`Heap::slots` index). `Copy`, so duplicating an `Obj` `Value` aliases
/// the same heap object — preserving by-reference sharing for lists / structs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GcRef(pub u32);

/// A runtime value: one 8-byte pointer-tagged word. See the module doc for the tag layout.
#[derive(Clone, Copy, PartialEq)]
pub struct Value(u64);

const TAG_INT_BIT: u64 = 1;
const TAG_MASK: u64 = 0b111;
const TAG_OBJ: u64 = 0b000;
const TAG_FLOAT: u64 = 0b010;
const NIL: u64 = 0b00_100;
const FALSE: u64 = 0b01_100;
const TRUE: u64 = 0b10_100;

impl Value {
    /// Largest i64 stored inline (larger boxes as `Obj::BigInt`). Inclusive.
    pub const INT_MAX_INLINE: i64 = (1 << 62) - 1;
    /// Smallest i64 stored inline (smaller boxes as `Obj::BigInt`). Inclusive.
    pub const INT_MIN_INLINE: i64 = -(1 << 62);

    /// Construct an **inline** int. `debug_assert`s the value fits the ±2^62 inline range — for a
    /// possibly-wide int use [`super::Vm::make_int`], which boxes out-of-range values.
    #[inline]
    pub fn int(n: i64) -> Value {
        debug_assert!(
            (Self::INT_MIN_INLINE..=Self::INT_MAX_INLINE).contains(&n),
            "Value::int out of inline range ({n}); use Vm::make_int for wide ints"
        );
        // Encode on the bit pattern: `n << 1` overflows i64 for n = 2^62 and panics in debug.
        Value(((n as u64) << 1) | TAG_INT_BIT)
    }
    #[inline]
    pub fn bool(b: bool) -> Value {
        Value(if b { TRUE } else { FALSE })
    }
    #[inline]
    pub fn nil() -> Value {
        Value(NIL)
    }
    #[inline]
    pub fn obj(r: GcRef) -> Value {
        Value((r.0 as u64) << 3 | TAG_OBJ)
    }
    /// A Float-tagged `Value` pointing at an `Obj::FloatBox`. Its own tag makes `is_float` heap-free.
    #[inline]
    pub fn float_ref(r: GcRef) -> Value {
        Value((r.0 as u64) << 3 | TAG_FLOAT)
    }

    #[inline]
    pub fn is_int(self) -> bool {
        self.0 & TAG_INT_BIT == TAG_INT_BIT
    }
    /// Recover the inline i64. Only valid for an inline `Int` (bit0=1); a boxed `BigInt` needs
    /// [`super::Vm::int_of`].
    #[inline]
    pub fn as_int(self) -> i64 {
        debug_assert!(self.is_int(), "as_int on non-int");
        (self.0 as i64) >> 1
    }
    /// The inline i64 iff this is an inline `Int`, else `None` (a boxed `BigInt` yields `None`).
    #[inline]
    pub fn as_int_inline(self) -> Option<i64> {
        if self.is_int() {
            Some((self.0 as i64) >> 1)
        } else {
            None
        }
    }
    /// A **true** `Obj` (tag 000): a heap object OR a boxed `BigInt`, but NOT a boxed float (its own
    /// Float tag). Matches the pre-swap semantics where floats were not `Obj`.
    #[inline]
    pub fn is_obj(self) -> bool {
        self.0 & TAG_MASK == TAG_OBJ
    }
    /// Heap-free: is this a boxed float (Float tag 010)?
    #[inline]
    pub fn is_float(self) -> bool {
        self.0 & TAG_MASK == TAG_FLOAT
    }
    /// The heap slot for an `Obj` OR a `Float` value (both store a `GcRef` in the high bits).
    #[inline]
    pub fn as_gcref(self) -> GcRef {
        GcRef((self.0 >> 3) as u32)
    }
    /// The heap slot iff this is a **true** `Obj` (tag 000: a heap object OR a boxed `BigInt`), else
    /// `None`. A boxed float (Float tag) yields `None` — use `is_float`/`Vm::float_of` for those.
    #[inline]
    pub fn as_obj(self) -> Option<GcRef> {
        if self.is_obj() {
            Some(self.as_gcref())
        } else {
            None
        }
    }
    #[inline]
    pub fn is_nil(self) -> bool {
        self.0 == NIL
    }
    #[inline]
    pub fn as_bool(self) -> Option<bool> {
        match self.0 {
            TRUE => Some(true),
            FALSE => Some(false),
            _ => None,
        }
    }
    /// The GC child handle for this value: `Some` for an `Obj` (000) OR a `Float` (010) — the two
    /// heap-backed tags — else `None`. GC uses this to trace boxed floats/big-ints alongside the
    /// container objects.
    #[inline]
    pub fn child_gcref(self) -> Option<GcRef> {
        match self.0 & TAG_MASK {
            TAG_OBJ | TAG_FLOAT => Some(self.as_gcref()),
            _ => None,
        }
    }

    /// A heap-free classification for `match` sites. Inline `Int`, `Bool`, `Nil`, or a heap handle
    /// (`Obj`). A **boxed float** and a **boxed big-int** both surface as `Obj(gcref)` here — resolve
    /// the concrete kind via `heap.get(gcref)` (`Obj::FloatBox` / `Obj::BigInt`), or use the
    /// `is_float` / `Vm::int_of` fast paths for the numeric hot loops.
    #[inline]
    pub fn view(self) -> ValueView {
        if self.0 & TAG_INT_BIT == TAG_INT_BIT {
            ValueView::Int((self.0 as i64) >> 1)
        } else {
            match self.0 {
                NIL => ValueView::Nil,
                TRUE => ValueView::Bool(true),
                FALSE => ValueView::Bool(false),
                _ => ValueView::Obj(self.as_gcref()), // Obj (000) or Float (010)
            }
        }
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.view() {
            ValueView::Int(n) => write!(f, "Int({n})"),
            ValueView::Bool(b) => write!(f, "Bool({b})"),
            ValueView::Nil => write!(f, "Nil"),
            ValueView::Obj(r) if self.is_float() => write!(f, "Float(@{})", r.0),
            ValueView::Obj(r) => write!(f, "Obj({})", r.0),
        }
    }
}

/// A heap-free classification of a [`Value`] for `match` sites. Boxed floats and big-ints both
/// surface as `Obj(GcRef)` (they need the heap to resolve); the numeric hot paths use
/// `Value::is_float` / `Vm::int_of` / `Vm::float_of` instead of going through this.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ValueView {
    Int(i64),
    Bool(bool),
    Nil,
    Obj(GcRef),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_is_8_bytes() {
        assert_eq!(std::mem::size_of::<Value>(), 8);
    }

    #[test]
    fn int_tag_roundtrips_including_boundary() {
        for n in [
            0i64,
            1,
            -1,
            Value::INT_MAX_INLINE,
            Value::INT_MIN_INLINE,
            1234567890123,
            -1234567890123,
        ] {
            assert_eq!(Value::int(n).as_int(), n, "n={n}");
            assert_eq!(Value::int(n).as_int_inline(), Some(n), "n={n}");
            assert!(Value::int(n).is_int(), "n={n}");
            assert!(matches!(Value::int(n).view(), ValueView::Int(m) if m == n));
        }
    }

    #[test]
    fn tag_classification_is_disjoint() {
        // GcRef(0) is a legitimate Obj (tag 000, payload 0) and must NOT collide with NIL.
        let z = Value::obj(GcRef(0));
        assert!(z.is_obj());
        assert!(!z.is_nil());
        assert_ne!(z, Value::nil());
        assert_eq!(z.as_gcref(), GcRef(0));

        assert!(Value::obj(GcRef(999)).is_obj());
        assert!(!Value::obj(GcRef(999)).is_int());
        assert!(!Value::obj(GcRef(999)).is_float());
        assert_eq!(Value::obj(GcRef(999)).as_gcref(), GcRef(999));

        let f = Value::float_ref(GcRef(7));
        assert!(f.is_float());
        assert!(!f.is_obj());
        assert!(!f.is_int());
        assert_eq!(f.as_gcref(), GcRef(7));
        assert!(matches!(f.view(), ValueView::Obj(r) if r == GcRef(7)));

        assert!(Value::nil().is_nil());
        assert!(matches!(Value::nil().view(), ValueView::Nil));
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert!(matches!(Value::bool(true).view(), ValueView::Bool(true)));
        assert_eq!(Value::int(5).as_bool(), None);
    }

    #[test]
    fn child_gcref_traces_obj_and_float_only() {
        assert_eq!(Value::obj(GcRef(3)).child_gcref(), Some(GcRef(3)));
        assert_eq!(Value::float_ref(GcRef(4)).child_gcref(), Some(GcRef(4)));
        assert_eq!(Value::int(9).child_gcref(), None);
        assert_eq!(Value::bool(true).child_gcref(), None);
        assert_eq!(Value::nil().child_gcref(), None);
    }
}
