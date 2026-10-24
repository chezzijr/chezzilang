//! Dynamic C-ABI FFI (v1): the runtime machinery behind an `extern "lib":` block. A [`Cffi`]
//! wraps a `dlopen`'d shared library, a resolved symbol address, and the C signature (as
//! [`CType`]s), and exposes `call(&mut dyn Host)` — reusing the engine-neutral [`Host`]/[`NativeRet`]
//! seam (`src/native/mod.rs`) used by the sole M:N VM engine.
//!
//! v1 marshals scalars: `int` (i64 ↔ C `long`), `float` (f64 ↔ C `double`), `bool`
//! (↔ C `_Bool`, 1 byte 0/1), and `str` (Chezzi str → null-terminated `const char*`; a borrowed `char*` return
//! is copied immediately into an owned Chezzi str, never freed). A `str` arg's marshalled C buffer is
//! RETAINED (leaked, keyed by content) instead of freed whenever the call could hand a pointer back
//! into it — a `ptr` return that aliases the argument, or any `ptr` parameter that may be a C
//! out-param (`RETAINED_STR_ARGS`, `retain_aliasing_str_args`) — so such a pointer stays valid past
//! the call that produced it. Plus the bidirectional fixed-width
//! integers `int8`/`int16`/`int32`/`int64`/`uint8`/`uint16`/`uint32`/`uint64` ([`CType::Int8`]..
//! [`CType::UInt64`]) for C `int32_t`/`uint32_t`/… — distinct from `int` (C `long`); a param truncates
//! the i64 to the C width (wrapping, C-cast), a return sign-/zero-extends back to i64. Plus opaque
//! `ptr` (Chezzi `ptr` ↔ C `void*`): an untyped raw-address handle, passed/returned by value and
//! never auto-freed (the caller calls the library's own destroy).
//!
//! Two RETURN-ONLY opt-in `str` forms deepen the `char*` return path (no grammar change — both ride
//! on a `Type` the backends' `ctype_of` recognizes, exactly like `ptr`):
//! - **`owned_str`** ([`CType::OwnedStr`]): an OWNED malloc'd `char*` (e.g. `strdup`). Copied into a
//!   Chezzi `str`, then **freed** with the loaded libc's `free` (resolved once via `dlsym("free")` at
//!   [`Cffi::new`]). To the program it is a plain `str`. NULL faults like a plain `str` return — use
//!   `owned_str?` for nullable. Caveat: the user asserts the buffer is genuinely malloc'd; declaring a
//!   STATIC/interned string `owned_str` corrupts the heap (a C-trust-boundary assertion, like the
//!   non-NUL-terminated-return over-read). A user-named deallocator is not supported (libc `free` only).
//! - **`str?`** ([`CType::OptStr`], surface `Option[str]`): a nullable `char*` (e.g. `getenv`). NULL →
//!   `None`, non-null → `Some(str)` (borrowed, not freed). The opt-in escape from the non-null `str`
//!   faulting-on-NULL rule. Composes: `owned_str?` ([`CType::OptOwnedStr`]) is nullable + owned.
//!
//! Plus (TICKET-217) `float32` ([`CType::Float32`], C `float`: a param rounds the f64 to nearest
//! f32, a return widens exactly), structs by value nested to any depth ([`CType::Struct`]; one
//! recursive [`write_field`]/[`read_field`] at libffi offsets), and C varargs (a `c_variadic` [`Cffi`]
//! reads each surplus arg through `Host::arg_c_value`, types it with [`vararg_ctype_of`] — C's default
//! argument promotions — and calls through `ffi_prep_cif_var`).
//!
//! `str` struct fields, stored/cross-thread callbacks, the rich Rust `Arc<dyn Any>` userdata handle,
//! and a custom user-named deallocator are deferred (documented limits).
//!
//! ## Send/Sync (for `--parallel`)
//! The VM stores `Obj::Cffi(Arc<Cffi>)`, and the M:N engine shares the parent address space across
//! OS-thread workers, so [`Cffi`] must be `Send + Sync`. Two design choices make that hold:
//! - libloading's `Symbol` is `!Send`, so the resolved symbol is stored as a raw `usize` address
//!   (cast to a `CodePtr` at call time) with the `Library` kept alive in the same `Arc`.
//! - libffi's `Cif` is `!Send` (raw pointers, no `unsafe impl Send`), so it is **not** stored;
//!   the cheap `Cif::new` (`prep_cif`) is rebuilt per call from the `Send` [`CType`] signature.

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_void};
use std::mem::ManuallyDrop;
use std::sync::{Mutex, OnceLock};

use libffi::middle::{Cif, CodePtr, Type, arg};
use libloading::Library;

use super::{Host, HostError, NativeRet};

/// A C-marshallable type — the v1 FFI surface. `int`→C `long`, `float`→C `double`,
/// `bool`→C `_Bool` (1 byte), `str`→C `const char*`, `ptr`→C `void*` (an opaque handle), the fixed-width
/// integers (`int8`..`uint64`), C `float` (`float32`), and a struct-by-value (`Struct`) of those,
/// nested to any depth.
///
/// Not `Copy`: the `Struct` variant carries owned data (`String`/`Vec`). It carries **only** owned
/// data, never a libffi `Type`/`Cif` (which are `!Send`/`!Sync`/`!Clone`): the libffi structure
/// `Type` is rebuilt per call from `fields` — exactly as the [`Cif`] is already rebuilt per call —
/// so [`CType`] (and the `Arc<Cffi>` that stores it) stays `Send + Sync` for `--parallel`/the M:N
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CType {
    Int,
    Float,
    Bool,
    Str,
    /// An opaque `void*` handle (Chezzi `ptr`): a raw address marshalled by value, in and out.
    Ptr,
    /// A C struct passed/returned BY VALUE. Fields are C scalars or nested `Struct`s, to any depth
    /// (`str`/`owned_str` fields are rejected by the checker). `name`/`field_names` mirror the Chezzi
    /// `struct` so a by-value RETURN lowers to a [`NativeRet::Struct`] the VM already knows how to build; the
    /// libffi structure `Type` (and per-field offsets/size/alignment) is computed from `fields` at call
    /// time via [`struct_layout`] — never stored — so the platform ABI (small-struct-in-registers vs
    /// by-hidden-pointer) is libffi's, not hand-rolled. A nested field sits at its parent's offset
    /// plus its own libffi offset (`write_field`/`read_field` recurse).
    Struct {
        name: String,
        field_names: Vec<String>,
        fields: Vec<CType>,
    },
    /// A RETURN-ONLY `char*` whose ownership transfers to Chezzi (surface type name `owned_str`).
    /// The returned pointer is copied into a Chezzi `str` and then **freed** with the loaded libc's
    /// `free` (a malloc'd buffer, e.g. `strdup`). The program still sees a plain `str`. NULL faults
    /// exactly like a plain `str` return (use `owned_str?` for a nullable owned return).
    OwnedStr,
    /// A RETURN-ONLY nullable `char*` (surface type `str?` / `Option[str]`): NULL → `None`,
    /// non-null → `Some(str)` (copied, **not** freed — borrowed). The opt-in escape from the
    /// non-null `str` faulting-on-NULL rule, for C fns that legitimately return NULL (e.g. `getenv`).
    OptStr,
    /// A RETURN-ONLY nullable, owned `char*` (surface type `owned_str?`): NULL → `None` (frees
    /// nothing), non-null → `Some(str)` copied **and** freed. The composition of `OwnedStr` + `OptStr`.
    OptOwnedStr,
    /// A fixed-width C integer (surface type names `int8`..`uint64`): a BIDIRECTIONAL marshalling
    /// distinction for C functions taking/returning `int32_t`, `uint32_t`, etc. — distinct from
    /// [`CType::Int`], which stays C `long`. To the Chezzi program each value is a plain `int`. A
    /// value written for C (param, struct field, `ffi.store_<w>`, callback return) must fit the width
    /// ([`CType::check_fits`]): out of range is a recoverable fault, never a silent wrap (`ffi.cast_<w>`
    /// wraps explicitly); uint64 accepts any int as its bit pattern. A RETURN sign-extends (signed) or
    /// zero-extends (unsigned) the C value back to i64, so it is in range by construction (a uint64
    /// above `i64::MAX` arrives negative). Valid as both param and return.
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    /// A C `float` (surface type name `float32`, TICKET-217). To the program it is a plain `float`
    /// (f64): a PARAM rounds to nearest f32 (`as f32`), a RETURN widens exactly. A finite value
    /// outside the f32 range faults at the C boundary (TICKET-218, Python `struct.pack('f', 1e39)`);
    /// NaN and inf pass through. Valid as param, return and field.
    Float32,
    /// A C function pointer passed as a PARAM (callbacks #4): a Chezzi closure marshalled into a
    /// libffi closure trampoline whose code address is the `void*` C receives. Params and the return
    /// are restricted to C SCALARS only (`is_scalar`) — no `str`/struct/nested callback. Sync +
    /// same-thread, and now ENFORCED as such: the trampoline is NOT freed when `call` returns — it is
    /// POISONED (its armed flag cleared) and LEAKED, and it also checks the invoking thread, so a C
    /// library that STORED the pointer (`signal`, `atexit`, GLib) and calls back later — or calls it
    /// from its own thread at any time — hits a named `abort()` instead of executing freed memory or
    /// re-entering the engine off-thread (gaps.md W6-8). See [`CallbackClosure`]. RETURN
    /// position is rejected by the checker (a callback can only be a parameter). The C signature is a
    /// plain `void*` to libffi, so `ffi_type` is `Type::pointer()`.
    Callback {
        params: Vec<CType>,
        /// `None` means VOID (`-> nil`), exactly like [`Cffi::ret`] — there is no `CType::Void`
        /// variant (see that field's doc). `callback_trampoline` must skip EVERY `write_c_result`
        /// site when this is `None`: libffi documents a void closure's result buffer as garbage.
        ret: Option<Box<CType>>,
    },
}

