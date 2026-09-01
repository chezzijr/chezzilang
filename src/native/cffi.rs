//! Dynamic C-ABI FFI (v1): the runtime machinery behind an `extern "lib":` block. A [`Cffi`]
//! wraps a `dlopen`'d shared library, a resolved symbol address, and the C signature (as
//! [`CType`]s), and exposes `call(&mut dyn Host)` — reusing the engine-neutral [`Host`]/[`NativeRet`]
//! seam (`src/native/mod.rs`) used by the sole M:N VM engine.
//!
//! v1 marshals scalars: `int` (i64 ↔ C `long`), `float` (f64 ↔ C `double`), `bool`
//! (↔ C `_Bool`, 1 byte 0/1), and `str` (Chezzi str → null-terminated `const char*`; a borrowed `char*` return
//! is copied immediately into an owned Chezzi str, never freed). Plus the bidirectional fixed-width
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
//! Structs by value, callbacks, varargs, the rich Rust `Arc<dyn Any>` userdata handle, and a custom
//! user-named deallocator are deferred (documented limits).
//!
//! ## Send/Sync (for `--parallel`)
//! The VM stores `Obj::Cffi(Arc<Cffi>)`, and the M:N engine shares the parent address space across
//! OS-thread workers, so [`Cffi`] must be `Send + Sync`. Two design choices make that hold:
//! - libloading's `Symbol` is `!Send`, so the resolved symbol is stored as a raw `usize` address
//!   (cast to a `CodePtr` at call time) with the `Library` kept alive in the same `Arc`.
//! - libffi's `Cif` is `!Send` (raw pointers, no `unsafe impl Send`), so it is **not** stored;
//!   the cheap `Cif::new` (`prep_cif`) is rebuilt per call from the `Send` [`CType`] signature.

use std::ffi::{CStr, CString, c_void};
use std::mem::ManuallyDrop;

use libffi::middle::{Cif, CodePtr, Type, arg};
use libloading::Library;

use super::{Host, HostError, NativeRet};

/// A C-marshallable type — the v1 FFI surface. `int`→C `long`, `float`→C `double`,
/// `bool`→C `_Bool` (1 byte), `str`→C `const char*`, `ptr`→C `void*` (an opaque handle), the fixed-width
/// integers (`int8`..`uint64`), and a flat struct-by-value (`Struct`) of those scalar variants.
///
/// Not `Copy`: the `Struct` variant carries owned data (`String`/`Vec`). It carries **only** owned
/// data, never a libffi `Type`/`Cif` (which are `!Send`/`!Sync`/`!Clone`): the libffi structure
/// `Type` is rebuilt per call from `fields` — exactly as the [`Cif`] is already rebuilt per call —
/// so [`CType`] (and the `Arc<Cffi>` that stores it) stays `Send + Sync` for `--parallel`/the M:N
/// snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CType {
    Int,
    Float,
    Bool,
    Str,
    /// An opaque `void*` handle (Chezzi `ptr`): a raw address marshalled by value, in and out.
    Ptr,
    /// A flat C struct passed/returned BY VALUE (v1: flat scalar fields only — nested structs and
    /// `str`/`owned_str` fields are rejected by the checker). `name`/`field_names` mirror the Chezzi
    /// `struct` so a by-value RETURN lowers to a [`NativeRet::Struct`] the VM already knows how to build; the
    /// libffi structure `Type` (and per-field offsets/size/alignment) is computed from `fields` at call
    /// time via [`struct_layout`] — never stored — so the platform ABI (small-struct-in-registers vs
    /// by-hidden-pointer) is libffi's, not hand-rolled. Fields are the scalar `CType` variants only
    /// (the checker rejects `Str`/`OwnedStr`/`OptStr`/`OptOwnedStr`/nested `Struct`).
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
    /// [`CType::Int`], which stays C `long`. To the Chezzi program each is a plain `int` (`Ty::Int`);
    /// the width/signedness is a runtime-only marshalling concern. A PARAM truncates the Chezzi i64 to
    /// the C width (wrapping, C-cast semantics — never an overflow trap); a RETURN sign-extends (signed)
    /// or zero-extends (unsigned) the C value back to i64. Valid as both param and return.
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
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
        ret: Box<CType>,
    },
}

