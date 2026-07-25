//! `std.ffi` — the small C-ABI vocabulary that pairs with the opaque `ptr` handle type used by
//! `extern "lib":` blocks (`src/native/cffi.rs`).
//!
//! The `ptr` *type* is a builtin marshalling primitive (a peer of `int`/`float`/`bool`/`str`, so it
//! can be named in an `extern` signature with no import). This module supplies the *values/helpers*
//! that operate on it — keeping the C vocabulary in the library, never the language (no new
//! keyword/literal). Today: the NULL sentinel and a null test.
//!
//! - `null() -> ptr` — the NULL pointer (address `0`). A handle-creating C fn that fails typically
//!   returns NULL; compare against this (or use [`is_null`]) to detect it.
//! - `is_null(p: ptr) -> bool` — `true` iff `p` is the NULL pointer.
//!
//! A `ptr` is opaque, untyped, and never auto-freed: call the library's own destroy
//! (e.g. `fclose`) explicitly. See `docs/spec.md` §Level-3 FFI.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};

/// Resolve the base address of a deref builtin's `ptr` arg (`args[0]`), failing with a *recoverable*
/// [`HostError`] (NOT a segfault) when it is the NULL pointer. This is the one cheaply-checkable
/// safety guard the deref surface offers: only address `0` can be detected — a dangling/misaligned/
/// out-of-bounds *non-null* pointer still faults (inherent, documented like ctypes UB). `name` is the
/// `ffi.<fn>` label for the error message.
#[cfg(unix)]
fn base_addr(h: &mut dyn Host, name: &str) -> Result<usize, HostError> {
    let addr = h.arg_ptr(0)?;
    if addr == 0 {
        return Err(HostError {
            message: format!("ffi.{name}: null pointer"),
        });
    }
    Ok(addr)
}

/// Read a C scalar of `ct`'s natural width at `addr + off`, reusing the EXACT sign/zero-extend rules
/// already tested in [`cffi::read_field`] (the struct-field reader). Builds a transient byte slice over
/// the raw pointer at the element width and delegates — no rule is re-derived here.
#[cfg(unix)]
fn load_scalar(addr: usize, off: usize, ct: &super::cffi::CType) -> NativeRet {
    let width = ctype_width(ct);
    // SAFETY: `addr` is non-null (checked by `base_addr`) and C-sourced (a `ptr` is opaque and cannot
    // be forged from an int — only an extern return / callback arg / `ffi.null()` produces one). The
    // caller's `off` + `width` must stay within the C-owned allocation; an out-of-bounds or dangling
    // pointer is undefined behavior (documented, ctypes-equivalent — not cheaply checkable). The slice
    // is read-only and lives only for this call; `read_field` copies the bytes out (no aliasing held).
    let slice = unsafe { std::slice::from_raw_parts((addr + off) as *const u8, width) };
    super::cffi::read_field(slice, 0, ct)
}

/// The natural C width (bytes) of a scalar [`cffi::CType`] — the element size to map over the raw
/// pointer. Mirrors the widths `read_field`/`write_field` read/write (NOT register width).
#[cfg(unix)]
fn ctype_width(ct: &super::cffi::CType) -> usize {
    use super::cffi::CType;
    match ct {
        CType::Int => std::mem::size_of::<std::os::raw::c_long>(),
        CType::Int8 | CType::UInt8 | CType::Bool => 1,
        CType::Int16 | CType::UInt16 => 2,
        CType::Int32 | CType::UInt32 => 4,
        CType::Int64 | CType::UInt64 => 8,
        CType::Float => 8,
        CType::Ptr => std::mem::size_of::<usize>(),
        // Non-scalar variants never reach the deref builtins (no public fn maps to them).
        _ => 0,
    }
}

/// The NULL pointer sentinel (`Ptr(0)`).
fn null(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "null", 0)?;
    Ok(NativeRet::Ptr(0))
}

/// Whether a `ptr` handle is NULL (address `0`).
fn is_null(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_null", 1)?;
    Ok(NativeRet::Bool(h.arg_ptr(0)? == 0))
}

// ---------------------------------------------------------------------------------------------
// C-buffer alloc layer — malloc/calloc/free-backed raw buffers for handing C array/buffer APIs
// (qsort, bsearch, fread-into-buffer, …). Fill/read with the load_*/store_* deref builtins above.
//
// ALLOCATOR: these call the LIBC allocator (`malloc`/`calloc`/`free`), NOT Rust's `GlobalAlloc`,
// so a buffer may be handed to a C fn that reallocs/frees it and it pairs with the same allocator
// the rest of `cffi` uses (the `owned_str` return path already frees `malloc`'d memory with C
// `free`). malloc/calloc/free are unconditionally linked libc on every supported unix target
// (the existing strlen/div/srand libc.so.6 FFI tests already prove libc symbols are in scope), so
// the extern decls resolve at link time with zero per-call dlsym/libffi overhead.
//
// MANUAL FREE: a `ptr` is never auto-freed (consistent with the FFI-ptr rule). The idiom is
// `defer ffi.free(p)`. Forgetting to free is a leak. Double-free / use-after-free / out-of-bounds
// store_/load_ beyond the allocation are the user's responsibility (an inherently unsafe surface,
// documented like ctypes — no bounds/lifetime tracking).
//
// Non-unix: the same names are registered but every call returns a `HostError` (mirrors the
// load_*/store_* `deref_unsupported` cfg pattern).
// ---------------------------------------------------------------------------------------------

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "malloc"]
    fn c_malloc(size: usize) -> *mut std::os::raw::c_void;
    #[link_name = "calloc"]
    fn c_calloc(nmemb: usize, size: usize) -> *mut std::os::raw::c_void;
    #[link_name = "free"]
    fn c_free(ptr: *mut std::os::raw::c_void);
}