impl CType {
    /// The `std.ffi` name of a C width (`Int8` → `"int8"`), the inverse of [`width_ctype`]. `None`
    /// for every non-width C type.
    pub fn width_name(&self) -> Option<&'static str> {
        WIDTHS.iter().find(|(_, c)| c == self).map(|(n, _)| *n)
    }

    /// The inclusive integer range of an integer C width, from Rust's own primitive bounds — the
    /// one owner of every width range (TICKET-218). `None` for `Float32` and every non-width type.
    pub fn int_range(&self) -> Option<(i128, i128)> {
        Some(match self {
            CType::Int8 => (i8::MIN as i128, i8::MAX as i128),
            CType::Int16 => (i16::MIN as i128, i16::MAX as i128),
            CType::Int32 => (i32::MIN as i128, i32::MAX as i128),
            CType::Int64 => (i64::MIN as i128, i64::MAX as i128),
            CType::UInt8 => (0, u8::MAX as i128),
            CType::UInt16 => (0, u16::MAX as i128),
            CType::UInt32 => (0, u32::MAX as i128),
            CType::UInt64 => (0, u64::MAX as i128),
            CType::Int
            | CType::Float
            | CType::Bool
            | CType::Str
            | CType::Ptr
            | CType::Struct { .. }
            | CType::OwnedStr
            | CType::OptStr
            | CType::OptOwnedStr
            | CType::Float32
            | CType::Callback { .. } => return None,
        })
    }

    /// Whether the Chezzi int `v` fits this integer width. A `constant` is checked against the full
    /// range. At run time `UInt64` accepts any int as its bit pattern (owner rule, 2026-10-06: a
    /// uint64 above `i64::MAX` is a negative Chezzi int). A non-width type accepts everything.
    pub fn fits_int(&self, v: i64, constant: bool) -> bool {
        if *self == CType::UInt64 && !constant {
            return true;
        }
        match self.int_range() {
            Some((lo, hi)) => (lo..=hi).contains(&(v as i128)),
            None => true,
        }
    }

    /// Whether the Chezzi float `v` fits this width: for `Float32` a finite value whose f32 rounding
    /// overflows does not fit (Go's constant rule); NaN and inf pass, as C allows. Every other type
    /// accepts everything.
    pub fn fits_f64(&self, v: f64) -> bool {
        *self != CType::Float32 || !(v.is_finite() && (v as f32).is_infinite())
    }

    /// The printed range of a width, e.g. `(-128..127)`; float32 prints `±f32::MAX` in `{:e}` form.
    pub fn range_text(&self) -> String {
        match self.int_range() {
            Some((lo, hi)) => format!("({lo}..{hi})"),
            None => {
                let m = f32::MAX as f64;
                format!("({:e}..{:e})", -m, m)
            }
        }
    }

    /// Wrap the Chezzi int `v` to this integer width the way a C cast does (Go `int8(v)`, Rust
    /// `v as i8`). `Int64`/`UInt64` (and non-widths) return `v` unchanged: an i64 holds their bits.
    pub fn wrap_int(&self, v: i64) -> i64 {
        match self {
            CType::Int8 => v as i8 as i64,
            CType::Int16 => v as i16 as i64,
            CType::Int32 => v as i32 as i64,
            CType::UInt8 => v as u8 as i64,
            CType::UInt16 => v as u16 as i64,
            CType::UInt32 => v as u32 as i64,
            CType::Int
            | CType::Float
            | CType::Bool
            | CType::Str
            | CType::Ptr
            | CType::Struct { .. }
            | CType::OwnedStr
            | CType::OptStr
            | CType::OptOwnedStr
            | CType::Int64
            | CType::UInt64
            | CType::Float32
            | CType::Callback { .. } => v,
        }
    }

    /// Round the Chezzi float `v` to C `float` the way a C cast does (`inf` when out of range).
    pub fn wrap_f64(&self, v: f64) -> f64 {
        v as f32 as f64
    }

    /// The C-boundary range check: `Err` (a recoverable fault) when `v` does not fit this width.
    /// Every Chezzi→C site calls it: extern params, `write_field`, the callback return.
    pub fn check_fits(&self, v: &NativeRet) -> Result<(), HostError> {
        let Some(name) = self.width_name() else {
            return Ok(());
        };
        let shown = match v {
            NativeRet::Int(n) if !self.fits_int(*n, false) => n.to_string(),
            NativeRet::Float(f) if !self.fits_f64(*f) => format!("{f:e}"),
            NativeRet::Int(_)
            | NativeRet::Float(_)
            | NativeRet::Bool(_)
            | NativeRet::Str(_)
            | NativeRet::Bytes(_)
            | NativeRet::List(_)
            | NativeRet::Struct { .. }
            | NativeRet::Map(_)
            | NativeRet::Ok(_)
            | NativeRet::Err(_)
            | NativeRet::Some(_)
            | NativeRet::None
            | NativeRet::Nil
            | NativeRet::Ptr(_) => return Ok(()),
        };
        Err(HostError {
            message: format!("value {shown} does not fit {name} {}", self.range_text()),
        })
    }

    /// Whether this is a C scalar a callback param/return may use — `int`/`float`/`bool`/`ptr` and
    /// the fixed-width integers. NOT `str`/`owned_str`/opt/struct/nested `Callback` (those have no
    /// register-width scalar marshalling for the trampoline arg-read / result-write). Shared by the
    /// checker's callback-part validation and the trampoline scalar read/write.
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            CType::Int
                | CType::Float
                | CType::Bool
                | CType::Ptr
                | CType::Int8
                | CType::Int16
                | CType::Int32
                | CType::Int64
                | CType::UInt8
                | CType::UInt16
                | CType::UInt32
                | CType::UInt64
                | CType::Float32
        )
    }

    /// The C type this value is passed as through a C `...` (C's default argument promotions) --
    /// the one table both the checker (`c_vararg_ctype`) and the runtime (`vararg_ctype_of`) end in.
    /// `float` becomes `double`; `_Bool` and every integer narrower than `int` become `int`; a wider
    /// integer, `ptr` and `str` (`char*`) pass as themselves. `None`: a struct, callback or
    /// return-only string form cannot be a C vararg.
    pub fn vararg_promoted(&self) -> Option<CType> {
        match self {
            CType::Float | CType::Float32 => Some(CType::Float),
            CType::Bool
            | CType::Int8
            | CType::Int16
            | CType::UInt8
            | CType::UInt16
            | CType::Int32 => Some(CType::Int32),
            CType::UInt32 => Some(CType::UInt32),
            CType::Int | CType::Int64 | CType::UInt64 | CType::Ptr | CType::Str => {
                Some(self.clone())
            }
            CType::Struct { .. }
            | CType::Callback { .. }
            | CType::OwnedStr
            | CType::OptStr
            | CType::OptOwnedStr => None,
        }
    }

    /// The libffi argument/result [`Type`] for this type. For a `Struct`, this builds a libffi
    /// structure type from the field types (libffi computes size/alignment/offsets from the ABI).
    fn ffi_type(&self) -> Type {
        match self {
            // int ↔ C `long` (i64 on LP64 Linux); bool ↔ C `_Bool` (1 byte, the SysV `_Bool`
            // stand-in libffi-rs itself uses).
            CType::Int => Type::c_long(),
            CType::Float => Type::f64(),
            CType::Float32 => Type::f32(),
            CType::Bool => Type::u8(),
            // str → `const char*`, ptr → `void*`, and every char*-returning variant — all
            // pointers to libffi (the owned/nullable distinction is a Chezzi-side lowering choice,
            // not an ABI one: the C signature is the same `char*`).
            CType::Str | CType::Ptr | CType::OwnedStr | CType::OptStr | CType::OptOwnedStr => {
                Type::pointer()
            }
            // Fixed-width C integers — the platform-exact libffi constructors (NOT c_short/c_int/
            // c_long, whose widths shift across LP64/LLP64). int64/uint64 use i64()/u64(), distinct
            // from `CType::Int`'s c_long().
            CType::Int8 => Type::i8(),
            CType::Int16 => Type::i16(),
            CType::Int32 => Type::i32(),
            CType::Int64 => Type::i64(),
            CType::UInt8 => Type::u8(),
            CType::UInt16 => Type::u16(),
            CType::UInt32 => Type::u32(),
            CType::UInt64 => Type::u64(),
            // A flat struct: a libffi structure type over the field ffi_types (rebuilt per call).
            CType::Struct { fields, .. } => Type::structure(fields.iter().map(|f| f.ffi_type())),
            // A callback param is a C function pointer — ABI-identical to `void*`.
            CType::Callback { .. } => Type::pointer(),
        }
    }
}

/// The C type a `std.ffi` width name (a `native type` decl of `std/ffi.chz`) marshals as -- the one owner of
/// that map. `None` for any other name.
pub fn width_ctype(name: &str) -> Option<CType> {
    WIDTHS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| c.clone())
}

/// The name ↔ C type rows of the `std.ffi` widths, read by [`width_ctype`] and [`CType::width_name`].
const WIDTHS: [(&str, CType); 9] = [
    ("int8", CType::Int8),
    ("int16", CType::Int16),
    ("int32", CType::Int32),
    ("int64", CType::Int64),
    ("uint8", CType::UInt8),
    ("uint16", CType::UInt16),
    ("uint32", CType::UInt32),
    ("uint64", CType::UInt64),
    ("float32", CType::Float32),
];

/// Build the libffi structure layout for a flat-scalar struct: the structure [`Type`], its total
/// size and alignment, and each field's byte offset — all computed by libffi from the platform ABI
/// (never hand-rolled padding). `ffi_get_struct_offsets` populates the offsets AND back-fills the
/// structure type's `size`/`alignment`, so the returned `Type` is safe to hand to `ffi_prep_cif`.
///
/// Returns `(Type, size, alignment, offsets)`. The caller must keep the returned `Type` alive across
/// any `ffi_call` that uses it (libffi reads through it). Only ever called with scalar leaf fields
/// (the checker rejects nested structs), so the layout is always well-defined.
fn struct_layout(fields: &[CType]) -> (Type, usize, usize, Vec<usize>) {
    let ty = Type::structure(fields.iter().map(|f| f.ffi_type()));
    let mut offsets = vec![0usize; fields.len()];
    // SAFETY: `ty` is a freshly-built libffi structure type with `fields.len()` members; `offsets`
    // has exactly that many slots. `ffi_get_struct_offsets` reads the member types and writes one
    // offset per member (and back-fills `ty`'s size/alignment). We pass libffi's own pointers.
    let status = unsafe {
        libffi::raw::ffi_get_struct_offsets(
            libffi::raw::ffi_abi_FFI_DEFAULT_ABI,
            ty.as_raw_ptr(),
            offsets.as_mut_ptr(),
        )
    };
    debug_assert_eq!(
        status,
        libffi::raw::ffi_status_FFI_OK,
        "ffi_get_struct_offsets failed"
    );
    // SAFETY: after a successful `ffi_get_struct_offsets`, the structure type's `size`/`alignment`
    // are populated; read them back through the raw pointer libffi just wrote.
    let (size, align) = unsafe {
        let raw = &*(ty.as_raw_ptr() as *const libffi::raw::ffi_type);
        (raw.size, raw.alignment as usize)
    };
    (ty, size, align, offsets)
}

/// Write one scalar field into a C-struct byte buffer at `offset`, casting the engine-neutral
/// [`NativeRet`] to the field's C width — reusing the SAME scalar marshalling rules as a top-level
/// arg (`int`→C `long`, the fixed widths truncate/wrap via `as`, `float`→C `double`, `bool`→C `_Bool`
/// 1 byte, `ptr`→C `void*`). Errors on a non-scalar field value (a checker-prevented case, guarded
/// defensively). `Str`/owned/opt/nested `Struct` fields are rejected by the checker, so they are
/// not reachable here.
pub(crate) fn write_field(
    buf: &mut [u8],
    offset: usize,
    ct: &CType,
    v: &NativeRet,
) -> Result<(), HostError> {
    // TICKET-218 — a width field written for C must fit (struct params, `ffi.store_<w>`).
    ct.check_fits(v)?;
    let want_int = |v: &NativeRet| match v {
        NativeRet::Int(n) => Ok(*n),
        other @ NativeRet::Float(_)
        | other @ NativeRet::Bool(_)
        | other @ NativeRet::Str(_)
        | other @ NativeRet::Bytes(_)
        | other @ NativeRet::List(_)
        | other @ NativeRet::Struct { .. }
        | other @ NativeRet::Map(_)
        | other @ NativeRet::Ok(_)
        | other @ NativeRet::Err(_)
        | other @ NativeRet::Some(_)
        | other @ NativeRet::None
        | other @ NativeRet::Nil
        | other @ NativeRet::Ptr(_) => Err(HostError {
            message: format!("struct field marshal: expected int, got {other:?}"),
        }),
    };
    macro_rules! put {
        ($val:expr) => {{
            let bytes = $val.to_ne_bytes();
            buf[offset..offset + bytes.len()].copy_from_slice(&bytes);
        }};
    }
    match ct {
        CType::Int => put!((want_int(v)? as std::os::raw::c_long)),
        CType::Int8 => put!((want_int(v)? as i8)),
        CType::Int16 => put!((want_int(v)? as i16)),
        CType::Int32 => put!((want_int(v)? as i32)),
        CType::Int64 => put!(want_int(v)?),
        CType::UInt8 => put!((want_int(v)? as u8)),
        CType::UInt16 => put!((want_int(v)? as u16)),
        CType::UInt32 => put!((want_int(v)? as u32)),
        CType::UInt64 => put!((want_int(v)? as u64)),
        CType::Float => {
            let f = match v {
                NativeRet::Float(f) => *f,
                NativeRet::Int(n) => *n as f64,
                other @ NativeRet::Bool(_)
                | other @ NativeRet::Str(_)
                | other @ NativeRet::Bytes(_)
                | other @ NativeRet::List(_)
                | other @ NativeRet::Struct { .. }
                | other @ NativeRet::Map(_)
                | other @ NativeRet::Ok(_)
                | other @ NativeRet::Err(_)
                | other @ NativeRet::Some(_)
                | other @ NativeRet::None
                | other @ NativeRet::Nil
                | other @ NativeRet::Ptr(_) => {
                    return Err(HostError {
                        message: format!("struct field marshal: expected float, got {other:?}"),
                    });
                }
            };
            put!(f);
        }
        CType::Float32 => match v {
            NativeRet::Float(f) => put!((*f as f32)),
            other @ NativeRet::Int(_)
            | other @ NativeRet::Bool(_)
            | other @ NativeRet::Str(_)
            | other @ NativeRet::Bytes(_)
            | other @ NativeRet::List(_)
            | other @ NativeRet::Struct { .. }
            | other @ NativeRet::Map(_)
            | other @ NativeRet::Ok(_)
            | other @ NativeRet::Err(_)
            | other @ NativeRet::Some(_)
            | other @ NativeRet::None
            | other @ NativeRet::Nil
            | other @ NativeRet::Ptr(_) => {
                return Err(HostError {
                    message: format!("struct field marshal: expected float, got {other:?}"),
                });
            }
        },
        CType::Bool => {
            let b = match v {
                NativeRet::Bool(b) => *b,
                other @ NativeRet::Int(_)
                | other @ NativeRet::Float(_)
                | other @ NativeRet::Str(_)
                | other @ NativeRet::Bytes(_)
                | other @ NativeRet::List(_)
                | other @ NativeRet::Struct { .. }
                | other @ NativeRet::Map(_)
                | other @ NativeRet::Ok(_)
                | other @ NativeRet::Err(_)
                | other @ NativeRet::Some(_)
                | other @ NativeRet::None
                | other @ NativeRet::Nil
                | other @ NativeRet::Ptr(_) => {
                    return Err(HostError {
                        message: format!("struct field marshal: expected bool, got {other:?}"),
                    });
                }
            };
            // C `_Bool` is one byte (0/1).
            put!((if b { 1u8 } else { 0u8 }));
        }
        CType::Ptr => {
            let a = match v {
                NativeRet::Ptr(a) => *a,
                other @ NativeRet::Int(_)
                | other @ NativeRet::Float(_)
                | other @ NativeRet::Bool(_)
                | other @ NativeRet::Str(_)
                | other @ NativeRet::Bytes(_)
                | other @ NativeRet::List(_)
                | other @ NativeRet::Struct { .. }
                | other @ NativeRet::Map(_)
                | other @ NativeRet::Ok(_)
                | other @ NativeRet::Err(_)
                | other @ NativeRet::Some(_)
                | other @ NativeRet::None
                | other @ NativeRet::Nil => {
                    return Err(HostError {
                        message: format!("struct field marshal: expected ptr, got {other:?}"),
                    });
                }
            };
            put!(a);
        }
        // A struct (a by-value param, or a nested field): a `NativeRet::List` of its fields in
        // declaration order, each written at `offset + inner offset` from libffi.
        CType::Struct { name, fields, .. } => {
            let NativeRet::List(items) = v else {
                return Err(HostError {
                    message: format!("struct field marshal: expected struct '{name}', got {v:?}"),
                });
            };
            if items.len() != fields.len() {
                return Err(HostError {
                    message: format!(
                        "struct '{name}' has {} field(s), expected {}",
                        items.len(),
                        fields.len()
                    ),
                });
            }
            let (_ty, _size, _align, offsets) = struct_layout(fields);
            for ((ct, off), item) in fields.iter().zip(offsets.iter()).zip(items.iter()) {
                write_field(buf, offset + off, ct, item)?;
            }
        }
        // The checker rejects str / owned / opt / callback fields, so they never reach here.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Callback { .. } => {
            return Err(HostError {
                message: "struct field marshal: str fields are not supported".into(),
            });
        }
    }
    Ok(())
}

