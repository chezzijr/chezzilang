//! Dynamic C-ABI FFI (v1): the runtime machinery behind an `extern "lib":` block. A [`Cffi`]
//! wraps a `dlopen`'d shared library, a resolved symbol address, and the C signature (as
//! [`CType`]s), and exposes `call(&mut dyn Host)` — reusing the engine-neutral [`Host`]/[`NativeRet`]
//! seam (`src/native/mod.rs`) so the VM and the frozen interpreter produce identical output.
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
    /// `struct` so a by-value RETURN lowers to a [`NativeRet::Struct`] both engines already build; the
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
}

impl CType {
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
fn write_field(buf: &mut [u8], offset: usize, ct: &CType, v: &NativeRet) -> Result<(), HostError> {
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
        // The checker rejects str / owned / opt / nested-struct fields, so they never reach here.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Struct { .. } => {
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
fn read_field(buf: &[u8], offset: usize, ct: &CType) -> NativeRet {
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
        // Unreachable for a well-typed struct (checker rejects str/owned/opt/nested); default to Nil.
        CType::Str
        | CType::OwnedStr
        | CType::OptStr
        | CType::OptOwnedStr
        | CType::Struct { .. } => NativeRet::Nil,
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
        let library = unsafe { Library::new(lib) }.map_err(|e| HostError {
            message: format!("cannot load library '{lib}': {e}"),
        })?;
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
    /// cached libc `free` (if resolved). The copy happens BEFORE the free, so the data is safe.
    /// SAFETY: `p` must be a non-null pointer to a NUL-terminated, malloc'd buffer (the `owned_str`
    /// user assertion across the C trust boundary). `free_addr`, if present, is the standard libc
    /// `void free(void*)`.
    unsafe fn copy_and_free_owned(&self, p: *const std::os::raw::c_char) -> String {
        let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
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
            return self.call_struct_return(&cif, code, &ffi_args, name, field_names, fields);
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
                    NativeRet::Str(CStr::from_ptr(p).to_string_lossy().into_owned())
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
                    NativeRet::Str(self.copy_and_free_owned(p))
                }
                Some(CType::OptStr) => {
                    // RETURN-ONLY nullable `char*`: NULL → None, non-null → Some(copied str). The
                    // borrowed (not freed) nullable opt-in (e.g. `getenv`).
                    let p: *const std::os::raw::c_char = cif.call(code, &ffi_args);
                    if p.is_null() {
                        NativeRet::None
                    } else {
                        NativeRet::Some(Box::new(NativeRet::Str(
                            CStr::from_ptr(p).to_string_lossy().into_owned(),
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
                        NativeRet::Some(Box::new(NativeRet::Str(self.copy_and_free_owned(p))))
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
                // A struct return was handled above (early return); a `None`/void return falls here.
                Some(CType::Struct { .. }) | None => {
                    let _: () = cif.call(code, &ffi_args);
                    NativeRet::Nil
                }
            }
        };
        // `cstrings` / `struct_bufs` (and the other storage) are dropped here, after the call returns
        // — never before.
        drop(cstrings);
        drop(struct_bufs);
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
        // field_names make this a `NativeRet::Struct` both engines already lower to a native struct.
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

    /// A standalone `Host` over fixed args, for unit-testing the FFI marshalling in isolation.
    #[derive(Default)]
    struct MockHost {
        ints: Vec<i64>,
        floats: Vec<f64>,
        strs: Vec<String>,
        ptrs: Vec<usize>,
        /// Struct args: each is its fields as engine-neutral [`NativeRet`] scalars (declaration order).
        structs: Vec<Vec<NativeRet>>,
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
        fn os_getcwd(&self) -> Result<String, HostError> {
            Ok("/".into())
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
}