/// `alloc(nbytes: int) -> ptr` — `malloc(nbytes)`; the bytes are GARBAGE (uninitialized). Faults
/// recoverably on a negative size or out-of-memory; never segfaults/aborts. `nbytes == 0` passes
/// through to `malloc(0)` (impl-defined: NULL or a unique ptr — not special-cased).
fn alloc(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "alloc", 1)?;
    #[cfg(unix)]
    {
        let n = h.arg_int(0)?;
        if n < 0 {
            return Err(HostError {
                message: "ffi.alloc: negative size".into(),
            });
        }
        // SAFETY: `malloc` is libc's allocator (always linked); it takes a byte count and returns
        // either a valid pointer to `n` uninitialized bytes or NULL. No memory is dereferenced here
        // — only the returned address is captured as an opaque `ptr`. Pairs with `ffi.free`.
        let p = unsafe { c_malloc(n as usize) };
        // OOM only when n > 0 (a legitimate NULL from malloc(0) is impl-defined, not an error).
        if p.is_null() && n > 0 {
            return Err(HostError {
                message: "ffi.alloc: out of memory".into(),
            });
        }
        Ok(NativeRet::Ptr(p as usize))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("alloc")
    }
}

/// `alloc_zeroed(nbytes: int) -> ptr` — `calloc`-style; the bytes are ZEROED. Same recoverable
/// faults as `alloc` (negative size, out-of-memory). `nbytes == 0` passes through to `calloc(0, 1)`.
fn alloc_zeroed(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "alloc_zeroed", 1)?;
    #[cfg(unix)]
    {
        let n = h.arg_int(0)?;
        if n < 0 {
            return Err(HostError {
                message: "ffi.alloc_zeroed: negative size".into(),
            });
        }
        // SAFETY: `calloc` is libc's allocator (always linked); `calloc(n, 1)` returns either a
        // valid pointer to `n` zeroed bytes or NULL. No memory is dereferenced here. Pairs with
        // `ffi.free`.
        let p = unsafe { c_calloc(n as usize, 1) };
        if p.is_null() && n > 0 {
            return Err(HostError {
                message: "ffi.alloc_zeroed: out of memory".into(),
            });
        }
        Ok(NativeRet::Ptr(p as usize))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("alloc_zeroed")
    }
}

/// `free(p: ptr)` — `free(p)`; returns nil. `free(ffi.null())` (address 0) is a safe no-op (C
/// `free(NULL)` is a defined no-op) — it does NOT route through `base_addr`. Double-free / freeing
/// a non-`ffi.alloc`'d pointer is the user's responsibility (undefined behavior, documented).
fn free(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "free", 1)?;
    #[cfg(unix)]
    {
        let addr = h.arg_ptr(0)?;
        if addr == 0 {
            // C free(NULL) is a defined no-op — return nil without calling free.
            return Ok(NativeRet::Nil);
        }
        // SAFETY: `addr` is a non-null C-sourced `ptr` (a `ptr` cannot be forged from an int). The
        // caller guarantees it was returned by `ffi.alloc`/`ffi.alloc_zeroed` (or a paired libc
        // allocator) and is freed at most once — a double-free / freeing a foreign pointer is
        // documented UB, the same contract as C `free`. `free` is libc's allocator.
        unsafe { c_free(addr as *mut std::os::raw::c_void) };
        Ok(NativeRet::Nil)
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("free")
    }
}

// ---------------------------------------------------------------------------------------------
// Memory deref builtins — read/write the C-owned memory behind an opaque `ptr` (load_*/store_*).
//
// SAFETY (the whole surface): these read/write ARBITRARY memory through a C-sourced address, so a
// bad pointer segfaults — an inherently unsafe surface, like Python `ctypes`. Mitigation Chezzi has
// that ctypes lacks: `ptr` is opaque and CANNOT be forged from an int (only an extern return, a
// callback arg, or `ffi.null()` yields one — provenance is C-sourced). The only cheaply-checkable
// guard is the NULL (address `0`) base pointer, which every fn rejects with a recoverable
// `HostError` BEFORE any deref; a dangling/misaligned/out-of-bounds *non-null* pointer is undefined
// behavior (documented limit). These run only on unix (where `extern`/`cffi` exist); a non-unix
// build registers the same names but every call returns a `HostError`.
// ---------------------------------------------------------------------------------------------

/// On non-unix targets every deref builtin returns this error (extern/cffi are unix-only).
#[cfg(not(unix))]
fn deref_unsupported(name: &str) -> Result<NativeRet, HostError> {
    Err(HostError {
        message: format!("ffi.{name}: FFI memory deref is only supported on unix"),
    })
}