/// Read one scalar field back out of a C-struct byte buffer at `offset`, widening to an engine-
/// neutral [`NativeRet`] — the mirror of [`write_field`]. Sub-word fields read their exact stored
/// width (NOT the register width — a struct member is at its real offset, unlike a narrow *return*
/// which libffi widens), then sign-/zero-extend back to i64. Only scalar leaf variants are reachable.
pub(crate) fn read_field(buf: &[u8], offset: usize, ct: &CType) -> NativeRet {
    macro_rules! get {
        ($ty:ty) => {{
            const N: usize = std::mem::size_of::<$ty>();
            let mut b = [0u8; N];
            b.copy_from_slice(&buf[offset..offset + N]);
            <$ty>::from_ne_bytes(b)
        }};
    }
    match ct {
        CType::Int => {
            let n: std::os::raw::c_long = get!(std::os::raw::c_long);
            NativeRet::Int(n as i64)
        }
        CType::Int8 => NativeRet::Int(get!(i8) as i64),
        CType::Int16 => NativeRet::Int(get!(i16) as i64),
        CType::Int32 => NativeRet::Int(get!(i32) as i64),
        CType::Int64 => NativeRet::Int(get!(i64)),
        CType::UInt8 => NativeRet::Int(get!(u8) as i64),
        CType::UInt16 => NativeRet::Int(get!(u16) as i64),
        CType::UInt32 => NativeRet::Int(get!(u32) as i64),
        // u64 -> i64 reinterprets the top bit (a value > i64::MAX wraps negative — documented v1 limit).
        CType::UInt64 => NativeRet::Int(get!(u64) as i64),
        CType::Float => NativeRet::Float(get!(f64)),
        CType::Float32 => NativeRet::Float(get!(f32) as f64),
        CType::Bool => {
            // C `_Bool` is one byte at its real offset; any nonzero byte is true.
            let c: u8 = get!(u8);
            NativeRet::Bool(c != 0)
        }
        CType::Ptr => NativeRet::Ptr(get!(usize)),
        // A struct (a by-value return, or a nested field): each field read at `offset + inner
        // offset` from libffi, as the `NativeRet::Struct` the VM lowers to a native struct.
        CType::Struct {
            name,
            field_names,
            fields,
        } => {
            let (_ty, _size, _align, offsets) = struct_layout(fields);
            NativeRet::Struct {
                name: name.clone(),
                fields: field_names
                    .iter()
                    .zip(fields.iter())
                    .zip(offsets.iter())
                    .map(|((fname, ct), off)| (fname.clone(), read_field(buf, offset + off, ct)))
                    .collect(),
            }
        }
        // Unreachable for a well-typed struct (checker rejects str/owned/opt/callback); Nil.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Callback { .. } => NativeRet::Nil,
    }
}

/// Read one INCOMING C scalar callback argument out of libffi's `*mut c_void` avalue slot and widen
/// it to an engine-neutral [`NativeRet`] — the reverse of the top-level return-narrowing rules. A
/// libffi closure passes each arg as a pointer to its natural-width C value (NOT register-widened
/// like a *return*), so each width reads its exact stored type then sign-/zero-extends back to i64.
/// Only the [`CType::is_scalar`] variants are reachable (the checker rejects non-scalar parts).
///
/// # Safety
/// `slot` must be a valid, aligned pointer to a C value of width matching `ct` (libffi's avalue slot
/// for that arg position). Only called from the trampoline with libffi's own avalue array.
unsafe fn read_c_arg(slot: *const c_void, ct: &CType) -> NativeRet {
    macro_rules! rd {
        ($ty:ty) => {{
            // SAFETY: `slot` points at a C value of this exact width (caller contract).
            unsafe { *(slot as *const $ty) }
        }};
    }
    match ct {
        CType::Int => {
            let n: std::os::raw::c_long = rd!(std::os::raw::c_long);
            NativeRet::Int(n as i64)
        }
        CType::Int8 => NativeRet::Int(rd!(i8) as i64),
        CType::Int16 => NativeRet::Int(rd!(i16) as i64),
        CType::Int32 => NativeRet::Int(rd!(i32) as i64),
        CType::Int64 => NativeRet::Int(rd!(i64)),
        CType::UInt8 => NativeRet::Int(rd!(u8) as i64),
        CType::UInt16 => NativeRet::Int(rd!(u16) as i64),
        CType::UInt32 => NativeRet::Int(rd!(u32) as i64),
        // u64 -> i64 reinterprets the top bit (documented v1 limit, same as the return path).
        CType::UInt64 => NativeRet::Int(rd!(u64) as i64),
        CType::Float => NativeRet::Float(rd!(f64)),
        CType::Float32 => NativeRet::Float(rd!(f32) as f64),
        CType::Bool => NativeRet::Bool(rd!(u8) != 0),
        CType::Ptr => NativeRet::Ptr(rd!(usize)),
        // Non-scalar parts are checker-rejected; default defensively to Nil.
        CType::Str
        | CType::Struct { .. }
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Callback { .. } => NativeRet::Nil,
    }
}

/// Write the callback's [`NativeRet`] RESULT into libffi's `*mut c_void` result slot, casting to the
/// declared return C width. CRITICAL: libffi's closure result buffer is register-width
/// (`sizeof(ffi_arg)`) for any sub-register integral return — so a sub-word integral result is
/// written as a full register word (the same rule the top-level narrow-*return* read follows in
/// reverse), never a narrow store that would leave the upper bytes of the register undefined. `float`
/// and `ptr` write their natural width. Only the [`CType::is_scalar`] variants are reachable.
///
/// # Safety
/// `slot` must be a valid pointer to a result buffer at least `sizeof(ffi_arg)` bytes (libffi's
/// closure rvalue contract), aligned for a register word. Only called from the trampoline.
unsafe fn write_c_result(slot: *mut c_void, ct: &CType, v: &NativeRet) {
    let as_int = |v: &NativeRet| match v {
        NativeRet::Int(n) => *n,
        NativeRet::Bool(b) => i64::from(*b),
        NativeRet::Float(_)
        | NativeRet::Str(_)
        | NativeRet::Bytes(_)
        | NativeRet::List(_)
        | NativeRet::Struct { .. }
        | NativeRet::Map(_)
        | NativeRet::Ok(_)
        | NativeRet::Err(_)
        | NativeRet::Some(_)
        | NativeRet::None
        | NativeRet::Nil
        | NativeRet::Ptr(_) => 0,
    };
    // SAFETY (all arms): `slot` is libffi's result buffer, >= register width and aligned (caller
    // contract). Sub-register integral returns widen to a full `ffi_arg`/`ffi_sarg` register word.
    unsafe {
        match ct {
            // Signed integral returns (`int`/the fixed signed widths) widen to the signed register
            // word (`ffi_sarg`). A sub-register width still occupies a full register in the result.
            CType::Int | CType::Int8 | CType::Int16 | CType::Int32 | CType::Int64 => {
                *(slot as *mut libffi::raw::ffi_sarg) = as_int(v) as libffi::raw::ffi_sarg;
            }
            // Unsigned integral returns widen to the unsigned register word (`ffi_arg`).
            CType::UInt8 | CType::UInt16 | CType::UInt32 | CType::UInt64 => {
                *(slot as *mut libffi::raw::ffi_arg) = as_int(v) as libffi::raw::ffi_arg;
            }
            CType::Bool => *(slot as *mut libffi::raw::ffi_arg) = as_int(v) as libffi::raw::ffi_arg,
            CType::Float => {
                let f = match v {
                    NativeRet::Float(f) => *f,
                    NativeRet::Int(n) => *n as f64,
                    NativeRet::Bool(_)
                    | NativeRet::Str(_)
                    | NativeRet::Bytes(_)
                    | NativeRet::List(_)
                    | NativeRet::Struct { .. }
                    | NativeRet::Map(_)
                    | NativeRet::Ok(_)
                    | NativeRet::Err(_)
                    | NativeRet::Some(_)
                    | NativeRet::None
                    | NativeRet::Nil
                    | NativeRet::Ptr(_) => 0.0,
                };
                *(slot as *mut f64) = f;
            }
            CType::Float32 => {
                let f = match v {
                    NativeRet::Float(f) => *f,
                    NativeRet::Int(n) => *n as f64,
                    NativeRet::Bool(_)
                    | NativeRet::Str(_)
                    | NativeRet::Bytes(_)
                    | NativeRet::List(_)
                    | NativeRet::Struct { .. }
                    | NativeRet::Map(_)
                    | NativeRet::Ok(_)
                    | NativeRet::Err(_)
                    | NativeRet::Some(_)
                    | NativeRet::None
                    | NativeRet::Nil
                    | NativeRet::Ptr(_) => 0.0,
                };
                *(slot as *mut f32) = f as f32;
            }
            CType::Ptr => {
                let a = match v {
                    NativeRet::Ptr(a) => *a,
                    NativeRet::Int(n) => *n as usize,
                    NativeRet::Float(_)
                    | NativeRet::Bool(_)
                    | NativeRet::Str(_)
                    | NativeRet::Bytes(_)
                    | NativeRet::List(_)
                    | NativeRet::Struct { .. }
                    | NativeRet::Map(_)
                    | NativeRet::Ok(_)
                    | NativeRet::Err(_)
                    | NativeRet::Some(_)
                    | NativeRet::None
                    | NativeRet::Nil => 0,
                };
                *(slot as *mut *mut c_void) = a as *mut c_void;
            }
            // Non-scalar returns are checker-rejected; zero the register word defensively.
            CType::Str
            | CType::Struct { .. }
            | CType::OwnedStr
            | CType::OptStr
            | CType::OptOwnedStr
            | CType::Callback { .. } => *(slot as *mut libffi::raw::ffi_arg) = 0,
        }
    }
}

/// The userdata a callback trampoline closes over: a raw `*mut dyn Host` (a fat pointer — sound ONLY
/// because the trampoline fires synchronously inside the same `ffi_call` on the same thread while the
/// `&mut dyn Host` is live one Rust frame up), the extern arg index of the closure, the callback's
/// param/return signature (borrowed from the live `Cffi`), and a fault out-slot the trampoline stashes
/// a host error / caught panic into for [`Cffi::call`] to re-raise. NOT stored, never sent across a
/// thread: built on the `call` stack, DISARMED (`armed = false`) and leaked when `call` returns — see
/// [`CallbackClosure`]'s `Drop`.
struct TrampolineCtx<'h> {
    /// The ARMED flag, and the ONLY field ever written after C can see the code pointer. `Cffi::call`
    /// sets it (`Release`) as its last act before `ffi_call`; `CallbackClosure::drop` clears it
    /// (`Release`) once the call returns, which POISONS the leaked trampoline.
    ///
    /// ATOMIC, not a plain `bool`: a C library that STORED the pointer can invoke the trampoline from
    /// ANOTHER thread (`signal`+`alarm` delivers to an arbitrary thread; GLib/libuv/ALSA call back on
    /// their own), so that load races this store. A plain read/write pair there is a data race — UB
    /// in the abstract machine no matter what the hardware does. The `Release`/`Acquire` pairing also
    /// publishes the `host`/`params`/`ret`/`fault` writes below to whatever thread does fire.
    armed: std::sync::atomic::AtomicBool,
    /// The thread that built this ctx — i.e. the one that will make the `ffi_call`. Written once at
    /// construction, BEFORE the code pointer exists in C, and never mutated: no race to read it.
    ///
    /// An atomic `armed` still cannot stop a foreign thread from observing a STALE `true` in the
    /// window around the poison store, so this is the race-free half of the guard: the callback
    /// contract is same-thread-during-the-call (see [`CType::Callback`]), so an invocation on any
    /// other thread aborts unconditionally rather than dereferencing `host`.
    owner: libc::pthread_t,
    // Filled in AFTER the whole arg-reading loop finishes (see `Cffi::call`): deriving this raw
    // pointer from the `&mut dyn Host` param mid-loop would be invalidated by the later `host.arg_*`
    // reborrows for trailing params (Stacked/Tree Borrows), so we capture it as the final use of
    // `host`. Written once, immediately before `armed` is set; never written again (poisoning clears
    // `armed` instead — a plain write here would race the trampoline's read).
    host: Option<*mut (dyn Host + 'h)>,
    arg_index: usize,
    params: *const [CType],
    /// `None` means the callback's return is VOID — the trampoline must not write into libffi's
    /// result buffer at all (see [`CType::Callback`]'s `ret` doc).
    ret: Option<*const CType>,
    fault: *mut Option<HostError>,
}

/// The message a poisoned (stored) callback aborts with. Kept as a `const` so the integration test
/// `tests/ffi_stored_callback.rs` and the docs quote one string.
const POISON_MSG: &[u8] = b"chezzi FFI: callback invoked after the extern call that received it \
returned; stored/cross-thread callbacks are not supported\n";

/// Same, for a callback invoked on a thread other than the one that made the extern call. Shares the
/// `stored/cross-thread callbacks are not supported` tail so one substring covers both.
const CROSS_THREAD_MSG: &[u8] =
    b"chezzi FFI: callback invoked from a thread other than the one that \
made the extern call; stored/cross-thread callbacks are not supported\n";

