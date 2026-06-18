//! Dynamic C-ABI FFI (v1): the runtime machinery behind an `extern "lib":` block. A [`Cffi`]
//! wraps a `dlopen`'d shared library, a resolved symbol address, and the C signature (as
//! [`CType`]s), and exposes `call(&mut dyn Host)` — reusing the engine-neutral [`Host`]/[`NativeRet`]
//! seam (`src/native/mod.rs`) so the VM and the frozen interpreter produce identical output.
//!
//! v1 marshals scalars only: `int` (i64 ↔ C `long`), `float` (f64 ↔ C `double`), `bool`
//! (↔ C `int` 0/1), and `str` (Chezzi str → null-terminated `const char*`; a borrowed `char*` return
//! is copied immediately into an owned Chezzi str, never freed). Plus opaque `ptr` (Chezzi `ptr` ↔ C
//! `void*`): an untyped raw-address handle, passed/returned by value and never auto-freed (the caller
//! calls the library's own destroy).
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
/// `bool`→C `int`, `str`→C `const char*`, `ptr`→C `void*` (an opaque handle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CType {
    Int,
    Float,
    Bool,
    Str,
    /// An opaque `void*` handle (Chezzi `ptr`): a raw address marshalled by value, in and out.
    Ptr,
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
}

impl CType {
    /// The libffi argument/result [`Type`] for this scalar.
    fn ffi_type(self) -> Type {
        match self {
            // int ↔ C `long` (i64 on LP64 Linux); bool marshals through C `int`.
            CType::Int => Type::c_long(),
            CType::Float => Type::f64(),
            CType::Bool => Type::c_int(),
            // str → `const char*`, ptr → `void*`, and every char*-returning variant — all
            // pointers to libffi (the owned/nullable distinction is a Chezzi-side lowering choice,
            // not an ABI one: the C signature is the same `char*`).
            CType::Str | CType::Ptr | CType::OwnedStr | CType::OptStr | CType::OptOwnedStr => {
                Type::pointer()
            }
        }
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
        let mut bool_args: Vec<std::os::raw::c_int> = Vec::new();
        let mut cstrings: Vec<CString> = Vec::new();
        let mut ptr_args: Vec<*const std::os::raw::c_char> = Vec::new();
        // Raw `void*` handles (Chezzi `ptr` args) — plain addresses, stable once pushed.
        let mut void_args: Vec<*mut c_void> = Vec::new();

        // First pass: extract & own every scalar so the `&`-references libffi captures stay valid.
        // Each slot records which storage vec holds it and at what index.
        enum Slot {
            Int(usize),
            Float(usize),
            Bool(usize),
            Ptr(usize),
            /// An opaque `void*` handle, stored in `void_args`.
            RawPtr(usize),
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
                    // Marshal a Chezzi `bool` into a C `int` (0/1) via the host's typed bool reader.
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
                // `OwnedStr`/`OptStr`/`OptOwnedStr` are RETURN-ONLY (the checker rejects them as
                // params before this point), so they can never appear in `self.params`.
                CType::OwnedStr | CType::OptStr | CType::OptOwnedStr => {
                    unreachable!("owned/nullable str CTypes are return-only and rejected as params")
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
            }
        }

        let arg_types = self.params.iter().map(|p| p.ffi_type());
        let result_ty = match self.ret {
            Some(c) => c.ffi_type(),
            None => Type::void(),
        };
        let cif = Cif::new(arg_types, result_ty);
        let code = CodePtr::from_ptr(self.sym as *const c_void);

        // SAFETY: `code` is a function whose C signature matches `self.params`/`self.ret`, which the
        // checker enforces (every extern fn's param + return types are marshallable scalars, and the
        // call site is type-checked). `cif` is built from that same signature, `ffi_args` matches it
        // in order/count, and all referenced storage (`int_args`/`float_args`/`bool_args`/`ptr_args`/
        // `cstrings`) is still in scope, so the read-through pointers are valid for the whole call.
        let ret = unsafe {
            match self.ret {
                Some(CType::Int) => {
                    let r: std::os::raw::c_long = cif.call(code, &ffi_args);
                    NativeRet::Int(r as i64)
                }
                Some(CType::Float) => {
                    let r: f64 = cif.call(code, &ffi_args);
                    NativeRet::Float(r)
                }
                Some(CType::Bool) => {
                    let r: std::os::raw::c_int = cif.call(code, &ffi_args);
                    NativeRet::Bool(r != 0)
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
                None => {
                    let _: () = cif.call(code, &ffi_args);
                    NativeRet::Nil
                }
            }
        };
        // `cstrings` (and the other storage) are dropped here, after the call returns — never before.
        drop(cstrings);
        Ok(ret)
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
        floats: Vec<f64>,
        strs: Vec<String>,
        ptrs: Vec<usize>,
        // Each arg names which vec + index it lives in.
        kinds: Vec<(char, usize)>,
    }

    impl MockHost {
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
    }

    impl Host for MockHost {
        fn arg_count(&self) -> usize {
            self.kinds.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            // not exercised by these tests
            let _ = i;
            Err(HostError {
                message: "no int args".into(),
            })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
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
}