impl CType {
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
        )
    }

    /// The libffi argument/result [`Type`] for this type. For a `Struct`, this builds a libffi
    /// structure type from the field types (libffi computes size/alignment/offsets from the ABI).
    fn ffi_type(&self) -> Type {
        match self {
            // int ↔ C `long` (i64 on LP64 Linux); bool ↔ C `_Bool` (1 byte, the SysV `_Bool`
            // stand-in libffi-rs itself uses).
            CType::Int => Type::c_long(),
            CType::Float => Type::f64(),
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
    let want_int = |v: &NativeRet| match v {
        NativeRet::Int(n) => Ok(*n),
        other => Err(HostError {
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
                other => {
                    return Err(HostError {
                        message: format!("struct field marshal: expected float, got {other:?}"),
                    });
                }
            };
            put!(f);
        }
        CType::Bool => {
            let b = match v {
                NativeRet::Bool(b) => *b,
                other => {
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
                other => {
                    return Err(HostError {
                        message: format!("struct field marshal: expected ptr, got {other:?}"),
                    });
                }
            };
            put!(a);
        }
        // The checker rejects str / owned / opt / nested-struct / callback fields, so they never
        // reach here.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Struct { .. }
        | CType::Callback { .. } => {
            return Err(HostError {
                message: "struct field marshal: str / nested-struct fields are not supported (v1)"
                    .into(),
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
        CType::Bool => {
            // C `_Bool` is one byte at its real offset; any nonzero byte is true.
            let c: u8 = get!(u8);
            NativeRet::Bool(c != 0)
        }
        CType::Ptr => NativeRet::Ptr(get!(usize)),
        // Unreachable for a well-typed struct (checker rejects str/owned/opt/nested/callback); Nil.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Struct { .. }
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
        CType::Bool => NativeRet::Bool(rd!(u8) != 0),
        CType::Ptr => NativeRet::Ptr(rd!(usize)),
        // Non-scalar parts are checker-rejected; default defensively to Nil.
        _ => NativeRet::Nil,
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
        _ => 0,
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
                    _ => 0.0,
                };
                *(slot as *mut f64) = f;
            }
            CType::Ptr => {
                let a = match v {
                    NativeRet::Ptr(a) => *a,
                    NativeRet::Int(n) => *n as usize,
                    _ => 0,
                };
                *(slot as *mut *mut c_void) = a as *mut c_void;
            }
            // Non-scalar returns are checker-rejected; zero the register word defensively.
            _ => *(slot as *mut libffi::raw::ffi_arg) = 0,
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
    ret: *const CType,
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
    // SAFETY: `ctx.ret` points at the live callback return CType.
    let ret_ct: &CType = unsafe { &*ctx.ret };

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
        host.invoke_callback(ctx.arg_index, &native_args)
    }));

    match outcome {
        Ok(Ok(v)) => {
            // SAFETY: `result` is libffi's rvalue buffer (>= register width, aligned); `ret_ct` is the
            // declared return width.
            unsafe { write_c_result(result, ret_ct, &v) };
        }
        Ok(Err(host_err)) => {
            // SAFETY: zero the result so C unwinds with a defined value.
            unsafe { write_c_result(result, ret_ct, &NativeRet::Int(0)) };
            // SAFETY: `ctx.fault` points into `Cffi::call`'s live `Box<Option<HostError>>` out-slot
            // (heap, freed when that call returns — the poison guard above is what keeps a
            // post-return invocation from writing through it).
            unsafe { *ctx.fault = Some(host_err) };
        }
        Err(panic_payload) => {
            // SAFETY: zero the result so C unwinds with a defined value (no panic into C frames).
            unsafe { write_c_result(result, ret_ct, &NativeRet::Int(0)) };
            let msg = panic_payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic_payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "callback panicked".to_string());
            // SAFETY: as above — the boxed fault out-slot is live for the duration of the call.
            unsafe {
                *ctx.fault = Some(HostError {
                    message: format!("callback panicked: {msg}"),
                })
            };
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
        })
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
        // C `_Bool` args are one byte (0/1); the `u8` backing matches the `_Bool` ffi_type.
        let mut bool_args: Vec<u8> = Vec::new();
        let mut cstrings: Vec<CString> = Vec::new();
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
                CType::Bool => {
                    // Marshal a Chezzi `bool` into a C `_Bool` (1 byte, 0/1) via the host's typed
                    // bool reader.
                    let b = host.arg_bool(i)?;
                    bool_args.push(if b { 1 } else { 0 });
                    slots.push(Slot::Bool(bool_args.len() - 1));
                }
                CType::Str => {
                    let s = host.arg_str(i)?;
                    let cs = CString::new(s).map_err(|_| HostError {
                        message: format!(
                            "argument {i} to '{}' contains an interior NUL byte",
                            self.name
                        ),
                    })?;
                    cstrings.push(cs);
                    ptr_args.push(std::ptr::null());
                    slots.push(Slot::Ptr(cstrings.len() - 1));
                }
                CType::Ptr => {
                    // An opaque handle: read its raw address and pass it through as a `void*`.
                    void_args.push(host.arg_ptr(i)? as *mut c_void);
                    slots.push(Slot::RawPtr(void_args.len() - 1));
                }
                // Fixed-width integers: read the Chezzi i64 and TRUNCATE to the C width via a Rust
                // `as` cast — wrapping (C-cast) semantics, never an overflow trap (300i64 -> int8 ==
                // 44, 255i64 -> int8 == -1). Each pushes into its own typed storage vec.
                CType::Int8 => {
                    i8_args.push(host.arg_int(i)? as i8);
                    slots.push(Slot::I8(i8_args.len() - 1));
                }
                CType::Int16 => {
                    i16_args.push(host.arg_int(i)? as i16);
                    slots.push(Slot::I16(i16_args.len() - 1));
                }
                CType::Int32 => {
                    i32_args.push(host.arg_int(i)? as i32);
                    slots.push(Slot::I32(i32_args.len() - 1));
                }
                CType::Int64 => {
                    i64_args.push(host.arg_int(i)?);
                    slots.push(Slot::I64(i64_args.len() - 1));
                }
                CType::UInt8 => {
                    u8_args.push(host.arg_int(i)? as u8);
                    slots.push(Slot::U8(u8_args.len() - 1));
                }
                CType::UInt16 => {
                    u16_args.push(host.arg_int(i)? as u16);
                    slots.push(Slot::U16(u16_args.len() - 1));
                }
                CType::UInt32 => {
                    u32_args.push(host.arg_int(i)? as u32);
                    slots.push(Slot::U32(u32_args.len() - 1));
                }
                CType::UInt64 => {
                    u64_args.push(host.arg_int(i)? as u64);
                    slots.push(Slot::U64(u64_args.len() - 1));
                }
                CType::Struct { fields, .. } => {
                    // Read the Chezzi struct's fields as engine-neutral scalars (declaration order),
                    // then write each into a C-ABI struct buffer at its libffi offset. The buffer is
                    // the `Arg` payload passed by value.
                    let field_vals = host.arg_struct_fields(i)?;
                    if field_vals.len() != fields.len() {
                        return Err(HostError {
                            message: format!(
                                "argument {i} to '{}' has {} struct field(s), expected {}",
                                self.name,
                                field_vals.len(),
                                fields.len()
                            ),
                        });
                    }
                    let (_ty, size, _align, offsets) = struct_layout(fields);
                    let words = size.div_ceil(8).max(1);
                    let mut buf: Vec<u64> = vec![0u64; words];
                    {
                        // SAFETY: a byte view over the 8-aligned `u64` storage (same lifetime/owner),
                        // used only to write each field at its libffi offset (all within `size`).
                        let bytes = unsafe {
                            std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, words * 8)
                        };
                        for ((ct, off), v) in
                            fields.iter().zip(offsets.iter()).zip(field_vals.iter())
                        {
                            write_field(bytes, *off, ct, v)?;
                        }
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
                        ret: ret.as_ref() as *const CType,
                        fault: fault_ptr,
                    });
                    // The closure's own CIF (callback signature). libffi stores a raw pointer to this
                    // CIF in the closure and reads through it when C invokes the callback, so the
                    // `ffi_cif` must stay at a STABLE ADDRESS until after `ffi_call`. `Box` it so the
                    // later move into `CallbackClosure`/the `callback_closures` Vec can't relocate it
                    // (a by-value `Cif` here dangled that pointer → SIGSEGV in `classify_argument`).
                    let cb_cif = Box::new(Cif::new(
                        params.iter().map(|p| p.ffi_type()),
                        ret.ffi_type(),
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

        // Fill the pointer args now that `cstrings` won't move again.
        for slot in &slots {
            if let Slot::Ptr(idx) = slot {
                ptr_args[*idx] = cstrings[*idx].as_ptr();
            }
        }

        // Build the libffi `Arg` list referencing the owned storage above.
        let mut ffi_args: Vec<libffi::middle::Arg> = Vec::with_capacity(slots.len());
        for slot in &slots {
            match slot {
                Slot::Int(idx) => ffi_args.push(arg(&int_args[*idx])),
                Slot::Float(idx) => ffi_args.push(arg(&float_args[*idx])),
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

        let arg_types = self.params.iter().map(|p| p.ffi_type());
        let result_ty = match &self.ret {
            Some(c) => c.ffi_type(),
            None => Type::void(),
        };
        let cif = Cif::new(arg_types, result_ty);
        let code = CodePtr::from_ptr(self.sym as *const c_void);

        // A by-value struct RETURN cannot use `middle::Cif::call::<R>` (it allocates a `MaybeUninit<R>`
        // for a statically-sized `R`); drop to the raw `ffi_call` with an own sized rvalue buffer.
        if let Some(CType::Struct {
            name,
            field_names,
            fields,
        }) = &self.ret
        {
            let r = self.call_struct_return(&cif, code, &ffi_args, name, field_names, fields);
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
        name: &str,
        field_names: &[String],
        fields: &[CType],
    ) -> Result<NativeRet, HostError> {
        let (_ty, size, align, offsets) = struct_layout(fields);
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

        // Read each field back at its libffi offset and widen to a NativeRet scalar. The name +
        // field_names make this a `NativeRet::Struct` the VM already lowers to a native struct.
        // SAFETY: byte view over the 8-aligned `u64` rvalue (same lifetime/owner); reads stay within
        // `words*8` bytes (every offset is `< size <= words*8`).
        let rbytes = unsafe { std::slice::from_raw_parts(rvalue.as_ptr() as *const u8, words * 8) };
        let out_fields: Vec<(String, NativeRet)> = field_names
            .iter()
            .zip(fields.iter())
            .zip(offsets.iter())
            .map(|((fname, ct), off)| (fname.clone(), read_field(rbytes, *off, ct)))
            .collect();
        Ok(NativeRet::Struct {
            name: name.to_string(),
            fields: out_fields,
        })
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
    }
}

impl std::fmt::Debug for Cffi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cffi")
            .field("name", &self.name)
            .field("params", &self.params)
            .field("ret", &self.ret)
            .finish()
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::native::{Host, HostError, NativeRet};

    /// Regression (callback SIGSEGV): `ffi_prep_closure_loc` stores a raw pointer to the callback
    /// `Cif`'s inner `ffi_cif`; libffi dereferences it when C later invokes the closure. The `Cif` is
    /// then moved into a `CallbackClosure` and into the `callback_closures` `Vec` (which can
    /// reallocate). A by-value `Cif` relocated → dangling pointer → SIGSEGV in libffi's
    /// `classify_argument` (layout-dependent, so it slipped past the 3-engine `ffi_qsort` goldens but
    /// crashed `chezzi run examples/ffi_qsort.chz`). The fix `Box`es the `Cif`; this pins the address
    /// across exactly those moves. A by-value `Cif` here makes this assertion fail.
    #[test]
    fn boxed_callback_cif_address_is_stable_across_moves() {
        // (1) Production tie-in: this only compiles while `CallbackClosure::_cif` derefs TWICE to a
        // `Cif` (i.e. is `ManuallyDrop<Box<Cif>>`, not a by-value `Cif` or a `ManuallyDrop<Cif>`).
        // If someone reverts the field to a bare `Cif`, `**c._cif` stops compiling and this
        // regression test breaks the build — the whole point. The pin now also covers the LEAKED
        // cif: `Drop` no longer frees the closure, so libffi may deref that `ffi_cif` long after
        // `Cffi::call` returned, and it must still sit at the address `ffi_prep_closure_loc` stored.
        fn _cif_is_heap_pinned(c: &CallbackClosure<'_>) -> *const Cif {
            &**c._cif
        }
        let _ = _cif_is_heap_pinned; // reference it so the compile-time guard isn't dead code
        // (2) The property that fix relies on: a `Box<Cif>` keeps its `ffi_cif` at a stable address
        // across the same moves `Cffi::call` performs (into `CallbackClosure`, into the reallocating
        // `callback_closures` Vec). libffi holds a raw pointer to that `ffi_cif` for the whole call.
        let cif = Box::new(Cif::new([Type::pointer(), Type::pointer()], Type::i32()));
        let raw_before = cif.as_raw_ptr();
        let mut closures: Vec<Box<Cif>> = Vec::with_capacity(0);
        for _ in 0..64 {
            closures.push(Box::new(Cif::new([Type::pointer()], Type::void())));
        }
        closures.push(cif);
        assert_eq!(
            raw_before,
            closures.last().unwrap().as_raw_ptr(),
            "the boxed callback Cif's ffi_cif must keep a stable address across the move into the \
             callback_closures Vec — libffi holds a raw pointer to it for the whole call"
        );
    }

    /// How a [`MockHost`] callback slot reacts when the C side invokes it (drives the callback
    /// trampoline tests without an engine): a pure scalar transform, an explicit `HostError`, or a
    /// Rust panic (to exercise the trampoline's `catch_unwind` + re-raise fault rule).
    enum CbBehavior {
        /// `f(int) -> int`: double the int arg, +1 (so the C fixture's `f(x)+1` is testable end-to-end).
        DoubleIntPlusOne,
        /// `f(double) -> double`: negate the float arg.
        NegateFloat,
        /// Always return an `Err(HostError)` — the trampoline must stash + re-raise it.
        ReturnsError,
        /// Panic in the callback body — `catch_unwind` must catch it and re-raise a fault, not abort.
        Panics,
    }

    /// A standalone `Host` over fixed args, for unit-testing the FFI marshalling in isolation.
    #[derive(Default)]
    struct MockHost {
        ints: Vec<i64>,
        floats: Vec<f64>,
        strs: Vec<String>,
        ptrs: Vec<usize>,
        /// Struct args: each is its fields as engine-neutral [`NativeRet`] scalars (declaration order).
        structs: Vec<Vec<NativeRet>>,
        /// Callback args: each is a [`CbBehavior`] the host's `invoke_callback` applies to the C args.
        callbacks: Vec<CbBehavior>,
        // Each arg names which vec + index it lives in.
        kinds: Vec<(char, usize)>,
    }

    impl MockHost {
        fn int(mut self, v: i64) -> Self {
            self.ints.push(v);
            self.kinds.push(('i', self.ints.len() - 1));
            self
        }
        fn float(mut self, v: f64) -> Self {
            self.floats.push(v);
            self.kinds.push(('f', self.floats.len() - 1));
            self
        }
        fn string(mut self, v: &str) -> Self {
            self.strs.push(v.to_string());
            self.kinds.push(('s', self.strs.len() - 1));
            self
        }
        fn ptr(mut self, v: usize) -> Self {
            self.ptrs.push(v);
            self.kinds.push(('p', self.ptrs.len() - 1));
            self
        }
        /// Push a by-value struct arg: its fields as engine-neutral scalars in declaration order.
        fn strukt(mut self, fields: Vec<NativeRet>) -> Self {
            self.structs.push(fields);
            self.kinds.push(('S', self.structs.len() - 1));
            self
        }
        /// Push a callback arg with the given reaction behavior.
        fn callback(mut self, b: CbBehavior) -> Self {
            self.callbacks.push(b);
            self.kinds.push(('C', self.callbacks.len() - 1));
            self
        }
    }

    impl Host for MockHost {
        fn arg_count(&self) -> usize {
            self.kinds.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            let (k, idx) = self.kinds[i];
            if k != 'i' {
                return Err(HostError {
                    message: format!("arg {i} is not an int"),
                });
            }
            Ok(self.ints[idx])
        }
        fn arg_is_int(&self, i: usize) -> bool {
            self.kinds[i].0 == 'i'
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.floats[idx])
        }
        fn arg_str(&mut self, i: usize) -> Result<String, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.strs[idx].clone())
        }
        fn arg_ptr(&mut self, i: usize) -> Result<usize, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.ptrs[idx])
        }
        fn arg_struct_fields(&mut self, i: usize) -> Result<Vec<NativeRet>, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.structs[idx].clone())
        }
        fn invoke_callback(
            &mut self,
            arg_index: usize,
            args: &[NativeRet],
        ) -> Result<NativeRet, HostError> {
            let (_, idx) = self.kinds[arg_index];
            match &self.callbacks[idx] {
                CbBehavior::DoubleIntPlusOne => match args.first() {
                    Some(NativeRet::Int(n)) => Ok(NativeRet::Int(n * 2 + 1)),
                    other => Err(HostError {
                        message: format!("callback expected int, got {other:?}"),
                    }),
                },
                CbBehavior::NegateFloat => match args.first() {
                    Some(NativeRet::Float(f)) => Ok(NativeRet::Float(-*f)),
                    other => Err(HostError {
                        message: format!("callback expected float, got {other:?}"),
                    }),
                },
                CbBehavior::ReturnsError => Err(HostError {
                    message: "callback deliberately failed".into(),
                }),
                CbBehavior::Panics => panic!("callback deliberately panicked"),
            }
        }
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError {
                message: "no map args".into(),
            })
        }
        fn write_stdout(&mut self, _s: &str) {}
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(None)
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<Vec<u8>, HostError> {
            Ok(b"/".to_vec())
        }
    }

    #[test]
    fn cos_of_zero_is_one() {
        let f = Cffi::new("libm.so.6", "cos", vec![CType::Float], Some(CType::Float))
            .expect("dlopen cos");
        let mut host = MockHost::default().float(0.0);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Float(1.0)));
    }

    #[test]
    fn sqrt_of_four_is_two() {
        let f = Cffi::new("libm.so.6", "sqrt", vec![CType::Float], Some(CType::Float))
            .expect("dlopen sqrt");
        let mut host = MockHost::default().float(4.0);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Float(2.0)));
    }

    #[test]
    fn strlen_of_hello_is_five() {
        let f = Cffi::new("libc.so.6", "strlen", vec![CType::Str], Some(CType::Int))
            .expect("dlopen strlen");
        let mut host = MockHost::default().string("hello");
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(5)));
    }

    #[test]
    fn null_str_return_is_a_fault_not_nil() {
        // `getenv` of an almost-certainly-unset var returns NULL. A `str`-typed return must NOT
        // silently become `nil` (that would break the static non-null `str` guarantee) — it faults.
        let f = Cffi::new("libc.so.6", "getenv", vec![CType::Str], Some(CType::Str))
            .expect("dlopen getenv");
        let mut host = MockHost::default().string("CHEZZI_DEFINITELY_UNSET_VAR_XYZ_42");
        let err = f
            .call(&mut host)
            .expect_err("NULL char* for a `str` return must fault");
        assert!(err.message.contains("returned NULL"), "{}", err.message);
    }

    #[test]
    fn nullable_str_return_null_is_none() {
        // `getenv` of an unset var returns NULL. Declared `str?` (OptStr), this must NOT fault —
        // it lowers to `None`, the whole point of the nullable opt-in.
        let f = Cffi::new("libc.so.6", "getenv", vec![CType::Str], Some(CType::OptStr))
            .expect("dlopen getenv");
        let mut host = MockHost::default().string("CHEZZI_DEFINITELY_UNSET_VAR_XYZ_42");
        assert_eq!(f.call(&mut host), Ok(NativeRet::None));
    }

    #[test]
    fn nullable_str_return_present_is_some() {
        // A SET env var, read back via getenv as `str?`, lowers to `Some("...")`. Set a uniquely
        // named var so the test is self-contained.
        // SAFETY: single-threaded test; the var name is unique to this test.
        unsafe { std::env::set_var("CHEZZI_FFI_OPTSTR_TEST", "present") };
        let f = Cffi::new("libc.so.6", "getenv", vec![CType::Str], Some(CType::OptStr))
            .expect("dlopen getenv");
        let mut host = MockHost::default().string("CHEZZI_FFI_OPTSTR_TEST");
        assert_eq!(
            f.call(&mut host),
            Ok(NativeRet::Some(Box::new(NativeRet::Str("present".into()))))
        );
    }

    #[test]
    fn owned_str_return_is_copied_and_freed() {
        // `strdup("hi") -> char*` returns a freshly malloc'd copy. Declared `owned_str`, the FFI
        // copies it into a Chezzi str AND frees the C buffer. Assert the value is correct; a loop of
        // strdup+free exercises that the freed buffer is reusable (no UAF / double-free abort).
        let dup = Cffi::new(
            "libc.so.6",
            "strdup",
            vec![CType::Str],
            Some(CType::OwnedStr),
        )
        .expect("dlopen strdup");
        for _ in 0..3 {
            let mut host = MockHost::default().string("hi");
            assert_eq!(dup.call(&mut host), Ok(NativeRet::Str("hi".into())));
        }
    }

    #[test]
    fn owned_nullable_str_null_is_none_no_free() {
        // `getenv` of an unset var returns NULL. Declared `owned_str?` (OptOwnedStr): NULL lowers to
        // `None` and frees nothing (free is skipped for NULL).
        let f = Cffi::new(
            "libc.so.6",
            "getenv",
            vec![CType::Str],
            Some(CType::OptOwnedStr),
        )
        .expect("dlopen getenv");
        let mut host = MockHost::default().string("CHEZZI_DEFINITELY_UNSET_VAR_XYZ_42");
        assert_eq!(f.call(&mut host), Ok(NativeRet::None));
    }

    #[test]
    fn tmpfile_then_fclose_roundtrips_an_opaque_handle() {
        // `tmpfile() -> FILE*` produces an opaque `ptr` handle (no args); `fclose(FILE*) -> int`
        // consumes it and returns 0. Exercises ptr-OUT (return) and ptr-IN (arg) in one round-trip.
        let open =
            Cffi::new("libc.so.6", "tmpfile", vec![], Some(CType::Ptr)).expect("dlopen tmpfile");
        let f = open.call(&mut MockHost::default()).expect("tmpfile call");
        let addr = match f {
            NativeRet::Ptr(a) => a,
            other => panic!("expected a ptr handle, got {other:?}"),
        };
        assert_ne!(addr, 0, "tmpfile should succeed given a writable temp dir");
        let close = Cffi::new("libc.so.6", "fclose", vec![CType::Ptr], Some(CType::Int))
            .expect("dlopen fclose");
        assert_eq!(
            close.call(&mut MockHost::default().ptr(addr)),
            Ok(NativeRet::Int(0))
        );
    }

    #[test]
    fn null_ptr_return_is_not_a_fault() {
        // `fopen` of a non-existent path returns a NULL `FILE*`. Unlike a `str` return (which faults
        // on NULL), a `ptr` return of NULL lowers to `Ptr(0)` — a legitimate "creation failed" signal
        // the program can test with `std.ffi.is_null` / `== std.ffi.null()`.
        let open = Cffi::new(
            "libc.so.6",
            "fopen",
            vec![CType::Str, CType::Str],
            Some(CType::Ptr),
        )
        .expect("dlopen fopen");
        let mut host = MockHost::default()
            .string("/nonexistent_dir_chezzi_xyz_42/nope")
            .string("r");
        assert_eq!(open.call(&mut host), Ok(NativeRet::Ptr(0)));
    }

    #[test]
    fn missing_library_is_an_error() {
        let err = Cffi::new("libdoesnotexist.so.999", "cos", vec![], None).unwrap_err();
        assert!(
            err.message.contains("cannot load library"),
            "{}",
            err.message
        );
    }

    #[test]
    fn missing_symbol_is_an_error() {
        let err = Cffi::new("libm.so.6", "no_such_symbol_xyz", vec![], None).unwrap_err();
        assert!(err.message.contains("not found"), "{}", err.message);
    }

    #[test]
    fn logical_libc_alias_loads() {
        assert!(Cffi::new("libc", "strlen", vec![CType::Str], Some(CType::Int)).is_ok());
    }

    #[test]
    fn logical_libm_alias_loads() {
        assert!(Cffi::new("libm", "sqrt", vec![CType::Float], Some(CType::Float)).is_ok());
    }

    #[test]
    fn non_alias_library_name_passes_through_verbatim() {
        let err = Cffi::new("libdoesnotexist.so.999", "cos", vec![], None).unwrap_err();
        assert!(
            err.message
                .contains("cannot load library 'libdoesnotexist.so.999'"),
            "{}",
            err.message
        );
    }

    #[test]
    fn alias_candidate_order_puts_the_versioned_soname_first() {
        assert!(resolve_lib_candidates("libapply.so").is_none());
        if cfg!(target_os = "linux") {
            assert_eq!(
                resolve_lib_candidates("libc"),
                Some(vec!["libc.so.6", "libc.so"])
            );
        }
    }

    #[test]
    fn cffi_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Cffi>();
    }

    // ---- Fixed-width integer marshalling (int8/.../uint64) ----

    #[test]
    fn int32_param_roundtrips() {
        // libc `abs(int)->int` declared with the fixed-width int32 marshalling type, in AND out.
        // abs(-5) == 5: exercises a signed int32 param and an int32 return, both round-trip to i64.
        let f = Cffi::new("libc.so.6", "abs", vec![CType::Int32], Some(CType::Int32))
            .expect("dlopen abs");
        let mut host = MockHost::default().int(-5);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(5)));
    }

    #[test]
    fn int8_param_truncates_large_value_c_cast_wraps() {
        // A Chezzi i64 too large for the C width TRUNCATES per a C cast (wrapping), never panics.
        // 255 (0xFF) as a signed `char` (int8) is -1; abs(-1) == 1. Declaring abs with int8 in AND
        // out proves the param cast wraps (255 -> -1) rather than saturating/erroring.
        let f = Cffi::new("libc.so.6", "abs", vec![CType::Int8], Some(CType::Int8))
            .expect("dlopen abs");
        let mut host = MockHost::default().int(255);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(1)));
    }

    #[test]
    fn int32_return_sign_extends_negative() {
        // `atoi("-1") -> int` returns the C int -1; declared int32 it must SIGN-extend back to the
        // Chezzi i64 -1 (not 0x00000000FFFFFFFF == 4294967295).
        let f = Cffi::new("libc.so.6", "atoi", vec![CType::Str], Some(CType::Int32))
            .expect("dlopen atoi");
        let mut host = MockHost::default().string("-1");
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(-1)));
    }

    // htonl's result is byte-order dependent (identity on big-endian), so these two oracles only
    // hold on little-endian targets — gate them rather than encode a wrong value elsewhere.
    #[test]
    #[cfg(target_endian = "little")]
    fn uint32_zero_extends_high_bit_in_and_out() {
        // `htonl(uint32)->uint32` converts host->network byte order. On little-endian Linux,
        // htonl(1) == 0x01000000 == 16777216, a value with bit 24 set. This exercises an unsigned
        // int32 param AND an unsigned int32 return that ZERO-extends to a positive i64.
        let f = Cffi::new(
            "libc.so.6",
            "htonl",
            vec![CType::UInt32],
            Some(CType::UInt32),
        )
        .expect("dlopen htonl");
        let mut host = MockHost::default().int(1);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(16777216)));
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn uint32_return_top_bit_is_positive_i64() {
        // htonl(0x80) == 0x80000000 == 2147483648 (> i32::MAX). As `uint32` it must ZERO-extend to a
        // positive i64 (2147483648), proving an unsigned return is NOT sign-extended into a negative.
        let f = Cffi::new(
            "libc.so.6",
            "htonl",
            vec![CType::UInt32],
            Some(CType::UInt32),
        )
        .expect("dlopen htonl");
        let mut host = MockHost::default().int(0x80);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(2147483648)));
    }

    // ---- Structs by value (flat scalar fields) ----

    /// A by-value struct RETURN: `div_t div(int numer, int denom)` returns a small POD struct by value
    /// (`{int quot; int rem;}` — two C `int` = int32). Exercises the struct-return rvalue buffer,
    /// libffi-computed field offsets, and 2×int32 alignment. `div(17, 5) == {3, 2}`.
    #[test]
    fn struct_return_div_roundtrips() {
        let div_t = CType::Struct {
            name: "DivT".to_string(),
            field_names: vec!["quot".to_string(), "rem".to_string()],
            fields: vec![CType::Int32, CType::Int32],
        };
        let f = Cffi::new(
            "libc.so.6",
            "div",
            vec![CType::Int32, CType::Int32],
            Some(div_t),
        )
        .expect("dlopen div");
        let mut host = MockHost::default().int(17).int(5);
        let ret = f.call(&mut host).expect("div call");
        assert_eq!(
            ret,
            NativeRet::Struct {
                name: "DivT".to_string(),
                fields: vec![
                    ("quot".to_string(), NativeRet::Int(3)),
                    ("rem".to_string(), NativeRet::Int(2)),
                ],
            }
        );
    }

    /// A mixed-field POD layout (a C `long`, a C `double`, a C `long`) — the alignment/padding case.
    /// There is no always-present pure libc fn taking such a struct by value, so this exercises the
    /// marshal machinery directly: build the libffi layout, write each field into the C-struct buffer
    /// at its computed offset (exactly what the `call` param path does), then read each field back at
    /// its offset and assert it equals the cast input — proving offsets + padding are handled.
    #[test]
    fn struct_param_mixed_fields_marshals() {
        let fields = vec![CType::Int, CType::Float, CType::Int];
        let (_ty, size, align, offsets) = struct_layout(&fields);
        assert_eq!(offsets.len(), 3);
        assert!(
            size >= 24,
            "mixed long/double/long POD is at least 24 bytes, got {size}"
        );
        assert_eq!(
            align, 8,
            "8-byte alignment forced by the double / long fields"
        );

        let mut buf = vec![0u8; size];
        write_field(&mut buf, offsets[0], &CType::Int, &NativeRet::Int(42)).unwrap();
        write_field(&mut buf, offsets[1], &CType::Float, &NativeRet::Float(2.5)).unwrap();
        write_field(&mut buf, offsets[2], &CType::Int, &NativeRet::Int(-7)).unwrap();

        assert_eq!(
            read_field(&buf, offsets[0], &CType::Int),
            NativeRet::Int(42)
        );
        assert_eq!(
            read_field(&buf, offsets[1], &CType::Float),
            NativeRet::Float(2.5)
        );
        assert_eq!(
            read_field(&buf, offsets[2], &CType::Int),
            NativeRet::Int(-7)
        );
    }

    /// A mixed fixed-width layout (int8, int32, float) round-trips through the buffer — exercises the
    /// sub-word field write/read at libffi offsets (a member is read at its real offset, not register-
    /// widened like a narrow *return*): a signed int8 sign-extends, an int32 fits, a float is exact.
    #[test]
    fn struct_fixed_width_fields_roundtrip() {
        let fields = vec![CType::Int8, CType::Int32, CType::Float];
        let (_ty, size, _align, offsets) = struct_layout(&fields);
        let mut buf = vec![0u8; size];
        write_field(&mut buf, offsets[0], &CType::Int8, &NativeRet::Int(255)).unwrap(); // -> -1
        write_field(&mut buf, offsets[1], &CType::Int32, &NativeRet::Int(-12345)).unwrap();
        write_field(&mut buf, offsets[2], &CType::Float, &NativeRet::Float(1.5)).unwrap();
        assert_eq!(
            read_field(&buf, offsets[0], &CType::Int8),
            NativeRet::Int(-1)
        );
        assert_eq!(
            read_field(&buf, offsets[1], &CType::Int32),
            NativeRet::Int(-12345)
        );
        assert_eq!(
            read_field(&buf, offsets[2], &CType::Float),
            NativeRet::Float(1.5)
        );
    }

    /// The struct PARAM marshal loop reads its fields through `Host::arg_struct_fields` in declaration
    /// order: prove the MockHost reader surfaces the fields in order (the cffi `call` loop then writes
    /// them into the C buffer at the libffi offsets, covered by `struct_param_mixed_fields_marshals`).
    #[test]
    fn struct_param_host_reads_fields_in_order() {
        let mut host = MockHost::default().strukt(vec![NativeRet::Int(3), NativeRet::Int(2)]);
        let fields = host.arg_struct_fields(0).unwrap();
        assert_eq!(fields, vec![NativeRet::Int(3), NativeRet::Int(2)]);
    }

    /// `bool` now means C `_Bool` (1 byte), not C `int` (4 bytes). Pin the libffi layout: a struct
    /// `[bool, int8]` must place the int8 at offset 1 (right after the 1-byte bool) and have total
    /// size 2 — not offset 4 / size 8, which is what the old `bool == c_int` lowering produced.
    /// (Failing-then-green: before the re-map, ffi_type(Bool) was Type::c_int() — offs[1]==4, size==8.)
    #[test]
    fn bool_marshals_as_one_byte_cbool() {
        let (_t, size, _a, offs) = struct_layout(&[CType::Bool, CType::Int8]);
        assert_eq!(offs[0], 0, "bool field at offset 0");
        assert_eq!(offs[1], 1, "int8 field directly after the 1-byte _Bool");
        assert_eq!(
            size, 2,
            "two 1-byte fields pack into 2 bytes (C _Bool, not int)"
        );
    }

    /// A struct `[bool, int8]` round-trips through the C-ABI buffer at the now-1-byte-bool offsets:
    /// write a `true` bool at offset 0 and a `7` int8 at offset 1, read both back. Before the re-map
    /// `write_field` wrote a 4-byte c_int for the bool, stomping the int8 that the new layout puts at
    /// offset 1 — so the round-trip read of the int8 (and the bool's stored width) diverged.
    #[test]
    fn struct_bool_field_marshals_one_byte() {
        let fields = vec![CType::Bool, CType::Int8];
        let (_ty, size, _align, offsets) = struct_layout(&fields);
        assert_eq!(offsets[1], 1, "int8 must sit at offset 1 (bool is 1 byte)");
        let mut buf = vec![0u8; size];
        write_field(&mut buf, offsets[0], &CType::Bool, &NativeRet::Bool(true)).unwrap();
        write_field(&mut buf, offsets[1], &CType::Int8, &NativeRet::Int(7)).unwrap();
        assert_eq!(
            read_field(&buf, offsets[0], &CType::Bool),
            NativeRet::Bool(true)
        );
        assert_eq!(
            read_field(&buf, offsets[1], &CType::Int8),
            NativeRet::Int(7)
        );
    }

    // ---- Sync scalar callbacks (callbacks #4) ----

    /// Compile a tiny C fixture to a `.so` in a unique temp dir and return its path. The fixture
    /// exports `int apply(int x, int (*f)(int)) { return f(x) + 1; }`,
    /// `double applyd(double x, double (*f)(double)) { return f(x); }`, and
    /// `int apply2(int (*f)(int), int x) { return f(x) + 1; }` (callback-FIRST, for the not-last
    /// arg path) — enough to round-trip an int and a float callback synchronously. Built with the
    /// system `cc` (the same toolchain the FFI
    /// tests already require: a unix LP64 host with libc/libm); a `cc` failure `panic!`s the test
    /// loudly rather than silently skipping (matching how `dlopen` failures are asserted, not skipped).
    fn build_callback_so() -> std::path::PathBuf {
        use std::io::Write;
        // A unique dir per call keeps parallel test threads from racing on the same file name.
        let dir = std::env::temp_dir().join(format!(
            "chezzi_cb_ffi_{}_{}",
            std::process::id(),
            CB_SO_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let csrc = dir.join("apply.c");
        let mut f = std::fs::File::create(&csrc).expect("create apply.c");
        f.write_all(
            br#"
#include <stdint.h>
int apply(int x, int (*f)(int)) { return f(x) + 1; }
double applyd(double x, double (*f)(double)) { return f(x); }
int apply2(int (*f)(int), int x) { return f(x) + 1; }
/* A small record returned BY POINTER, for the ffi deref tests. The fields are laid out at known C
   offsets: a at 0 (int32), b at 8 (int64, after padding), c at 16 (double). mkrec returns a pointer
   to a static instance the Chezzi side then reads field-by-field via the load builtins. */
struct R { int32_t a; int64_t b; double c; };
void* mkrec(void) { static struct R r = { -3, 70000, 2.5 }; return &r; }
"#,
        )
        .expect("write apply.c");
        drop(f);
        let so = dir.join("libapply.so");
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-o"])
            .arg(&so)
            .arg(&csrc)
            .status()
            .expect("spawn cc (a working C toolchain is required for the FFI callback tests)");
        assert!(status.success(), "cc failed to build the callback fixture");
        so
    }

    static CB_SO_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    #[test]
    fn callback_int_roundtrip() {
        // `apply(x, f) == f(x) + 1`, with `f` a Chezzi-side callback that computes `2*n + 1`.
        // apply(10, f) == (2*10 + 1) + 1 == 22.
        let so = build_callback_so();
        let f = Cffi::new(
            so.to_str().unwrap(),
            "apply",
            vec![
                CType::Int32,
                CType::Callback {
                    params: vec![CType::Int32],
                    ret: Box::new(CType::Int32),
                },
            ],
            Some(CType::Int32),
        )
        .expect("dlopen apply");
        let mut host = MockHost::default()
            .int(10)
            .callback(CbBehavior::DoubleIntPlusOne);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(22)));
    }

    #[test]
    fn callback_not_last_arg_roundtrip() {
        // Regression: a callback param that is NOT the final parameter. The host raw pointer baked
        // into the trampoline ctx is captured AFTER the whole arg loop (the last use of `host`), so
        // the trailing `host.arg_int` reborrow for `x` cannot invalidate it (Stacked/Tree Borrows
        // UB). Every other callback test puts the callback last, so this is the only one exercising
        // the not-last path. `apply2(f, 10) == f(10) + 1 == (2*10 + 1) + 1 == 22`.
        let so = build_callback_so();
        let f = Cffi::new(
            so.to_str().unwrap(),
            "apply2",
            vec![
                CType::Callback {
                    params: vec![CType::Int32],
                    ret: Box::new(CType::Int32),
                },
                CType::Int32,
            ],
            Some(CType::Int32),
        )
        .expect("dlopen apply2");
        let mut host = MockHost::default()
            .callback(CbBehavior::DoubleIntPlusOne)
            .int(10);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Int(22)));
    }

    #[test]
    fn callback_float_roundtrip() {
        // `applyd(x, f) == f(x)`, with `f` a callback that negates its float arg. applyd(2.5, f) == -2.5.
        let so = build_callback_so();
        let f = Cffi::new(
            so.to_str().unwrap(),
            "applyd",
            vec![
                CType::Float,
                CType::Callback {
                    params: vec![CType::Float],
                    ret: Box::new(CType::Float),
                },
            ],
            Some(CType::Float),
        )
        .expect("dlopen applyd");
        let mut host = MockHost::default()
            .float(2.5)
            .callback(CbBehavior::NegateFloat);
        assert_eq!(f.call(&mut host), Ok(NativeRet::Float(-2.5)));
    }

    #[test]
    fn callback_fault_is_reraised() {
        // A callback that returns Err must surface as `Cffi::call` returning that error (re-raised),
        // and the C side must have seen a defined (zeroed) return — no UB/abort.
        let so = build_callback_so();
        let f = Cffi::new(
            so.to_str().unwrap(),
            "apply",
            vec![
                CType::Int32,
                CType::Callback {
                    params: vec![CType::Int32],
                    ret: Box::new(CType::Int32),
                },
            ],
            Some(CType::Int32),
        )
        .expect("dlopen apply");
        let mut host = MockHost::default()
            .int(10)
            .callback(CbBehavior::ReturnsError);
        let err = f
            .call(&mut host)
            .expect_err("a failing callback must re-raise as the extern call's error");
        assert!(
            err.message.contains("callback deliberately failed"),
            "{}",
            err.message
        );
    }

    #[test]
    fn callback_panic_is_caught_and_reraised() {
        // A callback that PANICS must be caught by the trampoline's catch_unwind (no unwind into the
        // C frames / abort) and re-raised as the extern call's error.
        let so = build_callback_so();
        let f = Cffi::new(
            so.to_str().unwrap(),
            "apply",
            vec![
                CType::Int32,
                CType::Callback {
                    params: vec![CType::Int32],
                    ret: Box::new(CType::Int32),
                },
            ],
            Some(CType::Int32),
        )
        .expect("dlopen apply");
        let mut host = MockHost::default().int(10).callback(CbBehavior::Panics);
        let err = f
            .call(&mut host)
            .expect_err("a panicking callback must re-raise as the extern call's error");
        assert!(err.message.contains("callback panicked"), "{}", err.message);
    }

    #[test]
    fn callback_two_engine_parity() {
        // End-to-end: a real `.chz` program passing a Chezzi closure as a callback to a C fn.
        // `apply(10, n => n*n) == 10*10 + 1`. (Formerly run through both the serial and M:N VM
        // engines and cross-checked against each other; M:N is the only engine now, so one run
        // compared against the known-correct output is the whole test.)
        let so = build_callback_so();
        let src = format!(
            "extern \"{}\":\n    fn apply(x: int, f: fn(int) -> int) -> int\n\nprint(apply(10, fn(n: int) -> int: n * n))\n",
            so.to_str().unwrap()
        );
        let out = crate::vm::run_capture(&src).expect("run");
        assert_eq!(out, "101\n");
    }

    #[test]
    fn callback_float_two_engine_parity() {
        let so = build_callback_so();
        let src = format!(
            "extern \"{}\":\n    fn applyd(x: float, f: fn(float) -> float) -> float\n\nprint(applyd(2.5, fn(n: float) -> float: n + 1.0))\n",
            so.to_str().unwrap()
        );
        let out = crate::vm::run_capture(&src).expect("run");
        assert_eq!(out, "3.5\n");
    }

    #[test]
    fn callback_three_engine_parity() {
        // The M:N engine reuses the VM's invoke_value re-entry; a sync callback fires on the
        // calling worker thread (no cross-thread hand-off). (Formerly cross-checked against a
        // second serial/parallel run; M:N is the only engine now, so one run compared against the
        // known-correct output is the whole test.)
        let so = build_callback_so();
        let src = format!(
            "extern \"{}\":\n    fn apply(x: int, f: fn(int) -> int) -> int\n\nprint(apply(10, fn(n: int) -> int: n * n))\n",
            so.to_str().unwrap()
        );
        let out = crate::vm::run_capture(&src).expect("run");
        assert_eq!(out, "101\n");
    }

    /// Write a `.chz` program to a unique temp file (so `import std.ffi` resolves via the graph
    /// resolver — the standalone `run_capture` path can't resolve module-member calls).
    fn write_deref_chz(src: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "chezzi_ffideref_{}_{}.chz",
            std::process::id(),
            CB_SO_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, src).expect("write temp .chz");
        path
    }

    #[test]
    fn ffi_deref_load_two_engine_parity() {
        // End-to-end: a C fn returns a `ptr` to a struct { int32 a@0; int64 b@8; double c@16 }; the
        // Chezzi side reads each field via the ffi load builtins and prints them. This is the
        // end-to-end golden for the deref builtins (callbacks set the precedent: no examples/ file,
        // only the in-crate golden test — an examples/ golden would need `cc` at golden-test time).
        // (Formerly run through both the serial and M:N VM engines and cross-checked against each
        // other; M:N is the only engine now, so one run compared against the known-correct output
        // is the whole test.)
        let so = build_callback_so();
        let src = format!(
            "import std.ffi\n\
extern \"{}\":\n    fn mkrec() -> ptr\n\n\
p := mkrec()\n\
print(ffi.load_int32_at(p, 0))\n\
print(ffi.load_int64_at(p, 8))\n\
print(ffi.load_float_at(p, 16))\n",
            so.to_str().unwrap()
        );
        let entry = write_deref_chz(&src);
        let (out, _e, res, _) = crate::vm::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "faulted: {res:?}");
        assert_eq!(out, "-3\n70000\n2.5\n");
    }

    #[test]
    fn ffi_deref_store_then_load_two_engine_parity() {
        // Round-trip through C-owned memory: store into the static record's fields, then read back.
        // (mkrec returns the SAME static each call, so a store is observable on the next load.)
        let so = build_callback_so();
        let src = format!(
            "import std.ffi\n\
extern \"{}\":\n    fn mkrec() -> ptr\n\n\
p := mkrec()\n\
ffi.store_int32_at(p, 0, 99)\n\
ffi.store_float_at(p, 16, 1.5)\n\
print(ffi.load_int32_at(p, 0))\n\
print(ffi.load_float_at(p, 16))\n",
            so.to_str().unwrap()
        );
        let entry = write_deref_chz(&src);
        let (out, _e, res, _) = crate::vm::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "faulted: {res:?}");
        assert_eq!(out, "99\n1.5\n");
    }

    #[test]
    fn ffi_deref_three_engine_parity() {
        // The M:N engine reaches the deref builtins through the engine-neutral host path.
        // (mkrec's static is process-global; this test only reads.) (Formerly cross-checked
        // `run_file` against `run_file_with(entry, HostConfig::default())` — but `run_file` IS
        // verbatim `run_file_with(entry, HostConfig::default())` (`vm::run_file`'s doc), so that
        // was a tautology comparing a value to itself under two spellings; M:N is the only engine
        // now, so one run compared against the known-correct output is the whole test.)
        let so = build_callback_so();
        let src = format!(
            "import std.ffi\n\
extern \"{}\":\n    fn mkrec() -> ptr\n\n\
p := mkrec()\n\
print(ffi.load_int32_at(p, 0))\n",
            so.to_str().unwrap()
        );
        let entry = write_deref_chz(&src);
        let (out, _e, res, _) = crate::vm::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "faulted: {res:?}");
        assert_eq!(out, "-3\n");
    }

    /// C-buffer alloc layer end-to-end: `ffi.alloc` a buffer of N int64 slots, fill it from a Chezzi
    /// list via `store_int64_at`, read them back via `load_int64_at`, `ffi.free`. No `.so` needed
    /// (the buffer is process-local libc memory). Linux-gated like the other ffi goldens. (Formerly
    /// run through both the serial and M:N VM engines and cross-checked against each other; M:N is
    /// the only engine now, so one run compared against the known-correct output is the whole test.)
    #[test]
    #[cfg(target_os = "linux")]
    fn ffi_alloc_fill_read_two_engine_parity() {
        let src = concat!(
            "import std.ffi\n",
            "data := [10, 20, 30, 40]\n",
            "p := ffi.alloc(data.len() * 8)\n",
            "for i in range(data.len()):\n",
            "    ffi.store_int64_at(p, i * 8, data[i])\n",
            "for i in range(data.len()):\n",
            "    print(ffi.load_int64_at(p, i * 8))\n",
            "ffi.free(p)\n",
        );
        let entry = write_deref_chz(src);
        let (out, _e, res, _) = crate::vm::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "faulted: {res:?}");
        assert_eq!(out, "10\n20\n30\n40\n");
    }

    /// `alloc_zeroed` returns zeroed memory; reading before any store yields 0.
    #[test]
    #[cfg(target_os = "linux")]
    fn ffi_alloc_zeroed_two_engine_parity() {
        let src = concat!(
            "import std.ffi\n",
            "p := ffi.alloc_zeroed(32)\n",
            "for i in range(4):\n",
            "    print(ffi.load_int64_at(p, i * 8))\n",
            "ffi.free(p)\n",
        );
        let entry = write_deref_chz(src);
        let (out, _e, res, _) = crate::vm::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "faulted: {res:?}");
        assert_eq!(out, "0\n0\n0\n0\n");
    }

    /// The poison abort's only value is its message, and it goes out through a raw `write(2)` (Rust's
    /// stdio lock is not async-signal-safe). A single bare `write` drops it entirely on a
    /// non-blocking fd — an inherited-`O_NONBLOCK` tty, an editor/CI harness — leaving a bare SIGABRT
    /// with empty stderr, barely distinguishable from the SIGSEGV W6-8 replaced. `write_all_fd` must
    /// therefore ride out `EAGAIN` and short counts.
    #[test]
    #[cfg(target_os = "linux")]
    fn write_all_fd_delivers_through_a_full_nonblocking_fd() {
        let mut fds = [0i32; 2];
        // SAFETY: `pipe` fills the two-element array with the read/write fds.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe()");
        let (rfd, wfd) = (fds[0], fds[1]);
        // SAFETY: plain fcntl on our own fd — the inherited-O_NONBLOCK stderr case.
        unsafe { libc::fcntl(wfd, libc::F_SETFL, libc::O_NONBLOCK) };
        // Fill the pipe buffer so the next write is guaranteed to hit EAGAIN.
        let filler = [b'x'; 4096];
        loop {
            // SAFETY: writing our own buffer to our own fd.
            let n = unsafe { libc::write(wfd, filler.as_ptr() as *const c_void, filler.len()) };
            if n < 0 {
                break;
            }
        }
        let reader = std::thread::spawn(move || {
            // Stay blocked long enough that a non-retrying writer has already given up.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let mut buf = [0u8; 8192];
            let mut all: Vec<u8> = Vec::new();
            loop {
                // SAFETY: reading into our own buffer from our own fd.
                let n = unsafe { libc::read(rfd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                all.extend_from_slice(&buf[..n as usize]);
            }
            // SAFETY: closing our own fd, once.
            unsafe { libc::close(rfd) };
            all
        });
        write_all_fd(wfd, POISON_MSG);
        // SAFETY: closing our own fd, once — gives the reader its EOF.
        unsafe { libc::close(wfd) };
        let all = reader.join().expect("reader thread");
        assert!(
            all.ends_with(POISON_MSG),
            "the poison message must survive a full non-blocking fd; got {} bytes ending {:?}",
            all.len(),
            String::from_utf8_lossy(&all[all.len().saturating_sub(60)..]).into_owned()
        );
    }
}