/// Marshalled `str`-arg C buffers a `ptr` may still point into after their `Cffi::call` returned
/// (TICKET-060). Chezzi has no lifetime for a `ptr` (it is never auto-freed), so there is no owner to
/// attach the buffer to — retention means LEAKING it on purpose, for as long as the process runs.
/// Keyed by the NUL-terminated byte content rather than per-call: a loop marshalling the same string
/// retains ONE buffer, not one per call. The value is a `usize` (the buffer's address as an integer,
/// not a pointer) so the table stays `Send`.
static RETAINED_STR_ARGS: OnceLock<Mutex<HashMap<Vec<u8>, usize>>> = OnceLock::new();

fn retained_table() -> &'static Mutex<HashMap<Vec<u8>, usize>> {
    RETAINED_STR_ARGS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up a previously retained buffer by its NUL-terminated content. A C fn that WRITES through a
/// `str` param (`strtok`) mutates the shared buffer in place, but Chezzi `str` is immutable, so a
/// reuse that finds the buffer mutated is restored to `with_nul` here, under the table lock.
fn retained_lookup(with_nul: &[u8]) -> Option<*const std::os::raw::c_char> {
    let table = retained_table().lock().unwrap_or_else(|e| e.into_inner());
    let addr = *table.get(with_nul)?;
    // SAFETY: `addr` was produced by `retain_cstring` from a `CString::into_raw()` that is never
    // `from_raw`'d, so the allocation is live for the process's lifetime; `with_nul.len()` matches
    // the length recorded when it was inserted (the map key IS `with_nul`'s bytes).
    unsafe {
        let cur = std::slice::from_raw_parts(addr as *const u8, with_nul.len());
        if cur != with_nul {
            std::ptr::copy_nonoverlapping(with_nul.as_ptr(), addr as *mut u8, with_nul.len());
        }
    }
    Some(addr as *const std::os::raw::c_char)
}

/// Retain a marshalled `str` arg's `CString` forever, keyed by its content. Leaks the allocation `cs`
/// already owns — the pointer C may still hold into it — on purpose; it is never `from_raw`'d back.
fn retain_cstring(cs: CString) -> *const std::os::raw::c_char {
    let key = cs.as_bytes_with_nul().to_vec();
    let p = cs.into_raw();
    // An already-occupied entry means another call already retained this same content; this extra
    // buffer is simply leaked, untracked (still a correct pointer, just not the one future lookups
    // will hit).
    retained_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_insert(p as usize);
    p as *const std::os::raw::c_char
}

/// Collect every `ptr` address reachable in a [`NativeRet`] result: a bare [`NativeRet::Ptr`], or one
/// nested inside `Ok`/`Some`/a by-value struct's fields (a `ptr` also reaches the program as a struct
/// FIELD via `read_field`'s `CType::Ptr` arm).
fn collect_ptr_addrs(r: &NativeRet, out: &mut Vec<usize>) {
    match r {
        NativeRet::Ptr(a) => out.push(*a),
        NativeRet::Ok(b) | NativeRet::Some(b) => collect_ptr_addrs(b, out),
        NativeRet::Struct { fields, .. } => {
            for (_, v) in fields {
                collect_ptr_addrs(v, out);
            }
        }
        NativeRet::Int(_)
        | NativeRet::Float(_)
        | NativeRet::Bool(_)
        | NativeRet::Str(_)
        | NativeRet::Bytes(_)
        | NativeRet::List(_)
        | NativeRet::Map(_)
        | NativeRet::Err(_)
        | NativeRet::None
        | NativeRet::Nil => {}
    }
}

/// Retrospectively retain any `str`-arg `CString` a returned `ptr` aliases into. A `ptr` RETURN is
/// judged after the fact (not eagerly, like a `ptr` PARAM): only an address that actually falls
/// inside one of this call's marshalled buffers retains that buffer, so a `ptr`-plus-`str` shape with
/// no aliasing (`fopen(path: str, mode: str) -> ptr`) retains nothing.
fn retain_aliasing_str_args(ret: &NativeRet, cstrings: &mut [Option<CString>]) {
    let mut addrs = Vec::new();
    collect_ptr_addrs(ret, &mut addrs);
    if addrs.is_empty() {
        return;
    }
    for slot in cstrings.iter_mut() {
        let Some(cs) = slot else { continue };
        let base = cs.as_ptr() as usize;
        // The NUL terminator itself is a legal target (`strchr(s, 0)`); one byte past it is not.
        let end = base + cs.as_bytes_with_nul().len();
        if addrs.iter().any(|&a| a >= base && a < end) {
            let cs = slot.take().expect("checked Some above");
            retain_cstring(cs);
        }
    }
}

/// `write(2)` the WHOLE buffer, retrying a short count, `EINTR` and `EAGAIN`. Async-signal-safe (no
/// allocation, no lock, no Rust stdio). A single bare `write` loses the message outright on a
/// non-blocking fd 2 (an inherited-`O_NONBLOCK` tty, a CI harness) or on any signal arriving
/// mid-syscall — and the message is the entire value of the abort path.
///
/// The two retryable errnos get SEPARATE budgets, because only one of them sleeps. `EAGAIN` backs
/// off 1 ms per spin, so 2000 spins really is the ~2 s cap it claims. `EINTR` does NOT sleep (the
/// fd is writable; we were merely interrupted), so a shared counter would let a repeating signal —
/// `setitimer`/`SIGPROF`, or the `signal`+`alarm` shape this abort path exists for — burn the whole
/// budget in microseconds of pure syscall churn and return with NOTHING written, leaving a bare
/// SIGABRT and an empty stderr: precisely the failure this retry loop was added to prevent. An
/// `EINTR` that keeps recurring is also cheap to keep retrying, so it gets its own, larger budget.
///
/// ponytail: the `EAGAIN` back-off is a 1 ms sleep instead of `poll(POLLOUT)`. If a stuck reader
/// ever needs to block us for longer, swap the sleep for a `poll`.
fn write_all_fd(fd: i32, buf: &[u8]) {
    /// ~2 s at 1 ms per spin.
    const MAX_AGAIN: u32 = 2000;
    /// No sleep on this path, so the bound is on syscall churn, not wall-clock.
    const MAX_INTR: u32 = 100_000;
    let mut off = 0usize;
    let mut again_spins = 0u32;
    let mut intr_spins = 0u32;
    while off < buf.len() {
        // SAFETY: writing `buf.len() - off` bytes from within `buf`'s own allocation to a raw fd.
        let n = unsafe { libc::write(fd, buf[off..].as_ptr() as *const c_void, buf.len() - off) };
        if n > 0 {
            off += n as usize;
            continue;
        }
        if n == 0 {
            return; // cannot happen for a non-empty buffer; treat as a dead fd rather than spin
        }
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if e != libc::EINTR && e != libc::EAGAIN && e != libc::EWOULDBLOCK {
            return; // EPIPE / EBADF / ENOSPC: unrecoverable, the abort below is all that's left
        }
        if e == libc::EINTR {
            intr_spins += 1;
            if intr_spins > MAX_INTR {
                return;
            }
            continue; // writable, just interrupted — retry immediately, do NOT spend the sleep budget
        }
        again_spins += 1;
        if again_spins > MAX_AGAIN {
            return;
        }
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };
        // SAFETY: async-signal-safe sleep on a stack timespec; no out-param.
        unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
    }
}

/// A trampoline that must not re-enter the VM — either its `Cffi::call` already returned (the VM
/// back-pointer is stale) or it fired on a foreign thread. Report and `abort()`.
///
/// `abort` rather than a panic/fault: we are on a C stack (the realistic site is a C signal handler
/// — the W6-8 repro is `signal`/`raise`), unwinding from Rust into a C frame is itself UB, and a
/// panic here would be swallowed by the surrounding `catch_unwind`, whose handler would then write a
/// `HostError` through `ctx.fault` — which points at `Cffi::call`'s `Box<Option<HostError>>`, a HEAP
/// allocation freed when that call returned, so the write lands in freed heap (a use-after-free, not
/// a stack scribble). A second and quieter UB. Raw `write(2)` rather than `eprintln!` because Rust's
/// stdio lock is not async-signal-safe.
///
/// The program's own QUEUED stdout is deliberately DISCARDED here — this path calls nothing but
/// `write(2)` and `abort()`, both async-signal-safe. Draining the streamed sink first (an earlier
/// cut of this fix called `vm::stream::flush_stream`) is not merely unsafe-in-principle, it HANGS:
/// `flush_stream` pushes `Msg::Flush(ack)` on an `mpsc` and blocks on `rx.recv()` with no timeout,
/// and the only thread that can service that message is the stream writer itself. Two deterministic
/// wedges follow. (1) The poisoned trampoline fires ON the writer thread — an async signal
/// (`signal(SIGALRM, h)` + `alarm`, SIGINT from the tty) is delivered to any thread that has not
/// blocked it, and `std::thread::spawn`ed writers inherit an unblocked mask — so it sends a Flush
/// into the queue it is itself the sole consumer of and waits on itself, forever. (2) The writer is
/// parked in `write_all` on a full 64 kB stdout pipe whose reader never drains (`chezzi run p.chz |
/// (sleep 60; cat)`), so the Flush queues behind the stuck write. Either way: no SIGABRT, no exit,
/// no core — strictly worse than the SIGSEGV this whole change exists to replace. `flush_stream`'s
/// own contract says as much (`src/vm/stream.rs`: "Called by `main` AFTER the VM has finished (never
/// from a fiber)"); a C signal handler is further outside that precondition than a fiber is.
/// Separately, its `mpsc::channel()` + `send` both allocate, and glibc `malloc` is not
/// async-signal-safe — re-entering the allocator from a handler that interrupted it self-deadlocks
/// on the arena lock. Losing buffered stdout on a crash is what every other runtime does too
/// (CPython loses it on SIGSEGV/`abort`); the diagnostic itself is never lost, because it goes
/// straight to fd 2 and never touches the queue.
fn callback_poison_abort(msg: &[u8]) -> ! {
    write_all_fd(2, msg);
    std::process::abort()
}