/// Define a `load_<suffix>` (offset 0) + `load_<suffix>_at(p, off)` pair that reads `$ct` at the C
/// address, reusing `cffi::read_field`'s exact extend rules via [`load_scalar`].
macro_rules! load_fn {
    ($base:ident, $at:ident, $name:literal, $at_name:literal, $ct:expr) => {
        #[doc = concat!("`", $name, "(p) -> int/float/bool/ptr` — read the C value at `p` (offset 0).")]
        fn $base(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            expect_args(h, $name, 1)?;
            #[cfg(unix)]
            {
                let addr = base_addr(h, $name)?;
                Ok(load_scalar(addr, 0, &$ct))
            }
            #[cfg(not(unix))]
            {
                let _ = h;
                deref_unsupported($name)
            }
        }
        #[doc = concat!("`", $at_name, "(p, off) -> …` — read the C value at byte offset `off`.")]
        fn $at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            expect_args(h, $at_name, 2)?;
            #[cfg(unix)]
            {
                let addr = base_addr(h, $at_name)?;
                let off = h.arg_int(1)?;
                if off < 0 {
                    return Err(HostError {
                        message: format!("ffi.{}: negative offset", $at_name),
                    });
                }
                Ok(load_scalar(addr, off as usize, &$ct))
            }
            #[cfg(not(unix))]
            {
                let _ = h;
                deref_unsupported($at_name)
            }
        }
    };
}

load_fn!(
    load_int,
    load_int_at,
    "load_int",
    "load_int_at",
    super::cffi::CType::Int
);
load_fn!(
    load_int8,
    load_int8_at,
    "load_int8",
    "load_int8_at",
    super::cffi::CType::Int8
);
load_fn!(
    load_int16,
    load_int16_at,
    "load_int16",
    "load_int16_at",
    super::cffi::CType::Int16
);
load_fn!(
    load_int32,
    load_int32_at,
    "load_int32",
    "load_int32_at",
    super::cffi::CType::Int32
);
load_fn!(
    load_int64,
    load_int64_at,
    "load_int64",
    "load_int64_at",
    super::cffi::CType::Int64
);
load_fn!(
    load_uint8,
    load_uint8_at,
    "load_uint8",
    "load_uint8_at",
    super::cffi::CType::UInt8
);
load_fn!(
    load_uint16,
    load_uint16_at,
    "load_uint16",
    "load_uint16_at",
    super::cffi::CType::UInt16
);
load_fn!(
    load_uint32,
    load_uint32_at,
    "load_uint32",
    "load_uint32_at",
    super::cffi::CType::UInt32
);
load_fn!(
    load_uint64,
    load_uint64_at,
    "load_uint64",
    "load_uint64_at",
    super::cffi::CType::UInt64
);
load_fn!(
    load_float,
    load_float_at,
    "load_float",
    "load_float_at",
    super::cffi::CType::Float
);
load_fn!(
    load_bool,
    load_bool_at,
    "load_bool",
    "load_bool_at",
    super::cffi::CType::Bool
);
load_fn!(
    load_ptr,
    load_ptr_at,
    "load_ptr",
    "load_ptr_at",
    super::cffi::CType::Ptr
);

// load_float32 is special: `read_field` has no f32 arm, so it reads 4 bytes as f32 then widens to
// the Chezzi `float` (f64). Defined by hand (with its own SAFETY comment) rather than the macro.
#[cfg(unix)]
fn load_float32_impl(addr: usize, off: usize) -> NativeRet {
    // SAFETY: `addr` is non-null (base_addr) and C-sourced; the caller guarantees 4 readable bytes at
    // `addr + off` lie within the C allocation (out-of-bounds is documented UB). `read_unaligned`
    // tolerates any alignment; the bytes are copied out into an `f32` (no reference held).
    let f = unsafe { ((addr + off) as *const f32).read_unaligned() };
    NativeRet::Float(f as f64)
}

/// `load_float32(p) -> float` — read a C `float` (4 bytes) and widen to Chezzi `float`.
fn load_float32(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "load_float32", 1)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "load_float32")?;
        Ok(load_float32_impl(addr, 0))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("load_float32")
    }
}

/// `load_float32_at(p, off) -> float` — read a C `float` at byte offset `off`.
fn load_float32_at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "load_float32_at", 2)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "load_float32_at")?;
        let off = h.arg_int(1)?;
        if off < 0 {
            return Err(HostError {
                message: "ffi.load_float32_at: negative offset".into(),
            });
        }
        Ok(load_float32_impl(addr, off as usize))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("load_float32_at")
    }
}

/// `load_str(p) -> str` — copy the NUL-terminated C string at `p` into a Chezzi `str`. The buffer is
/// NOT freed (it is borrowed). Precondition: `p` points at a well-formed, NUL-terminated C string —
/// there is no max-length cap, so a non-terminated buffer reads until it faults or finds a stray NUL.
fn load_str(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "load_str", 1)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "load_str")?;
        load_str_impl(addr, 0, "ffi.load_str")
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("load_str")
    }
}

/// `load_str_at(p, off) -> str` — copy the NUL-terminated C string at byte offset `off`.
fn load_str_at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "load_str_at", 2)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "load_str_at")?;
        let off = h.arg_int(1)?;
        if off < 0 {
            return Err(HostError {
                message: "ffi.load_str_at: negative offset".into(),
            });
        }
        load_str_impl(addr, off as usize, "ffi.load_str_at")
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("load_str_at")
    }
}

/// The one wording for "a C string crossing into Chezzi is not UTF-8". A Chezzi `str` IS UTF-8, so a
/// non-UTF-8 C buffer is a clean fault, never a silently-mangled string — the same contract
/// `Socket.read` already holds (a binary payload is an `Err`, never silent U+FFFD). `load_uint8_at`
/// is the raw-byte escape hatch.
pub(crate) fn non_utf8_err(what: &str, e: std::str::Utf8Error) -> HostError {
    HostError {
        message: format!(
            "{what}: C string is not valid UTF-8 (first bad byte at offset {}); read the raw bytes \
             with ffi.load_uint8_at",
            e.valid_up_to()
        ),
    }
}