/// The libffi closure handler (one shared `extern "C"` fn; the per-callback distinction is the
/// userdata). C calls this with the CIF, a result buffer, the avalue array, and our `TrampolineCtx*`.
/// It reads the C scalar args into [`NativeRet`]s, re-enters the engine via [`Host::invoke_callback`]
/// (keyed by arg index, so no engine `Value` leaks here), and writes the result back. The ENTIRE body
/// is wrapped in `catch_unwind`: a Chezzi fault or a Rust panic must NOT unwind into the C frames —
/// on either, a zeroed result is written (so C unwinds with a defined value) and the error is stashed
/// in `ctx.fault` for `Cffi::call` to re-raise (stronger than ctypes, which swallows to stderr + 0).
///
/// # Safety
/// Invoked only by libffi for a closure prepped with a CIF matching `ctx.params -> ctx.ret` and a
/// `TrampolineCtx*` userdata. `args` is the avalue array (one slot per param), `result` the rvalue
/// buffer (>= register width), `userdata` the `TrampolineCtx*`.
unsafe extern "C" fn callback_trampoline(
    _cif: *mut libffi::raw::ffi_cif,
    result: *mut c_void,
    args: *mut *mut c_void,
    userdata: *mut c_void,
) {
    // SAFETY: `userdata` is the `TrampolineCtx*` we passed to `ffi_prep_closure_loc`, live for the
    // whole `ffi_call` (it sits on `Cffi::call`'s stack frame, one Rust frame up). The lifetime is
    // erased at runtime — reading it through any concrete `'h` is sound because the trampoline only
    // uses `ctx.host` synchronously within this call, while the original `&mut dyn Host` is live.
    let ctx = unsafe { &*(userdata as *const TrampolineCtx<'_>) };
    // POISON GUARD — FIRST, before every other field is touched. Once `Cffi::call` returned, its
    // `CallbackClosure::drop` cleared `armed` and leaked this ctx; `host` is stale, `params`/`ret`
    // point into a possibly-freed `Cffi` and `fault` into a freed heap slot, so reading ANY of them
    // (or entering `catch_unwind`, whose error path writes through `fault`) would be UB. This is the
    // whole W6-8 fix: a stored callback aborts loudly instead of executing freed memory.
    //
    // TWO checks, because one cannot cover both threads:
    //  * `armed` (Acquire, pairing with both `Release` stores) is exact on the calling thread —
    //    program order means a post-return invocation ALWAYS sees `false`. Off-thread it is merely
    //    race-FREE: a foreign reader may still observe a stale `true` around the poison store.
    //  * the owner-thread check closes exactly that hole, and needs no synchronisation of its own
    //    (`owner` is write-once, before C ever sees the code pointer). The callback contract is
    //    same-thread-during-the-call, so any other thread is unsupported by construction — that also
    //    covers a C library that hands the pointer to its own worker thread WHILE the call runs.
    if !ctx.armed.load(std::sync::atomic::Ordering::Acquire) {
        callback_poison_abort(POISON_MSG);
    }
    // SAFETY: `pthread_self`/`pthread_equal` are async-signal-safe and read no shared state.
    if unsafe { libc::pthread_equal(libc::pthread_self(), ctx.owner) } == 0 {
        callback_poison_abort(CROSS_THREAD_MSG);
    }
    // SAFETY: `armed` was true and we are the owning thread, so `Cffi::call` has not returned and
    // `host` (a plain `Copy` raw pointer, published by the `Release` store) is live.
    // (`abort`, not `expect`: unreachable, but a panic HERE would unwind into the C frame — the one
    // thing this whole path exists to prevent.)
    let host_ptr = match ctx.host {
        Some(p) => p,
        None => callback_poison_abort(POISON_MSG),
    };
    // SAFETY: `ctx.params` is a slice pointer into the live `Cffi`'s signature (valid for the call).
    let params: &[CType] = unsafe { &*ctx.params };
    // SAFETY: `ctx.ret` (when `Some`) points at the live callback return CType. `None` means the
    // callback's return is VOID.
    let ret_ct: Option<&CType> = ctx.ret.map(|p| unsafe { &*p });
    // Every write into libffi's result buffer for this call MUST go through this closure: a void
    // callback (`ret_ct: None`) writes NOTHING — libffi documents that buffer as garbage for a void
    // closure. One closure, one gate, for all four outcomes below (already-faulted short-circuit,
    // ok, host-error, panic).
    let write = |v: &NativeRet| {
        if let Some(ct) = ret_ct {
            // SAFETY: `result` is libffi's rvalue buffer (>= register width, aligned); `ct` is the
            // declared return width.
            unsafe { write_c_result(result, ct, v) };
        }
    };

    // TICKET-060: this extern call is already doomed once a fault is stashed -- its error is
    // re-raised when `ffi_call` returns -- but C does not know that and keeps calling (libc `qsort`
    // performs 15 comparisons for a 10-element input, not 1). No Chezzi code may run after the FIRST
    // fault, so short-circuit here with a zeroed result. C's own loop still runs regardless: Chezzi
    // cannot unwind through C frames the way Go cgo does, so only the Chezzi side of it stops.
    // SAFETY: `ctx.fault` is live per the `armed` + owner-thread guards above; only the owning thread
    // touches it.
    if unsafe { (*ctx.fault).is_some() } {
        write(&NativeRet::Int(0));
        return;
    }

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Read each incoming C scalar arg into a NativeRet.
        let mut native_args: Vec<NativeRet> = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            // SAFETY: `args` is libffi's avalue array with `params.len()` slots; slot `i` points at a
            // C value of width matching `p` (the CIF was built from these same param types).
            let slot = unsafe { *args.add(i) } as *const c_void;
            native_args.push(unsafe { read_c_arg(slot, p) });
        }
        // SAFETY: `host_ptr` (resolved by the poison guard above) is the raw pointer captured as the
        // FINAL use of the `&mut dyn Host` param (after every `host.arg_*` read), so no later
        // reborrow has invalidated it. `armed` + the owner-thread check mean `Cffi::call` has not
        // returned yet AND we are its thread, so the trampoline is firing synchronously inside that
        // same `ffi_call` while the borrow is dormant one frame up; no other alias is active.
        let host: &mut dyn Host = unsafe { &mut *host_ptr };
        // TICKET-218 — the callback's return crosses into C: a width return must fit, or it takes
        // the fault path below (zeroed result, first fault re-raised).
        host.invoke_callback(ctx.arg_index, &native_args)
            .and_then(|v| match ret_ct {
                Some(ct) => ct.check_fits(&v).map(|()| v),
                None => Ok(v),
            })
    }));

    match outcome {
        Ok(Ok(v)) => {
            write(&v);
        }
        Ok(Err(host_err)) => {
            // Zero the result so C unwinds with a defined value.
            write(&NativeRet::Int(0));
            // SAFETY: `ctx.fault` points into `Cffi::call`'s live `Box<Option<HostError>>` out-slot
            // (heap, freed when that call returns — the poison guard above is what keeps a
            // post-return invocation from writing through it).
            // TICKET-060: the FIRST fault is the one re-raised -- don't overwrite an existing one.
            unsafe {
                if (*ctx.fault).is_none() {
                    *ctx.fault = Some(host_err);
                }
            }
        }
        Err(panic_payload) => {
            // Zero the result so C unwinds with a defined value (no panic into C frames).
            write(&NativeRet::Int(0));
            let msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "callback panicked".to_string());
            // SAFETY: as above — the boxed fault out-slot is live for the duration of the call.
            // TICKET-060: the FIRST fault is the one re-raised -- don't overwrite an existing one.
            unsafe {
                if (*ctx.fault).is_none() {
                    *ctx.fault = Some(HostError {
                        message: format!("callback panicked: {msg}"),
                    });
                }
            }
        }
    }
}

/// A live libffi closure backing one callback arg: the allocated closure handle, the CIF (libffi
/// reads through it whenever the closure is invoked) and the boxed [`TrampolineCtx`] userdata.
/// Dropping either FREES all three (the trampoline was never armed, so C never saw the code pointer)
/// or POISONS and LEAKS them (it was) — see the `Drop` impl.
struct CallbackClosure<'h> {
    handle: *mut libffi::raw::ffi_closure,
    _cif: ManuallyDrop<Box<Cif>>,
    // Patched (its `host` field, then `armed`) after the arg loop, then read by libffi during the
    // call via the userdata pointer. Not `_`-prefixed because `Cffi::call` writes it; `ctx.armed` is
    // the ARMED flag the `Drop` impl reads.
    ctx: ManuallyDrop<Box<TrampolineCtx<'h>>>,
}

// NOTE: `_cif` is `ManuallyDrop<Box<Cif>>`, NOT `Cif`. `ffi_prep_closure_loc` stores a raw pointer to the `Cif`'s
// inner `ffi_cif`; libffi dereferences it when C later invokes the closure. A by-value `Cif` here
// would be MOVED (into this struct, then into the `callback_closures` Vec, which can reallocate),
// relocating the `ffi_cif` and dangling that stored pointer → `classify_argument` reads freed memory
// → SIGSEGV (manifests only under some memory layouts — it slipped past the goldens). The `Box` pins
// the `ffi_cif` at a stable heap address across every move, exactly like `ctx` above.

impl Drop for CallbackClosure<'_> {
    /// POISON, don't free (gaps.md **W6-8**). C may have STORED the code pointer (`signal`,
    /// `atexit`, GLib, `pthread_cleanup_*`) — it used to be `ffi_closure_free`d here, so the next
    /// invocation from C executed freed memory: a SIGSEGV reachable from checker-clean code. A
    /// check-time reject is impossible (the identical `fn(int) -> int` param is correct for
    /// `qsort`, which invokes DURING the call), so instead:
    ///
    /// - clear the ARMED flag (`armed = false`, `Release`) — the trampoline's first act is to load
    ///   it (`Acquire`) and `abort()`, so the stale `host` back-pointer is never dereferenced. The
    ///   flag is atomic because a stored callback can fire on another thread, which would make a
    ///   plain write/read pair a data race;
    /// - leak the `ffi_closure` allocation + `_cif` + `ctx`. ALL THREE must survive: libffi's generated trampoline
    ///   derefs the prepped `ffi_cif` to marshal args and loads the userdata pointer BEFORE our
    ///   Rust fn runs, so freeing the cif or the ctx would just relocate the SIGSEGV into
    ///   `classify_argument` (that is the 3038f67 bug again).
    ///
    /// …but ONLY for a trampoline that was actually ARMED. `Cffi::call` sets `ctx.armed` as its last
    /// act before `ffi_call`; an unset flag here means the call bailed during arg
    /// marshalling (an interior-NUL `str`, a return-only C type, a failed closure alloc for a later
    /// callback arg — all `recover:`-able), so `ffi_call` never ran and C provably never saw the code
    /// pointer. Nothing to protect: free it. Leaking those would leak per *attempt*, and a
    /// `recover:` retry loop that never calls into C would grow the pool for nothing.
    ///
    /// ponytail: an ARMED trampoline leaks one closure + cif + ctx per callback-passing extern call.
    /// That is ~400 B of RSS, but it comes out of libffi's exec pool as a W^X page PAIR, so it also
    /// consumes `vm.max_map_count` (~1 new VMA per ~130 calls). A `qsort` in a hot loop therefore
    /// grows memory AND mapping count; when the pool can no longer grow, the next callback-passing
    /// call fails with the recoverable "cannot allocate a callback trampoline" error raised at the
    /// `ffi_closure_alloc` site (never a crash — that NULL is checked precisely because this leak
    /// makes exhaustion reachable). Accepted trade for killing the UB. Upgrade path: cache and reuse
    /// one trampoline per (closure identity, signature) instead of allocating per call, and free it
    /// when the owning closure is collected.
    fn drop(&mut self) {
        // `Relaxed`: only this thread ever sets the flag, and it is the thread that armed it.
        if !self.ctx.armed.load(std::sync::atomic::Ordering::Relaxed) {
            // Never armed → `ffi_call` never ran → free everything, no leak.
            // SAFETY: `handle` came from `ffi_closure_alloc` and is freed exactly once (here); no C
            // code holds the code pointer, since the extern call never happened.
            unsafe { libffi::low::closure_free(self.handle) };
            // SAFETY: both `ManuallyDrop` fields are live and dropped exactly once (this is the only
            // `Drop` for them), after the closure that referenced them is gone.
            unsafe {
                ManuallyDrop::drop(&mut self._cif);
                ManuallyDrop::drop(&mut self.ctx);
            }
            return;
        }
        // Armed: C may have stored the code pointer. Poison (the exact inverse of `Cffi::call`'s
        // arming store) and leak. `Release` so a trampoline that loads `false` also sees every write
        // this thread made before it; `host` itself is deliberately LEFT ALONE — writing it here
        // would race a concurrent trampoline's read of the same field.
        self.ctx
            .armed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// A resolved, callable C function: the live `Library`, the symbol address, and the marshalling
/// signature. Built eagerly at module init (`dlopen` failure surfaces at startup). `Send + Sync`:
/// `Library` is `Send + Sync` on unix, the address is a plain `usize`, and the rest is owned data.
pub struct Cffi {
    /// Kept alive so the symbol address stays valid; named `_lib` because it is never read directly.
    _lib: Library,
    /// The resolved function address (libloading `Symbol` is `!Send`; a raw `usize` is `Send`).
    sym: usize,
    params: Vec<CType>,
    ret: Option<CType>,
    /// The Chezzi-visible name, for error messages.
    name: String,
    /// The address of `free` in the loaded library (resolved once via `dlsym("free")` at construction),
    /// used to release an `OwnedStr`/`OptOwnedStr` return after it is copied. `None` when the symbol
    /// can't be resolved (the lib has no `free`) — then an owned return degrades to the old leak rather
    /// than aborting. Excluded from `PartialEq`: it's a function of the lib, not the signature.
    free_addr: Option<usize>,
    /// Declared with a trailing bare `...` (C varargs): every call builds its CIF with
    /// `ffi_prep_cif_var` (fixed count = `params.len()`), and each surplus arg passes as its
    /// [`vararg_ctype_of`] type.
    c_variadic: bool,
}

/// The C type a vararg VALUE passes as through a C `...`: its scalar `CType`, then C's default
/// argument promotions ([`CType::vararg_promoted`], the one table the checker's `c_vararg_ctype` also
/// ends in). `None` for a struct, list or any other non-scalar value.
pub fn vararg_ctype_of(v: &NativeRet) -> Option<CType> {
    match v {
        NativeRet::Int(_) => CType::Int,
        NativeRet::Float(_) => CType::Float,
        NativeRet::Bool(_) => CType::Bool,
        NativeRet::Str(_) => CType::Str,
        NativeRet::Ptr(_) => CType::Ptr,
        NativeRet::Bytes(_)
        | NativeRet::List(_)
        | NativeRet::Struct { .. }
        | NativeRet::Map(_)
        | NativeRet::Ok(_)
        | NativeRet::Err(_)
        | NativeRet::Some(_)
        | NativeRet::None
        | NativeRet::Nil => return None,
    }
    .vararg_promoted()
}

/// A CIF for a C variadic call: `Cif::new` builds and owns the arg/result types, then
/// `ffi_prep_cif_var` re-preps its `ffi_cif` in place with `nfixed` fixed args. A fixed-arity CIF is
/// not a substitute: on x86-64 it leaves `%al` unset, which breaks `double` varargs.
fn variadic_cif(nfixed: usize, args: Vec<Type>, ret: Type) -> Result<Cif, HostError> {
    let ntotal = args.len();
    let cif = Cif::new(args, ret);
    let raw = cif.as_raw_ptr();
    // SAFETY: `raw` is the `ffi_cif` that `cif` owns; its `rtype`/`arg_types` point at the result
    // type and the `ntotal`-long arg type array `cif` also owns, so both outlive this call and every
    // later `ffi_call` through `cif`. `nfixed <= ntotal` (the surplus args are appended after the
    // declared params).
    let status = unsafe {
        libffi::low::prep_cif_var(
            raw,
            libffi::raw::ffi_abi_FFI_DEFAULT_ABI,
            nfixed,
            ntotal,
            (*raw).rtype,
            (*raw).arg_types,
        )
    };
    status.map_err(|e| HostError {
        message: format!("ffi_prep_cif_var failed: {e:?}"),
    })?;
    Ok(cif)
}

/// Marshal one `str` arg into a NUL-terminated C buffer and push it to `cstrings`/`ptr_args`,
/// returning its index there. The buffer is retained (TICKET-060) when an identical one already is,
/// or when `out_param_risk` says a `ptr` arg may carry a pointer back into it; otherwise its pointer
/// is filled in after every arg is read.
/// Read extern int arg `i` for the fixed-width param `p`, faulting when it does not fit the width
/// (`CType::check_fits`). The caller's `as` narrowing is then lossless.
fn arg_width_int(host: &mut dyn Host, i: usize, p: &CType) -> Result<i64, HostError> {
    let n = host.arg_int(i)?;
    p.check_fits(&NativeRet::Int(n))?;
    Ok(n)
}

fn marshal_str_arg(
    s: String,
    i: usize,
    fn_name: &str,
    out_param_risk: bool,
    cstrings: &mut Vec<Option<CString>>,
    ptr_args: &mut Vec<*const std::os::raw::c_char>,
) -> Result<usize, HostError> {
    let cs = CString::new(s).map_err(|_| HostError {
        message: format!("argument {i} to '{fn_name}' contains an interior NUL byte"),
    })?;
    if let Some(p) = retained_lookup(cs.as_bytes_with_nul()) {
        cstrings.push(None);
        ptr_args.push(p);
    } else if out_param_risk {
        cstrings.push(None);
        ptr_args.push(retain_cstring(cs));
    } else {
        cstrings.push(Some(cs));
        ptr_args.push(std::ptr::null());
    }
    Ok(cstrings.len() - 1)
}

/// Resolves a LOGICAL library name (`"libc"`/`"libm"`) to a per-platform candidate list, tried in
/// order by [`Cffi::new`]. `None` means `lib` is not a logical name -- `dlopen` it verbatim. `Some`
/// with an empty `Vec` means `lib` is a logical name with no known library on this platform.
///
/// `libc.so`/`libm.so` are glibc **linker scripts**, not ELF (`dlopen` rejects them), so on Linux
/// the versioned soname is tried first and the unversioned name is a last-resort fallback for musl
/// and other unices.
pub fn resolve_lib_candidates(lib: &str) -> Option<Vec<&'static str>> {
    match lib {
        "libc" => Some(if cfg!(target_os = "linux") {
            vec!["libc.so.6", "libc.so"]
        } else if cfg!(target_os = "macos") {
            vec!["libSystem.B.dylib", "libc.dylib"]
        } else if cfg!(unix) {
            vec!["libc.so"]
        } else {
            vec![]
        }),
        "libm" => Some(if cfg!(target_os = "linux") {
            vec!["libm.so.6", "libm.so"]
        } else if cfg!(target_os = "macos") {
            vec!["libSystem.B.dylib", "libm.dylib"]
        } else if cfg!(unix) {
            vec!["libm.so"]
        } else {
            vec![]
        }),
        _ => None,
    }
}