#[cfg(unix)]
fn load_str_impl(addr: usize, off: usize, what: &str) -> Result<NativeRet, HostError> {
    // SAFETY: `addr` is non-null (base_addr) and C-sourced; the caller guarantees a NUL-terminated C
    // string begins at `addr + off` within the C allocation. `CStr::from_ptr` scans to the first NUL
    // (no max-length cap — a non-terminated buffer is documented UB); `to_str` VALIDATES the bytes
    // (a non-UTF-8 buffer faults instead of mangling to U+FFFD) and `to_owned` copies them into an
    // owned `String` (the C buffer is borrowed, never freed).
    let s = unsafe {
        std::ffi::CStr::from_ptr((addr + off) as *const std::os::raw::c_char)
            .to_str()
            .map_err(|e| non_utf8_err(what, e))?
            .to_owned()
    };
    Ok(NativeRet::Str(s))
}

// --- STORE: write a Chezzi value into C-owned memory at the pointer's natural C width. ---

/// Read a store builtin's value arg (last position) into a [`NativeRet`] matching `ct`'s kind, for
/// [`cffi::write_field`] to cast to the C width. `vi` is the value arg index (1 base form, 2 `_at`).
#[cfg(unix)]
fn store_value(
    h: &mut dyn Host,
    vi: usize,
    ct: &super::cffi::CType,
) -> Result<NativeRet, HostError> {
    use super::cffi::CType;
    Ok(match ct {
        CType::Float => NativeRet::Float(h.arg_float(vi)?),
        CType::Bool => NativeRet::Bool(h.arg_bool(vi)?),
        CType::Ptr => NativeRet::Ptr(h.arg_ptr(vi)?),
        // every integer width (Int + fixed widths) reads an i64; write_field truncates to the C width.
        _ => NativeRet::Int(h.arg_int(vi)?),
    })
}

/// Write `val` (`ct`'s natural C width) into the C memory at `addr + off`, reusing
/// [`cffi::write_field`]'s exact truncation rules via a transient mutable byte slice.
#[cfg(unix)]
fn store_scalar(
    addr: usize,
    off: usize,
    ct: &super::cffi::CType,
    val: &NativeRet,
) -> Result<NativeRet, HostError> {
    let width = ctype_width(ct);
    // SAFETY: `addr` is non-null (base_addr) and C-sourced; the caller guarantees `width` bytes at
    // `addr + off` are writable C-owned memory (out-of-bounds is documented UB). The mutable slice
    // lives only for this call and no other reference aliases it; `write_field` copies the bytes in.
    let slice = unsafe { std::slice::from_raw_parts_mut((addr + off) as *mut u8, width) };
    super::cffi::write_field(slice, 0, ct, val)?;
    Ok(NativeRet::Nil)
}

/// Define a `store_<suffix>(p, v)` (offset 0) + `store_<suffix>_at(p, off, v)` pair. The `_at` form
/// takes the offset BEFORE the value. Both write at `$ct`'s natural C width and return `nil`.
macro_rules! store_fn {
    ($base:ident, $at:ident, $name:literal, $at_name:literal, $ct:expr) => {
        #[doc = concat!("`", $name, "(p, v)` — write `v` to the C value at `p` (offset 0); returns nil.")]
        fn $base(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            expect_args(h, $name, 2)?;
            #[cfg(unix)]
            {
                let addr = base_addr(h, $name)?;
                let val = store_value(h, 1, &$ct)?;
                store_scalar(addr, 0, &$ct, &val)
            }
            #[cfg(not(unix))]
            {
                let _ = h;
                deref_unsupported($name)
            }
        }
        #[doc = concat!("`", $at_name, "(p, off, v)` — write `v` at byte offset `off`; returns nil.")]
        fn $at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            expect_args(h, $at_name, 3)?;
            #[cfg(unix)]
            {
                let addr = base_addr(h, $at_name)?;
                let off = h.arg_int(1)?;
                if off < 0 {
                    return Err(HostError {
                        message: format!("ffi.{}: negative offset", $at_name),
                    });
                }
                let val = store_value(h, 2, &$ct)?;
                store_scalar(addr, off as usize, &$ct, &val)
            }
            #[cfg(not(unix))]
            {
                let _ = h;
                deref_unsupported($at_name)
            }
        }
    };
}

store_fn!(
    store_int,
    store_int_at,
    "store_int",
    "store_int_at",
    super::cffi::CType::Int
);
store_fn!(
    store_int8,
    store_int8_at,
    "store_int8",
    "store_int8_at",
    super::cffi::CType::Int8
);
store_fn!(
    store_int16,
    store_int16_at,
    "store_int16",
    "store_int16_at",
    super::cffi::CType::Int16
);
store_fn!(
    store_int32,
    store_int32_at,
    "store_int32",
    "store_int32_at",
    super::cffi::CType::Int32
);
store_fn!(
    store_int64,
    store_int64_at,
    "store_int64",
    "store_int64_at",
    super::cffi::CType::Int64
);
store_fn!(
    store_uint8,
    store_uint8_at,
    "store_uint8",
    "store_uint8_at",
    super::cffi::CType::UInt8
);
store_fn!(
    store_uint16,
    store_uint16_at,
    "store_uint16",
    "store_uint16_at",
    super::cffi::CType::UInt16
);
store_fn!(
    store_uint32,
    store_uint32_at,
    "store_uint32",
    "store_uint32_at",
    super::cffi::CType::UInt32
);
store_fn!(
    store_uint64,
    store_uint64_at,
    "store_uint64",
    "store_uint64_at",
    super::cffi::CType::UInt64
);
store_fn!(
    store_bool,
    store_bool_at,
    "store_bool",
    "store_bool_at",
    super::cffi::CType::Bool
);
store_fn!(
    store_ptr,
    store_ptr_at,
    "store_ptr",
    "store_ptr_at",
    super::cffi::CType::Ptr
);

// store_float / store_float32 are special: write_field has no f32 arm; float32 hand-writes 4 bytes.
#[cfg(unix)]
fn store_float_impl(addr: usize, off: usize, f: f64) -> NativeRet {
    // SAFETY: `addr` non-null + C-sourced; caller guarantees 8 writable bytes at `addr + off`.
    unsafe { ((addr + off) as *mut f64).write_unaligned(f) };
    NativeRet::Nil
}

#[cfg(unix)]
fn store_float32_impl(addr: usize, off: usize, f: f64) -> NativeRet {
    // SAFETY: `addr` non-null + C-sourced; caller guarantees 4 writable bytes at `addr + off`. The f64
    // narrows to f32 (C `float`) via `as` — the same cast a C `float` param would apply.
    unsafe { ((addr + off) as *mut f32).write_unaligned(f as f32) };
    NativeRet::Nil
}

/// `store_float(p, v)` — write a C `double` (8 bytes) at `p`; returns nil.
fn store_float(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "store_float", 2)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "store_float")?;
        let f = h.arg_float(1)?;
        Ok(store_float_impl(addr, 0, f))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("store_float")
    }
}

/// `store_float_at(p, off, v)` — write a C `double` at byte offset `off`; returns nil.
fn store_float_at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "store_float_at", 3)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "store_float_at")?;
        let off = h.arg_int(1)?;
        if off < 0 {
            return Err(HostError {
                message: "ffi.store_float_at: negative offset".into(),
            });
        }
        let f = h.arg_float(2)?;
        Ok(store_float_impl(addr, off as usize, f))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("store_float_at")
    }
}

/// `store_float32(p, v)` — narrow `v` to a C `float` (4 bytes) and write at `p`; returns nil.
fn store_float32(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "store_float32", 2)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "store_float32")?;
        let f = h.arg_float(1)?;
        Ok(store_float32_impl(addr, 0, f))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("store_float32")
    }
}

/// `store_float32_at(p, off, v)` — narrow `v` to a C `float` and write at byte offset `off`; nil.
fn store_float32_at(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "store_float32_at", 3)?;
    #[cfg(unix)]
    {
        let addr = base_addr(h, "store_float32_at")?;
        let off = h.arg_int(1)?;
        if off < 0 {
            return Err(HostError {
                message: "ffi.store_float32_at: negative offset".into(),
            });
        }
        let f = h.arg_float(2)?;
        Ok(store_float32_impl(addr, off as usize, f))
    }
    #[cfg(not(unix))]
    {
        let _ = h;
        deref_unsupported("store_float32_at")
    }
}

/// The callable members of `std.ffi`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("null", null),
    ("is_null", is_null),
    // --- loads (base form = offset 0; `_at` form takes a byte offset) ---
    ("load_int", load_int),
    ("load_int_at", load_int_at),
    ("load_int8", load_int8),
    ("load_int8_at", load_int8_at),
    ("load_int16", load_int16),
    ("load_int16_at", load_int16_at),
    ("load_int32", load_int32),
    ("load_int32_at", load_int32_at),
    ("load_int64", load_int64),
    ("load_int64_at", load_int64_at),
    ("load_uint8", load_uint8),
    ("load_uint8_at", load_uint8_at),
    ("load_uint16", load_uint16),
    ("load_uint16_at", load_uint16_at),
    ("load_uint32", load_uint32),
    ("load_uint32_at", load_uint32_at),
    ("load_uint64", load_uint64),
    ("load_uint64_at", load_uint64_at),
    ("load_float", load_float),
    ("load_float_at", load_float_at),
    ("load_float32", load_float32),
    ("load_float32_at", load_float32_at),
    ("load_bool", load_bool),
    ("load_bool_at", load_bool_at),
    ("load_ptr", load_ptr),
    ("load_ptr_at", load_ptr_at),
    ("load_str", load_str),
    ("load_str_at", load_str_at),
    // --- stores (base form = offset 0; `_at` form takes the byte offset BEFORE the value) ---
    ("store_int", store_int),
    ("store_int_at", store_int_at),
    ("store_int8", store_int8),
    ("store_int8_at", store_int8_at),
    ("store_int16", store_int16),
    ("store_int16_at", store_int16_at),
    ("store_int32", store_int32),
    ("store_int32_at", store_int32_at),
    ("store_int64", store_int64),
    ("store_int64_at", store_int64_at),
    ("store_uint8", store_uint8),
    ("store_uint8_at", store_uint8_at),
    ("store_uint16", store_uint16),
    ("store_uint16_at", store_uint16_at),
    ("store_uint32", store_uint32),
    ("store_uint32_at", store_uint32_at),
    ("store_uint64", store_uint64),
    ("store_uint64_at", store_uint64_at),
    ("store_float", store_float),
    ("store_float_at", store_float_at),
    ("store_float32", store_float32),
    ("store_float32_at", store_float32_at),
    ("store_bool", store_bool),
    ("store_bool_at", store_bool_at),
    ("store_ptr", store_ptr),
    ("store_ptr_at", store_ptr_at),
    // --- C-buffer alloc layer (libc malloc/calloc/free; MANUAL free — `defer ffi.free(p)`) ---
    ("alloc", alloc),
    ("alloc_zeroed", alloc_zeroed),
    ("free", free),
];