/// `Some(reason)` when `libc`/`libm` resolve to no known library on this platform (see
/// [`resolve_lib_candidates`]); `None` otherwise. Used by FFI goldens to skip loudly instead of
/// compiling themselves out.
pub fn platform_c_library_missing() -> Option<&'static str> {
    match resolve_lib_candidates("libc") {
        Some(c) if c.is_empty() => {
            Some("this platform has no libc/libm alias (see native::cffi::resolve_lib_candidates)")
        }
        _ => None,
    }
}

impl Cffi {
    /// `dlopen(lib)` + `dlsym(sym_name)`, capturing the marshalling signature. Errors (library not
    /// found, symbol missing) surface as a [`HostError`] the engine maps to a runtime error.
    pub fn new(
        lib: &str,
        sym_name: &str,
        params: Vec<CType>,
        ret: Option<CType>,
    ) -> Result<Self, HostError> {
        // SAFETY: dlopen of an arbitrary shared library. The caller (the `extern "lib":` author)
        // is responsible for naming a real library; we surface any loader error rather than UB.
        let library = match resolve_lib_candidates(lib) {
            None => unsafe { Library::new(lib) }.map_err(|e| HostError {
                message: format!("cannot load library '{lib}': {e}"),
            })?,
            Some(candidates) if candidates.is_empty() => {
                return Err(HostError {
                    message: format!(
                        "cannot load library '{lib}': no shared library is known for this logical name on this platform"
                    ),
                });
            }
            Some(candidates) => {
                let mut attempts = Vec::new();
                let mut loaded = None;
                for cand in &candidates {
                    match unsafe { Library::new(cand) } {
                        Ok(lib) => {
                            loaded = Some(lib);
                            break;
                        }
                        Err(e) => attempts.push(format!("{cand} ({e})")),
                    }
                }
                loaded.ok_or_else(|| HostError {
                    message: format!(
                        "cannot load library '{lib}' (tried {}): {}",
                        candidates.join(", "),
                        attempts.join("; ")
                    ),
                })?
            }
        };
        let addr = {
            // SAFETY: dlsym of a named symbol in the just-loaded library. We do not deref the
            // pointer here; it is only invoked later via libffi with the checker-verified signature.
            let symbol: libloading::Symbol<'_, *mut c_void> = unsafe {
                library.get(sym_name.as_bytes()).map_err(|e| HostError {
                    message: format!("symbol '{sym_name}' not found in '{lib}': {e}"),
                })?
            };
            // SAFETY: we relinquish the lifetime tie to `library`, but keep `library` alive for the
            // whole life of this `Cffi`, so the address remains valid until drop.
            unsafe { symbol.try_as_raw_ptr() }.ok_or_else(|| HostError {
                message: format!("symbol '{sym_name}' has no address on this platform"),
            })? as usize
        };
        // Resolve `free` once (only needed for an owned-str return). Best-effort: a lib without a
        // `free` symbol leaves this `None` and the owned return degrades to the documented leak.
        // Resolving from the just-loaded `library` gets libc's `free` (libc is in every process's
        // symbol scope; a third-party lib's own allocator is not supported — see docs §Level-3).
        let free_addr = if matches!(ret, Some(CType::OwnedStr) | Some(CType::OptOwnedStr)) {
            // SAFETY: dlsym of a named symbol; the pointer is only ever invoked later via libffi with
            // the standard `void free(void*)` signature, never deref'd here.
            unsafe {
                library
                    .get::<*mut c_void>(b"free")
                    .ok()
                    .and_then(|s| s.try_as_raw_ptr())
                    .map(|p| p as usize)
            }
        } else {
            None
        };
        Ok(Cffi {
            _lib: library,
            sym: addr,
            params,
            ret,
            name: sym_name.to_string(),
            free_addr,
            c_variadic: false,
        })
    }

    /// Mark this fn C variadic (`fn printf(fmt: str, ...)`): see [`Cffi::c_variadic`].
    pub fn with_c_variadic(mut self, on: bool) -> Self {
        self.c_variadic = on;
        self
    }

    /// Whether this fn takes C varargs after its declared params.
    pub fn is_c_variadic(&self) -> bool {
        self.c_variadic
    }

    /// Copy a non-null `char*` return into an owned Chezzi `String`, then free the C buffer with the
    /// cached libc `free` (if resolved). The copy happens BEFORE the free, so the data is safe — and
    /// a non-UTF-8 buffer still frees before its fault propagates (no leak on the error path).
    /// SAFETY: `p` must be a non-null pointer to a NUL-terminated, malloc'd buffer (the `owned_str`
    /// user assertion across the C trust boundary). `free_addr`, if present, is the standard libc
    /// `void free(void*)`.
    unsafe fn copy_and_free_owned(
        &self,
        p: *const std::os::raw::c_char,
    ) -> Result<String, HostError> {
        let s = unsafe { CStr::from_ptr(p) }
            .to_str()
            .map(str::to_owned)
            .map_err(|e| super::ffi::non_utf8_err(&format!("extern fn '{}'", self.name), e));
        if let Some(free) = self.free_addr {
            let free_code = CodePtr::from_ptr(free as *const c_void);
            let free_cif = Cif::new([Type::pointer()], Type::void());
            let ptr = p as *mut c_void;
            // SAFETY: `free_code` is libc `free`; `free_cif` matches its `void free(void*)` signature;
            // `ptr` is the malloc'd buffer we just copied out of (non-null, owned per user assertion).
            unsafe {
                let _: () = free_cif.call(free_code, &[arg(&ptr)]);
            }
        }
        s
    }

    /// Does a parameter take a callback, so the C function can call back into the engine?
    pub fn has_callback_param(&self) -> bool {
        self.params
            .iter()
            .any(|p| matches!(p, CType::Callback { .. }))
    }

    /// The declared parameter types (used by the engines to bounds-check arity before the call).
    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    /// The Chezzi-visible function name (for diagnostics).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Pull each argument through the [`Host`] (the same `arg_int`/`arg_float`/`arg_str` the std
    /// modules use), marshal to libffi, invoke the C function, and lower the result into a
    /// [`NativeRet`]. Arity and arg types are guaranteed by the checker, so a wrong arg is a bug.
    pub fn call(&self, host: &mut dyn Host) -> Result<NativeRet, HostError> {
        // Storage that must outlive the libffi `call`: integer/float/bool scalars (libffi reads
        // through `&` pointers) and the `CString`s backing `const char*` args (the pointer must
        // stay valid for the whole call).
        let mut int_args: Vec<std::os::raw::c_long> = Vec::new();
        let mut float_args: Vec<f64> = Vec::new();
        // C `float` args (`float32`): the Chezzi f64 rounded to nearest f32.
        let mut f32_args: Vec<f32> = Vec::new();
        // C `_Bool` args are one byte (0/1); the `u8` backing matches the `_Bool` ffi_type.
        let mut bool_args: Vec<u8> = Vec::new();
        // A `None` slot means a retained buffer whose pointer is already filled in `ptr_args`
        // (TICKET-060) -- either because a lookup found an existing one for this content, or because
        // this arg is eagerly retained (see `out_param_risk` below).
        let mut cstrings: Vec<Option<CString>> = Vec::new();
        let mut ptr_args: Vec<*const std::os::raw::c_char> = Vec::new();
        // Raw `void*` handles (Chezzi `ptr` args) — plain addresses, stable once pushed.
        let mut void_args: Vec<*mut c_void> = Vec::new();
        // Fixed-width integer args: each width gets its own typed vec because libffi reads through a
        // `&T` of the exact C width. The Chezzi i64 is C-cast (`as`) to the width, wrapping on
        // overflow (never a trap).
        let mut i8_args: Vec<i8> = Vec::new();
        let mut i16_args: Vec<i16> = Vec::new();
        let mut i32_args: Vec<i32> = Vec::new();
        let mut i64_args: Vec<i64> = Vec::new();
        let mut u8_args: Vec<u8> = Vec::new();
        let mut u16_args: Vec<u16> = Vec::new();
        let mut u32_args: Vec<u32> = Vec::new();
        let mut u64_args: Vec<u64> = Vec::new();
        // By-value struct args: each is a heap byte buffer holding the C-ABI struct image, written at
        // libffi-computed offsets. Each buffer is a `Vec<u64>` (not `Vec<u8>`): libffi's by-value
        // struct avalue must satisfy the struct's natural alignment, and a `u64` backing guarantees
        // 8-byte alignment — `>=` every v1 flat-scalar field's alignment (ptr/double/int64 = 8), where
        // a plain `Vec<u8>` only guarantees 1. The `Vec<u64>` owns its allocation, so its address is
        // stable as this outer `Vec` grows; the `Arg` points at the first word (the struct's first byte).
        let mut struct_bufs: Vec<Vec<u64>> = Vec::new();
        // Live libffi closures backing any callback args. Each holds the CIF + boxed userdata libffi
        // reads through; all kept alive until AFTER `ffi_call` returns, then dropped — which POISONS
        // and LEAKS an armed one (and frees an unarmed one): `CallbackClosure::drop`, gaps.md W6-8.
        // The `void*` code pointer pushed as the arg is stored in `cb_codes`.
        let mut callback_closures: Vec<CallbackClosure<'_>> = Vec::new();
        let mut cb_codes: Vec<*mut c_void> = Vec::new();
        // A fault out-slot the trampoline stashes a callback error / caught panic into; drained after
        // the call and re-raised as the extern call's own error (boxed so its address is stable while
        // `callback_closures` grows — each `TrampolineCtx` holds a raw `*mut` to it).
        let mut callback_fault: Box<Option<HostError>> = Box::new(None);
        let fault_ptr: *mut Option<HostError> = &mut *callback_fault;

        // First pass: extract & own every scalar so the `&`-references libffi captures stay valid.
        // Each slot records which storage vec holds it and at what index.
        enum Slot {
            Int(usize),
            Float(usize),
            /// A C `float` (`float32`), stored in `f32_args`.
            F32(usize),
            Bool(usize),
            Ptr(usize),
            /// An opaque `void*` handle, stored in `void_args`.
            RawPtr(usize),
            /// A fixed-width integer, stored in the matching `iN_args`/`uN_args` vec.
            I8(usize),
            I16(usize),
            I32(usize),
            I64(usize),
            U8(usize),
            U16(usize),
            U32(usize),
            U64(usize),
            /// A by-value struct arg, stored in `struct_bufs` (the `Arg` points at the buffer start).
            Struct(usize),
            /// A callback arg: the closure's code `void*` is stored in `cb_codes` at this index (the
            /// `Arg` points at that pointer cell).
            Callback(usize),
        }
        let mut slots: Vec<Slot> = Vec::with_capacity(self.params.len());
        // C varargs: every arg past the declared params, read up front (all `host.arg_*` reads must
        // precede the callback host-pointer capture below) with the C type it passes as.
        let mut varargs: Vec<(CType, NativeRet)> = Vec::new();
        if self.c_variadic {
            for i in self.params.len()..host.arg_count() {
                let v = host.arg_c_value(i)?;
                let ct = vararg_ctype_of(&v).ok_or_else(|| HostError {
                    message: format!(
                        "argument {i} to '{}' cannot be passed to a C variadic parameter",
                        self.name
                    ),
                })?;
                varargs.push((ct, v));
            }
        }
        // A `ptr` PARAM may be a C out-param the callee writes an interior pointer into (`strtol`'s
        // endptr): that write is invisible afterwards, because we can neither size nor safely read
        // the user's buffer, so any `str` arg alongside a `ptr` param is retained EAGERLY rather than
        // judged retrospectively like a `ptr` return. A vararg `ptr`/`str` counts too (`sscanf`).
        let arg_ctypes = || self.params.iter().chain(varargs.iter().map(|(c, _)| c));
        let out_param_risk = arg_ctypes().any(|p| matches!(p, CType::Ptr))
            && arg_ctypes().any(|p| matches!(p, CType::Str));
        for (i, p) in self.params.iter().enumerate() {
            match p {
                CType::Int => {
                    int_args.push(host.arg_int(i)? as std::os::raw::c_long);
                    slots.push(Slot::Int(int_args.len() - 1));
                }
                CType::Float => {
                    float_args.push(host.arg_float(i)?);
                    slots.push(Slot::Float(float_args.len() - 1));
                }
                CType::Float32 => {
                    let f = host.arg_float(i)?;
                    p.check_fits(&NativeRet::Float(f))?;
                    f32_args.push(f as f32);
                    slots.push(Slot::F32(f32_args.len() - 1));
                }
                CType::Bool => {
                    // Marshal a Chezzi `bool` into a C `_Bool` (1 byte, 0/1) via the host's typed
                    // bool reader.
                    let b = host.arg_bool(i)?;
                    bool_args.push(if b { 1 } else { 0 });
                    slots.push(Slot::Bool(bool_args.len() - 1));
                }
                CType::Str => {
                    let s = host.arg_str(i)?;
                    let idx = marshal_str_arg(
                        s,
                        i,
                        &self.name,
                        out_param_risk,
                        &mut cstrings,
                        &mut ptr_args,
                    )?;
                    slots.push(Slot::Ptr(idx));
                }
                CType::Ptr => {
                    // An opaque handle: read its raw address and pass it through as a `void*`.
                    void_args.push(host.arg_ptr(i)? as *mut c_void);
                    slots.push(Slot::RawPtr(void_args.len() - 1));
                }
                // Fixed-width integers: read the Chezzi i64, range-check it against the C width
                // (`arg_width_int`, a recoverable fault: `value 300 does not fit int8 (-128..127)`),
                // then narrow with `as` — lossless once checked; uint64 keeps the i64's bit pattern.
                // Each pushes into its own typed storage vec.
                CType::Int8 => {
                    i8_args.push(arg_width_int(host, i, p)? as i8);
                    slots.push(Slot::I8(i8_args.len() - 1));
                }
                CType::Int16 => {
                    i16_args.push(arg_width_int(host, i, p)? as i16);
                    slots.push(Slot::I16(i16_args.len() - 1));
                }
                CType::Int32 => {
                    i32_args.push(arg_width_int(host, i, p)? as i32);
                    slots.push(Slot::I32(i32_args.len() - 1));
                }
                CType::Int64 => {
                    i64_args.push(arg_width_int(host, i, p)?);
                    slots.push(Slot::I64(i64_args.len() - 1));
                }
                CType::UInt8 => {
                    u8_args.push(arg_width_int(host, i, p)? as u8);
                    slots.push(Slot::U8(u8_args.len() - 1));
                }
                CType::UInt16 => {
                    u16_args.push(arg_width_int(host, i, p)? as u16);
                    slots.push(Slot::U16(u16_args.len() - 1));
                }
                CType::UInt32 => {
                    u32_args.push(arg_width_int(host, i, p)? as u32);
                    slots.push(Slot::U32(u32_args.len() - 1));
                }
                CType::UInt64 => {
                    u64_args.push(arg_width_int(host, i, p)? as u64);
                    slots.push(Slot::U64(u64_args.len() - 1));
                }
                CType::Struct { fields, .. } => {
                    // Read the Chezzi struct as a nested value shape (declaration order), then write
                    // it into a C-ABI struct buffer at the libffi offsets (`write_field` recurses into
                    // nested struct fields). The buffer is the `Arg` payload passed by value.
                    let v = host.arg_c_value(i)?;
                    let (_ty, size, _align, _offsets) = struct_layout(fields);
                    let words = size.div_ceil(8).max(1);
                    let mut buf: Vec<u64> = vec![0u64; words];
                    {
                        // SAFETY: a byte view over the 8-aligned `u64` storage (same lifetime/owner),
                        // used only to write each field at its libffi offset (all within `size`).
                        let bytes = unsafe {
                            std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, words * 8)
                        };
                        write_field(bytes, 0, p, &v).map_err(|e| HostError {
                            message: format!("argument {i} to '{}': {}", self.name, e.message),
                        })?;
                    }
                    struct_bufs.push(buf);
                    slots.push(Slot::Struct(struct_bufs.len() - 1));
                }
                CType::Callback { params, ret } => {
                    // Build a libffi closure trampoline for this callback param. Its userdata holds a
                    // this arg index + the callback signature + the shared fault slot; when C invokes
                    // the code pointer, `callback_trampoline` re-enters the engine via
                    // `host.invoke_callback(arg_index, ...)`. The host raw pointer is deliberately NOT
                    // captured here — it is patched in after the loop (as the final use of `host`), so
                    // a trailing `host.arg_*` reborrow can't invalidate it (Stacked/Tree Borrows).
                    let mut ctx = Box::new(TrampolineCtx {
                        armed: std::sync::atomic::AtomicBool::new(false),
                        // SAFETY: `pthread_self` is async-signal-safe and reads no shared state.
                        owner: unsafe { libc::pthread_self() },
                        host: None,
                        arg_index: i,
                        params: params.as_slice() as *const [CType],
                        ret: ret.as_deref().map(|r| r as *const CType),
                        fault: fault_ptr,
                    });
                    // The closure's own CIF (callback signature). libffi stores a raw pointer to this
                    // CIF in the closure and reads through it when C invokes the callback, so the
                    // `ffi_cif` must stay at a STABLE ADDRESS until after `ffi_call`. `Box` it so the
                    // later move into `CallbackClosure`/the `callback_closures` Vec can't relocate it
                    // (a by-value `Cif` here dangled that pointer → SIGSEGV in `classify_argument`).
                    let cb_cif = Box::new(Cif::new(
                        params.iter().map(|p| p.ffi_type()),
                        match ret {
                            Some(r) => r.ffi_type(),
                            None => Type::void(),
                        },
                    ));
                    // NOT `libffi::low::closure_alloc()`: on failure `ffi_closure_alloc` returns NULL
                    // WITHOUT writing the code pointer, and that wrapper `assume_init()`s the
                    // still-uninitialised slot (uninit read = UB) and hands back a NULL handle
                    // `ffi_prep_closure_loc` then writes through (NULL-deref SIGSEGV). Exhausting the
                    // exec-closure pool is REACHABLE here — an armed trampoline is leaked, not freed
                    // (see `CallbackClosure`'s `Drop`) — so the NULL must become a clean, recoverable
                    // Chezzi error instead of the crash the leak would otherwise walk into.
                    let mut code: *mut c_void = std::ptr::null_mut();
                    // SAFETY: the libffi entry point `low::closure_alloc` wraps; it writes `code` and
                    // returns the writable closure handle, or returns NULL having written nothing.
                    let handle = unsafe {
                        libffi::raw::ffi_closure_alloc(
                            std::mem::size_of::<libffi::raw::ffi_closure>(),
                            &mut code,
                        )
                    } as *mut libffi::raw::ffi_closure;
                    if handle.is_null() || code.is_null() {
                        if !handle.is_null() {
                            // SAFETY: allocated just above and never handed to C.
                            unsafe { libffi::low::closure_free(handle) };
                        }
                        return Err(HostError {
                            message: format!(
                                "cannot allocate a callback trampoline for argument {i} to '{}': \
                                 the FFI closure pool is exhausted (one trampoline leaks per \
                                 callback-passing extern call — see gaps.md W6-8)",
                                self.name
                            ),
                        });
                    }
                    // SAFETY: `handle`/`code` are a fresh closure pair from `ffi_closure_alloc`; `cb_cif`
                    // is the callback's CIF (kept alive in `CallbackClosure`); `callback_trampoline`
                    // matches libffi's `RawCallback` shape; `&mut *ctx` is the live boxed userdata
                    // (also kept alive in `CallbackClosure`, address stable behind the `Box`).
                    let status = unsafe {
                        libffi::raw::ffi_prep_closure_loc(
                            handle,
                            cb_cif.as_raw_ptr(),
                            Some(callback_trampoline),
                            &mut *ctx as *mut TrampolineCtx<'_> as *mut c_void,
                            code,
                        )
                    };
                    if status != libffi::raw::ffi_status_FFI_OK {
                        // SAFETY: free the just-allocated closure before bailing (no call used it).
                        unsafe { libffi::low::closure_free(handle) };
                        return Err(HostError {
                            message: format!(
                                "failed to build callback trampoline for argument {i} to '{}'",
                                self.name
                            ),
                        });
                    }
                    // `ManuallyDrop` because an ARMED closure is POISONED-AND-LEAKED, not freed, when
                    // the call returns (see `CallbackClosure::drop`, which still frees an unarmed
                    // one). The `Box`es stay — they are what pins the `ffi_cif`/userdata addresses
                    // libffi stored above.
                    callback_closures.push(CallbackClosure {
                        handle,
                        _cif: ManuallyDrop::new(cb_cif),
                        ctx: ManuallyDrop::new(ctx),
                    });
                    cb_codes.push(code);
                    slots.push(Slot::Callback(cb_codes.len() - 1));
                }
                // `OwnedStr`/`OptStr`/`OptOwnedStr` are RETURN-ONLY (the checker rejects them as
                // params, resolving alias chains first). This arm should be unreachable, but we
                // return a recoverable fault rather than `unreachable!` so a checker gap can never
                // abort the process.
                CType::OwnedStr | CType::OptStr | CType::OptOwnedStr => {
                    return Err(HostError {
                        message: format!(
                            "argument {i} to '{}' uses a return-only C type \
                             (owned_str / str? cannot be a parameter)",
                            self.name
                        ),
                    });
                }
            }
        }
        // The C varargs, after the declared params, each as its promoted C type.
        let nparams = self.params.len();
        for (k, (ct, v)) in varargs.iter().enumerate() {
            match (ct, v) {
                (CType::Int, NativeRet::Int(n)) => {
                    int_args.push(*n as std::os::raw::c_long);
                    slots.push(Slot::Int(int_args.len() - 1));
                }
                (CType::Int32, NativeRet::Bool(b)) => {
                    i32_args.push(i32::from(*b));
                    slots.push(Slot::I32(i32_args.len() - 1));
                }
                (CType::Float, NativeRet::Float(f)) => {
                    float_args.push(*f);
                    slots.push(Slot::Float(float_args.len() - 1));
                }
                (CType::Ptr, NativeRet::Ptr(a)) => {
                    void_args.push(*a as *mut c_void);
                    slots.push(Slot::RawPtr(void_args.len() - 1));
                }
                (CType::Str, NativeRet::Str(s)) => {
                    let idx = marshal_str_arg(
                        s.clone(),
                        nparams + k,
                        &self.name,
                        out_param_risk,
                        &mut cstrings,
                        &mut ptr_args,
                    )?;
                    slots.push(Slot::Ptr(idx));
                }
                _ => {
                    return Err(HostError {
                        message: format!(
                            "argument {} to '{}' cannot be passed to a C variadic parameter",
                            nparams + k,
                            self.name
                        ),
                    });
                }
            }
        }

        // Capture the host pointer for every callback trampoline NOW — as the final use of `host`,
        // after all `host.arg_*` reads are done. Deriving it earlier (mid-loop) would be invalidated
        // by the subsequent reborrows for trailing params under Stacked/Tree Borrows; capturing it
        // last makes the raw pointer the live tag for the duration of `ffi_call` (where the
        // trampoline, firing synchronously on this thread, dereferences it).
        if !callback_closures.is_empty() {
            let host_ptr: *mut (dyn Host + '_) = host;
            for cc in &mut callback_closures {
                cc.ctx.host = Some(host_ptr);
                // ARM last, with `Release`: this publishes `host` (and every other ctx field) to
                // whatever thread ends up invoking the trampoline, and is the flag `Drop` clears to
                // poison the leaked allocation.
                cc.ctx
                    .armed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }

        // Fill the pointer args now that `cstrings` won't move again. A `None` slot's `ptr_args` entry
        // was already filled (a retained/looked-up buffer) at the `CType::Str` arm above.
        for slot in &slots {
            if let Slot::Ptr(idx) = slot
                && let Some(cs) = &cstrings[*idx]
            {
                ptr_args[*idx] = cs.as_ptr();
            }
        }

        // Build the libffi `Arg` list referencing the owned storage above.
        let mut ffi_args: Vec<libffi::middle::Arg> = Vec::with_capacity(slots.len());
        for slot in &slots {
            match slot {
                Slot::Int(idx) => ffi_args.push(arg(&int_args[*idx])),
                Slot::Float(idx) => ffi_args.push(arg(&float_args[*idx])),
                Slot::F32(idx) => ffi_args.push(arg(&f32_args[*idx])),
                Slot::Bool(idx) => ffi_args.push(arg(&bool_args[*idx])),
                Slot::Ptr(idx) => ffi_args.push(arg(&ptr_args[*idx])),
                Slot::RawPtr(idx) => ffi_args.push(arg(&void_args[*idx])),
                Slot::I8(idx) => ffi_args.push(arg(&i8_args[*idx])),
                Slot::I16(idx) => ffi_args.push(arg(&i16_args[*idx])),
                Slot::I32(idx) => ffi_args.push(arg(&i32_args[*idx])),
                Slot::I64(idx) => ffi_args.push(arg(&i64_args[*idx])),
                Slot::U8(idx) => ffi_args.push(arg(&u8_args[*idx])),
                Slot::U16(idx) => ffi_args.push(arg(&u16_args[*idx])),
                Slot::U32(idx) => ffi_args.push(arg(&u32_args[*idx])),
                Slot::U64(idx) => ffi_args.push(arg(&u64_args[*idx])),
                Slot::Struct(idx) => ffi_args.push(arg(&struct_bufs[*idx][0])),
                // The callback arg is the closure's code `void*` (a C function pointer).
                Slot::Callback(idx) => ffi_args.push(arg(&cb_codes[*idx])),
            }
        }

        let arg_types = self
            .params
            .iter()
            .chain(varargs.iter().map(|(c, _)| c))
            .map(|p| p.ffi_type());
        let result_ty = match &self.ret {
            Some(c) => c.ffi_type(),
            None => Type::void(),
        };
        let cif = if self.c_variadic {
            variadic_cif(nparams, arg_types.collect(), result_ty)?
        } else {
            Cif::new(arg_types.collect::<Vec<_>>(), result_ty)
        };
        let code = CodePtr::from_ptr(self.sym as *const c_void);

        // A by-value struct RETURN cannot use `middle::Cif::call::<R>` (it allocates a `MaybeUninit<R>`
        // for a statically-sized `R`); drop to the raw `ffi_call` with an own sized rvalue buffer.
        if let Some(ret @ CType::Struct { .. }) = &self.ret {
            let r = self.call_struct_return(&cif, code, &ffi_args, ret);
            if let Ok(v) = &r {
                retain_aliasing_str_args(v, &mut cstrings);
            }
            // A callback fired during the call may have stashed a fault; re-raise it over the result.
            // Read the fault BEFORE dropping the closures, then drop (poisons + leaks an armed one).
            if let Some(err) = callback_fault.take() {
                drop(callback_closures);
                return Err(err);
            }
            drop(callback_closures);
            return r;
        }

        // SAFETY: `code` is a function whose C signature matches `self.params`/`self.ret`, which the
        // checker enforces (every extern fn's param + return types are marshallable scalars, and the
        // call site is type-checked). `cif` is built from that same signature, `ffi_args` matches it
        // in order/count, and all referenced storage (`int_args`/`float_args`/`bool_args`/`ptr_args`/
        // `cstrings`) is still in scope, so the read-through pointers are valid for the whole call.
        let ret = unsafe {
            match &self.ret {
                Some(CType::Int) => {
                    let r: std::os::raw::c_long = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i64)
                }
                Some(CType::Float) => {
                    let r: f64 = cif.call(code, &ffi_args);
                    NativeRet::Float(r)
                }
                Some(CType::Float32) => {
                    let r: f32 = cif.call(code, &ffi_args);
                    NativeRet::Float(r as f64)
                }
                Some(CType::Bool) => {
                    // A C `_Bool` return is one byte, but libffi rvalue-widens any sub-register
                    // integral return to a full `ffi_arg` (register word) — the same rule the
                    // narrow-int arms below follow. Read the register width, then narrow to a byte
                    // and test `!= 0`. Reading through a 1-byte buffer would let `ffi_call` stomp
                    // 7 bytes past it (the stack OOB the narrow-int-return fix documents).
                    let r: std::os::raw::c_ulong = cif.call(code, &ffi_args);
                    NativeRet::Bool((r as u8) != 0)
                }
                Some(CType::Str) => {
                    let p: *const std::os::raw::c_char = cif.call(code, &ffi_args);
                    if p.is_null() {
                        // The extern is statically typed to return `str`, a non-null type. Yielding
                        // `nil` here would silently break that guarantee (a later `len(v)` would fault
                        // far from the cause). Fault honestly at the boundary instead — recoverable via
                        // `recover:`. (A genuinely-nullable C return opts in with `str?`; an owned
                        // malloc'd return that should be freed opts in with `owned_str`.)
                        return Err(HostError {
                            message: format!(
                                "extern fn '{}' returned NULL for its declared `str` return",
                                self.name
                            ),
                        });
                    }
                    // Copy immediately into an owned Chezzi str. A borrowed return: we never `free`
                    // the pointer (use `owned_str` for a malloc'd buffer Chezzi should own + free).
                    // `to_str` VALIDATES: a non-UTF-8 buffer is a fault, never a mangled `str`.
                    NativeRet::Str(
                        CStr::from_ptr(p)
                            .to_str()
                            .map_err(|e| {
                                super::ffi::non_utf8_err(&format!("extern fn '{}'", self.name), e)
                            })?
                            .to_owned(),
                    )
                }
                Some(CType::OwnedStr) => {
                    // RETURN-ONLY owned `char*`: same non-null rule as `str` (NULL faults — use
                    // `owned_str?` for nullable), but the malloc'd buffer is freed after the copy.
                    let p: *const std::os::raw::c_char = cif.call(code, &ffi_args);
                    if p.is_null() {
                        return Err(HostError {
                            message: format!(
                                "extern fn '{}' returned NULL for its declared `owned_str` return",
                                self.name
                            ),
                        });
                    }
                    NativeRet::Str(self.copy_and_free_owned(p)?)
                }
                Some(CType::OptStr) => {
                    // RETURN-ONLY nullable `char*`: NULL → None, non-null → Some(copied str). The
                    // borrowed (not freed) nullable opt-in (e.g. `getenv`).
                    let p: *const std::os::raw::c_char = cif.call(code, &ffi_args);
                    if p.is_null() {
                        NativeRet::None
                    } else {
                        NativeRet::Some(Box::new(NativeRet::Str(
                            CStr::from_ptr(p)
                                .to_str()
                                .map_err(|e| {
                                    super::ffi::non_utf8_err(
                                        &format!("extern fn '{}'", self.name),
                                        e,
                                    )
                                })?
                                .to_owned(),
                        )))
                    }
                }
                Some(CType::OptOwnedStr) => {
                    // RETURN-ONLY nullable + owned `char*`: NULL → None (frees nothing), non-null →
                    // Some(copied str) and the buffer is freed after the copy.
                    let p: *const std::os::raw::c_char = cif.call(code, &ffi_args);
                    if p.is_null() {
                        NativeRet::None
                    } else {
                        NativeRet::Some(Box::new(NativeRet::Str(self.copy_and_free_owned(p)?)))
                    }
                }
                Some(CType::Ptr) => {
                    // An opaque handle. Unlike `str`, a NULL return is NOT a fault — it is a
                    // legitimate "creation failed" signal; it lowers to `Ptr(0)` (== `std.ffi.null()`)
                    // for the program to test. The address is never deref'd or freed here.
                    let p: *mut c_void = cif.call(code, &ffi_args);
                    NativeRet::Ptr(p as usize)
                }
                // Fixed-width integer returns. CRITICAL: any integral return narrower than the
                // register word is widened by libffi, which writes a FULL `ffi_arg`/`ffi_sarg`
                // (= `c_ulong`/`c_long`, register-sized) into the rvalue buffer. Reading through a
                // narrow `iN`/`uN` would allocate a 1/2/4-byte buffer and let `ffi_call` stomp 4–7
                // bytes past it (stack OOB write / UB). So the sub-word widths read the
                // register-width word and then `as`-narrow-then-widen to recover the declared value
                // (signed narrow SIGN-extends, e.g. int32 -1 -> i64 -1; unsigned narrow ZERO-extends,
                // e.g. uint32 0xFFFFFFFF -> i64 4294967295). This masks any high-bit padding and is
                // endianness-independent (value-semantics on a Rust integer). `int64`/`uint64` are
                // already register-width, so they read their exact type directly. Mirrors the read
                // path in libffi-rs's own high-level wrapper.
                Some(CType::Int8) => {
                    let r: std::os::raw::c_long = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i8 as i64)
                }
                Some(CType::Int16) => {
                    let r: std::os::raw::c_long = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i16 as i64)
                }
                Some(CType::Int32) => {
                    let r: std::os::raw::c_long = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i32 as i64)
                }
                Some(CType::Int64) => {
                    let r: i64 = cif.call(code, &ffi_args);
                    NativeRet::Int(r)
                }
                Some(CType::UInt8) => {
                    let r: std::os::raw::c_ulong = cif.call(code, &ffi_args);
                    NativeRet::Int(r as u8 as i64)
                }
                Some(CType::UInt16) => {
                    let r: std::os::raw::c_ulong = cif.call(code, &ffi_args);
                    NativeRet::Int(r as u16 as i64)
                }
                Some(CType::UInt32) => {
                    let r: std::os::raw::c_ulong = cif.call(code, &ffi_args);
                    NativeRet::Int(r as u32 as i64)
                }
                Some(CType::UInt64) => {
                    // u64 -> i64 reinterprets the top bit (a value > i64::MAX wraps negative). This is
                    // the documented v1 limit: Chezzi `int` is i64, so a C uint64 above i64::MAX is not
                    // representable and wraps (C-cast). The other 7 widths fit i64 losslessly.
                    let r: u64 = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i64)
                }
                // A struct return was handled above (early return); a callback return is checker-
                // rejected (param-only); a `None`/void return falls here.
                Some(CType::Struct { .. }) | Some(CType::Callback { .. }) | None => {
                    let _: () = cif.call(code, &ffi_args);
                    NativeRet::Nil
                }
            }
        };
        // Retain (leak) any `str`-arg buffer a returned `ptr` aliases into, BEFORE the drop below --
        // a retained slot is already `None` there (TICKET-060), so this outlives that drop by design.
        retain_aliasing_str_args(&ret, &mut cstrings);
        // `cstrings` / `struct_bufs` (and the other storage) are dropped here, after the call returns
        // — never before. The callback closures are dropped last (an armed one is poisoned + leaked,
        // NOT freed — see `CallbackClosure::drop`) — also only after the call.
        drop(cstrings);
        drop(struct_bufs);
        drop(callback_closures);
        // A callback may have stashed a fault during the call; re-raise it over the C result (stronger
        // than ctypes' swallow-to-stderr-and-return-0). The fault slot was zeroed by the trampoline.
        if let Some(err) = callback_fault.take() {
            return Err(err);
        }
        Ok(ret)
    }

    /// Invoke a C fn whose return is a struct BY VALUE. `middle::Cif::call::<R>` can't represent a
    /// dynamically-sized return, so this drops to the raw `ffi_call` with an own rvalue buffer.
    ///
    /// libffi's rvalue contract: the rvalue buffer must be at least the struct's size AND at least
    /// register width (`sizeof(ffi_arg)`) — libffi may write a full register for a small struct
    /// returned in a register, so a buffer sized to exactly a tiny struct could be written past (the
    /// same OOB class the narrow-int-return fix guarded). We size it to
    /// `max(struct_size, sizeof(ffi_arg))` rounded up to the struct alignment, then read each field
    /// strictly at its libffi offset.
    fn call_struct_return(
        &self,
        cif: &Cif,
        code: CodePtr,
        ffi_args: &[libffi::middle::Arg],
        ret: &CType,
    ) -> Result<NativeRet, HostError> {
        let CType::Struct { fields, .. } = ret else {
            return Err(HostError {
                message: format!(
                    "extern fn '{}': struct return without a struct type",
                    self.name
                ),
            });
        };
        let (_ty, size, align, _offsets) = struct_layout(fields);
        let reg = std::mem::size_of::<libffi::raw::ffi_arg>();
        let mut rsize = size.max(reg);
        // Round up to the struct alignment so a register-floor bump can't leave a partial trailing slot.
        if align > 0 {
            rsize = rsize.div_ceil(align) * align;
        }
        // `Vec<u64>` (not `Vec<u8>`): libffi writes a small struct result into the rvalue with typed,
        // alignment-sensitive stores at the libffi-computed field offsets, so the buffer must meet the
        // struct's natural alignment. A `u64` backing guarantees 8-byte alignment (`>=` every v1
        // flat-scalar field's alignment); a `Vec<u8>` only guarantees 1.
        let words = rsize.div_ceil(8).max(1);
        let mut rvalue: Vec<u64> = vec![0u64; words];

        // `Arg` is `#[repr(C)]` over a single `*mut c_void`, so an `&[Arg]` is layout-compatible with
        // the `*mut *mut c_void` avalue array libffi expects (exactly what `middle::Cif::call` does
        // internally before calling `low::call`).
        // SAFETY: `cif` was prepped from this fn's signature (incl. the struct result type, identical
        // to the one whose offsets we computed); `ffi_args` matches the params in order/count and the
        // backing storage is still alive in the caller; `rvalue` is `words*8 >= max(struct_size, reg)`
        // bytes and 8-aligned.
        unsafe {
            libffi::raw::ffi_call(
                cif.as_raw_ptr(),
                Some(*code.as_safe_fun()),
                rvalue.as_mut_ptr() as *mut c_void,
                ffi_args.as_ptr() as *mut *mut c_void,
            );
        }

        // Read the struct back at offset 0 (`read_field` recurses into nested fields at their libffi
        // offsets) as the `NativeRet::Struct` the VM already lowers to a native struct.
        // SAFETY: byte view over the 8-aligned `u64` rvalue (same lifetime/owner); reads stay within
        // `words*8` bytes (every offset is `< size <= words*8`).
        let rbytes = unsafe { std::slice::from_raw_parts(rvalue.as_ptr() as *const u8, words * 8) };
        Ok(read_field(rbytes, 0, ret))
    }
}

impl PartialEq for Cffi {
    /// Two `Cffi`s are equal iff they resolve to the same symbol address with the same signature.
    /// (Used only to satisfy `Value`'s derived `PartialEq`; extern fns are rarely compared, and a
    /// program has no identity operator that would expose subtler distinctions.)
    fn eq(&self, other: &Self) -> bool {
        self.sym == other.sym
            && self.params == other.params
            && self.ret == other.ret
            && self.name == other.name
            && self.c_variadic == other.c_variadic
    }
}

impl std::fmt::Debug for Cffi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cffi")
            .field("name", &self.name)
            .field("params", &self.params)
            .field("ret", &self.ret)
            .field("c_variadic", &self.c_variadic)
            .finish()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