/// The fixed-width C-ABI integer *type* names that `std.ffi` exports (Chezzi's first type imports).
/// Each maps 1:1 to a C `int{N}_t`/`uint{N}_t` and is recognized by the checker (resolving to a plain
/// `Ty::Int`) ONLY in a module that imports it per-name (`import int32, uint32 from std.ffi`), exactly
/// like the callable `MEMBERS` above. The width/signedness is a runtime-only marshalling distinction
/// the backends recover via `ctype_of` — these names are NOT bound as callable values. This list is
/// the single declaring authority; the checker reads it (see `native_module_sig` + `resolve_type`).
pub const TYPE_NAMES: &[&str] = &[
    "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A standalone `Host` serving a single `ptr` arg, for testing `is_null` in isolation.
    #[derive(Default)]
    struct PtrHost {
        ptrs: Vec<usize>,
    }

    impl Host for PtrHost {
        fn arg_count(&self) -> usize {
            self.ptrs.len()
        }
        fn arg_int(&mut self, _i: usize) -> Result<i64, HostError> {
            Err(HostError {
                message: "no int args".into(),
            })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
        }
        fn arg_float(&mut self, _i: usize) -> Result<f64, HostError> {
            Err(HostError {
                message: "no float args".into(),
            })
        }
        fn arg_ptr(&mut self, i: usize) -> Result<usize, HostError> {
            self.ptrs.get(i).copied().ok_or(HostError {
                message: "missing ptr arg".into(),
            })
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError {
                message: "no str args".into(),
            })
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
    fn null_is_address_zero() {
        let mut host = PtrHost::default();
        assert_eq!(null(&mut host), Ok(NativeRet::Ptr(0)));
    }

    #[test]
    fn is_null_true_for_zero_false_otherwise() {
        let mut host = PtrHost { ptrs: vec![0] };
        assert_eq!(is_null(&mut host), Ok(NativeRet::Bool(true)));
        let mut host = PtrHost { ptrs: vec![0x1234] };
        assert_eq!(is_null(&mut host), Ok(NativeRet::Bool(false)));
    }

    // ---- Memory deref builtins (load_*/store_*) ----

    /// A kind-tagged `Host` over positional args (ptr / int / float / bool), for unit-testing the
    /// deref builtins against a real buffer the test owns. Each arg names which vec it lives in.
    #[derive(Default)]
    struct ArgHost {
        ints: Vec<i64>,
        floats: Vec<f64>,
        bools: Vec<bool>,
        ptrs: Vec<usize>,
        kinds: Vec<(char, usize)>,
    }

    impl ArgHost {
        fn ptr(mut self, v: usize) -> Self {
            self.ptrs.push(v);
            self.kinds.push(('p', self.ptrs.len() - 1));
            self
        }
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
        fn boolean(mut self, v: bool) -> Self {
            self.bools.push(v);
            self.kinds.push(('b', self.bools.len() - 1));
            self
        }
    }

    impl Host for ArgHost {
        fn arg_count(&self) -> usize {
            self.kinds.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.ints[idx])
        }
        fn arg_is_int(&self, i: usize) -> bool {
            self.kinds[i].0 == 'i'
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.floats[idx])
        }
        fn arg_bool(&mut self, i: usize) -> Result<bool, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.bools[idx])
        }
        fn arg_ptr(&mut self, i: usize) -> Result<usize, HostError> {
            let (_, idx) = self.kinds[i];
            Ok(self.ptrs[idx])
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError {
                message: "no str args".into(),
            })
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

    /// The raw address of a value, as a `usize` the deref builtins consume as a C-sourced `ptr`.
    fn addr_of<T>(p: &T) -> usize {
        p as *const T as usize
    }

    #[test]
    fn load_int_reads_c_long() {
        let v: [std::os::raw::c_long; 1] = [-5];
        let mut h = ArgHost::default().ptr(addr_of(&v[0]));
        assert_eq!(load_int(&mut h), Ok(NativeRet::Int(-5)));
    }

    #[test]
    fn load_widths_sign_and_zero_extend() {
        // A buffer holding a negative i32 then a high-bit u32, contiguous.
        let buf: [u32; 2] = [0xFFFF_FFFF, 0x8000_0001];
        let base = addr_of(&buf[0]);
        // i32 0xFFFFFFFF sign-extends to -1; the second u32 0x80000001 (at offset 4) zero-extends.
        assert_eq!(
            load_int32(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(-1))
        );
        assert_eq!(
            load_uint32_at(&mut ArgHost::default().ptr(base).int(4)),
            Ok(NativeRet::Int(0x8000_0001))
        );
    }

    #[test]
    fn load_int8_uint8_extend() {
        let byte: u8 = 0xFF;
        let base = addr_of(&byte);
        // 0xFF as i8 = -1 (sign-extend); as u8 = 255 (zero-extend).
        assert_eq!(
            load_int8(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(-1))
        );
        assert_eq!(
            load_uint8(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(255))
        );
    }

    #[test]
    fn load_uint32_zero_extends_high_bit() {
        let v: u32 = 0x8000_0001;
        let base = addr_of(&v);
        assert_eq!(
            load_uint32(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(0x8000_0001))
        );
        // The signed read of the same bits is negative — proving the extend rule differs by width.
        // 0x80000001 as i32 = -2147483647.
        assert_eq!(
            load_int32(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(-2147483647))
        );
    }

    #[test]
    fn load_float_bool_ptr() {
        let f: f64 = 3.5;
        assert_eq!(
            load_float(&mut ArgHost::default().ptr(addr_of(&f))),
            Ok(NativeRet::Float(3.5))
        );
        let g: f32 = 1.25;
        assert_eq!(
            load_float32(&mut ArgHost::default().ptr(addr_of(&g))),
            Ok(NativeRet::Float(1.25))
        );
        let t: u8 = 1;
        let fb: u8 = 0;
        assert_eq!(
            load_bool(&mut ArgHost::default().ptr(addr_of(&t))),
            Ok(NativeRet::Bool(true))
        );
        assert_eq!(
            load_bool(&mut ArgHost::default().ptr(addr_of(&fb))),
            Ok(NativeRet::Bool(false))
        );
        let target: usize = 0xDEAD;
        let pp: usize = addr_of(&target); // a pointer to a usize holding the address 0xDEAD
        assert_eq!(
            load_ptr(&mut ArgHost::default().ptr(pp)),
            Ok(NativeRet::Ptr(0xDEAD))
        );
    }

    #[test]
    fn load_at_offset_reads_the_right_field() {
        let buf: [i64; 3] = [10, 20, 30];
        let base = addr_of(&buf[0]);
        assert_eq!(
            load_int_at(&mut ArgHost::default().ptr(base).int(8)),
            Ok(NativeRet::Int(20))
        );
        assert_eq!(
            load_int64_at(&mut ArgHost::default().ptr(base).int(16)),
            Ok(NativeRet::Int(30))
        );
    }

    #[test]
    fn load_str_copies_cstring() {
        let cs = std::ffi::CString::new("hello").unwrap();
        let base = cs.as_ptr() as usize;
        assert_eq!(
            load_str(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Str("hello".into()))
        );
        // load_str_at past the first two bytes reads the suffix.
        assert_eq!(
            load_str_at(&mut ArgHost::default().ptr(base).int(2)),
            Ok(NativeRet::Str("llo".into()))
        );
    }

    /// W6-14 — a non-UTF-8 C buffer FAULTS instead of silently mapping the bad byte to U+FFFD (which
    /// handed back a mangled `str` with no error). A Chezzi `str` is UTF-8; the raw-byte hatch is
    /// `load_uint8_at`, which the message names.
    #[test]
    fn load_str_rejects_invalid_utf8() {
        let cs = std::ffi::CString::new([0x41u8, 0xFF, 0x42]).unwrap();
        let base = cs.as_ptr() as usize;
        let err = load_str(&mut ArgHost::default().ptr(base)).unwrap_err();
        assert!(
            err.message.contains("not valid UTF-8") && err.message.contains("load_uint8_at"),
            "unexpected message: {}",
            err.message
        );
        // The offset form faults alike, and the reported offset is relative to the read start.
        let err_at = load_str_at(&mut ArgHost::default().ptr(base).int(1)).unwrap_err();
        assert!(
            err_at.message.contains("offset 0"),
            "unexpected message: {}",
            err_at.message
        );
        // A valid multi-byte string still crosses intact (the check is validation, not ASCII-only).
        let ok = std::ffi::CString::new("héllo ☃").unwrap();
        assert_eq!(
            load_str(&mut ArgHost::default().ptr(ok.as_ptr() as usize)),
            Ok(NativeRet::Str("héllo ☃".into()))
        );
    }

    #[test]
    fn store_then_load_roundtrip() {
        let mut buf = [0u8; 32];
        let base = buf.as_mut_ptr() as usize;
        // store_int then load_int.
        assert_eq!(
            store_int(&mut ArgHost::default().ptr(base).int(42)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_int(&mut ArgHost::default().ptr(base)),
            Ok(NativeRet::Int(42))
        );
        // store_int32_at(off=8, -7) then load_int32_at.
        assert_eq!(
            store_int32_at(&mut ArgHost::default().ptr(base).int(8).int(-7)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_int32_at(&mut ArgHost::default().ptr(base).int(8)),
            Ok(NativeRet::Int(-7))
        );
        // store_float / store_bool / store_ptr round-trips.
        assert_eq!(
            store_float_at(&mut ArgHost::default().ptr(base).int(16).float(2.5)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_float_at(&mut ArgHost::default().ptr(base).int(16)),
            Ok(NativeRet::Float(2.5))
        );
        assert_eq!(
            store_bool_at(&mut ArgHost::default().ptr(base).int(24).boolean(true)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_bool_at(&mut ArgHost::default().ptr(base).int(24)),
            Ok(NativeRet::Bool(true))
        );
        assert_eq!(
            store_ptr_at(&mut ArgHost::default().ptr(base).int(0).ptr(0xBEEF)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_ptr_at(&mut ArgHost::default().ptr(base).int(0)),
            Ok(NativeRet::Ptr(0xBEEF))
        );
        // store_float32 round-trips an exactly-representable value.
        assert_eq!(
            store_float32_at(&mut ArgHost::default().ptr(base).int(8).float(1.25)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_float32_at(&mut ArgHost::default().ptr(base).int(8)),
            Ok(NativeRet::Float(1.25))
        );
    }

    #[test]
    fn store_writes_natural_width_only() {
        // store_int8_at(off=0, 0x1FF) must write ONLY the low byte (0xFF), leaving the next untouched.
        let mut buf = [0xAAu8; 4];
        let base = buf.as_mut_ptr() as usize;
        assert_eq!(
            store_int8_at(&mut ArgHost::default().ptr(base).int(0).int(0x1FF)),
            Ok(NativeRet::Nil)
        );
        // Read back through the same deref path (avoids the compiler assuming `buf` unchanged).
        assert_eq!(
            load_uint8_at(&mut ArgHost::default().ptr(base).int(0)),
            Ok(NativeRet::Int(0xFF)),
            "low byte written (truncated to i8)"
        );
        assert_eq!(
            load_uint8_at(&mut ArgHost::default().ptr(base).int(1)),
            Ok(NativeRet::Int(0xAA)),
            "adjacent byte untouched (natural 1-byte width)"
        );
    }

    #[test]
    fn null_pointer_is_recoverable_error() {
        let cases: Vec<(&str, NativeFn)> = vec![
            ("load_int", load_int),
            ("load_str", load_str),
            ("load_ptr", load_ptr),
        ];
        for (name, f) in cases {
            let mut h = ArgHost::default().ptr(0);
            let err = f(&mut h).expect_err("null deref must be a recoverable error");
            assert!(
                err.message.contains("null pointer") && err.message.contains(name),
                "{}",
                err.message
            );
        }
        // store_int on a null base too.
        let mut h = ArgHost::default().ptr(0).int(1);
        let err = store_int(&mut h).expect_err("null store must be a recoverable error");
        assert!(
            err.message.contains("null pointer") && err.message.contains("store_int"),
            "{}",
            err.message
        );
    }

    #[test]
    fn members_registers_every_builtin() {
        let names: std::collections::HashSet<&str> = MEMBERS.iter().map(|(n, _)| *n).collect();
        // null/is_null + 14 loads × 2 forms + 13 stores × 2 forms + alloc/alloc_zeroed/free
        // = 2 + 28 + 26 + 3 = 59.
        assert_eq!(MEMBERS.len(), 59, "expected exactly 59 std.ffi members");
        for n in [
            "load_int",
            "load_int_at",
            "load_uint64_at",
            "load_float32",
            "load_str_at",
            "load_ptr",
            "store_int",
            "store_int_at",
            "store_float32_at",
            "store_bool",
            "store_ptr_at",
        ] {
            assert!(names.contains(n), "MEMBERS missing {n}");
        }
    }

    // ---- C-buffer alloc layer (alloc / alloc_zeroed / free) ----

    #[cfg(unix)]
    #[test]
    fn alloc_roundtrip_and_free() {
        // alloc a buffer, store an int64 into it via the existing store builtin, read it back, free.
        let p = alloc(&mut ArgHost::default().int(64)).expect("alloc ok");
        let addr = match p {
            NativeRet::Ptr(a) => a,
            other => panic!("alloc must return a Ptr, got {other:?}"),
        };
        assert_ne!(addr, 0, "alloc(64) must not return NULL");
        assert_eq!(
            store_int64_at(&mut ArgHost::default().ptr(addr).int(0).int(123)),
            Ok(NativeRet::Nil)
        );
        assert_eq!(
            load_int64_at(&mut ArgHost::default().ptr(addr).int(0)),
            Ok(NativeRet::Int(123))
        );
        assert_eq!(
            free(&mut ArgHost::default().ptr(addr)),
            Ok(NativeRet::Nil),
            "free must return nil without crashing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn alloc_zeroed_reads_zero() {
        let p = alloc_zeroed(&mut ArgHost::default().int(32)).expect("alloc_zeroed ok");
        let addr = match p {
            NativeRet::Ptr(a) => a,
            other => panic!("alloc_zeroed must return a Ptr, got {other:?}"),
        };
        assert_ne!(addr, 0);
        for off in [0, 8, 16, 24] {
            assert_eq!(
                load_int64_at(&mut ArgHost::default().ptr(addr).int(off)),
                Ok(NativeRet::Int(0)),
                "alloc_zeroed byte at offset {off} must be zero"
            );
        }
        assert_eq!(free(&mut ArgHost::default().ptr(addr)), Ok(NativeRet::Nil));
    }

    #[cfg(unix)]
    #[test]
    fn alloc_negative_is_recoverable_error() {
        let err = alloc(&mut ArgHost::default().int(-1)).expect_err("negative size must error");
        assert!(
            err.message.contains("ffi.alloc: negative size"),
            "{}",
            err.message
        );
        let err =
            alloc_zeroed(&mut ArgHost::default().int(-1)).expect_err("negative size must error");
        assert!(
            err.message.contains("ffi.alloc_zeroed: negative size"),
            "{}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn free_null_is_noop() {
        // C free(NULL) is a defined no-op; ffi.free(ffi.null()) must NOT error (and must NOT route
        // through base_addr, which rejects address 0).
        assert_eq!(
            free(&mut ArgHost::default().ptr(0)),
            Ok(NativeRet::Nil),
            "free(null) must be a safe no-op returning nil"
        );
    }

    #[test]
    fn members_registers_alloc_layer() {
        let names: std::collections::HashSet<&str> = MEMBERS.iter().map(|(n, _)| *n).collect();
        assert_eq!(MEMBERS.len(), 59, "expected exactly 59 std.ffi members");
        for n in ["alloc", "alloc_zeroed", "free"] {
            assert!(names.contains(n), "MEMBERS missing {n}");
        }
    }
}
