//! Dynamic C-ABI FFI (v1): the runtime machinery behind an `extern "lib":` block. A [`Cffi`]
//! wraps a `dlopen`'d shared library, a resolved symbol address, and the C signature (as
//! [`CType`]s), and exposes `call(&mut dyn Host)` — reusing the engine-neutral [`Host`]/[`NativeRet`]
//! seam (`src/native/mod.rs`) so the VM and the frozen interpreter produce identical output.
//!
//! v1 marshals scalars only: `int` (i64 ↔ C `long`), `float` (f64 ↔ C `double`), `bool`
//! (↔ C `int` 0/1), and `str` (Chezzi str → null-terminated `const char*`; a `char*` return is
//! copied immediately into an owned Chezzi str). Structs by value, callbacks, varargs, opaque
//! pointers / userdata, and `char*` ownership transfer / `free` are deferred (documented limits).
//!
//! ## Send/Sync (for `--parallel`)
//! The VM stores `Obj::Cffi(Arc<Cffi>)`, and the M:N engine shares the parent address space across
//! OS-thread workers, so [`Cffi`] must be `Send + Sync`. Two design choices make that hold:
//! - libloading's `Symbol` is `!Send`, so the resolved symbol is stored as a raw `usize` address
//!   (cast to a `CodePtr` at call time) with the `Library` kept alive in the same `Arc`.
//! - libffi's `Cif` is `!Send` (raw pointers, no `unsafe impl Send`), so it is **not** stored;
//!   the cheap `Cif::new` (`prep_cif`) is rebuilt per call from the `Send` [`CType`] signature.

use std::ffi::{c_void, CStr, CString};

use libffi::middle::{arg, Cif, CodePtr, Type};
use libloading::Library;

use super::{Host, HostError, NativeRet};

/// A C-marshallable scalar type — the v1 FFI surface. `int`→C `long`, `float`→C `double`,
/// `bool`→C `int`, `str`→C `const char*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CType {
    Int,
    Float,
    Bool,
    Str,
}

impl CType {
    /// The libffi argument/result [`Type`] for this scalar.
    fn ffi_type(self) -> Type {
        match self {
            // int ↔ C `long` (i64 on LP64 Linux); bool marshals through C `int`.
            CType::Int => Type::c_long(),
            CType::Float => Type::f64(),
            CType::Bool => Type::c_int(),
            // str → `const char*` (a pointer).
            CType::Str => Type::pointer(),
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
        let library = unsafe { Library::new(lib) }
            .map_err(|e| HostError { message: format!("cannot load library '{lib}': {e}") })?;
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
            unsafe { symbol.try_as_raw_ptr() }
                .ok_or_else(|| HostError {
                    message: format!("symbol '{sym_name}' has no address on this platform"),
                })? as usize
        };
        Ok(Cffi { _lib: library, sym: addr, params, ret, name: sym_name.to_string() })
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

        // First pass: extract & own every scalar so the `&`-references libffi captures stay valid.
        // Each slot records which storage vec holds it and at what index.
        enum Slot {
            Int(usize),
            Float(usize),
            Bool(usize),
            Ptr(usize),
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
                        // `recover:`. (A genuinely-nullable C return is out of v1 scope; model it by
                        // returning `int`/a sentinel, or wait for a future `str?` extern return.)
                        return Err(HostError {
                            message: format!(
                                "extern fn '{}' returned NULL for its declared `str` return",
                                self.name
                            ),
                        });
                    }
                    // Copy immediately into an owned Chezzi str. We never `free` the pointer
                    // (v1 documented limit: no ownership transfer), so a malloc'd return leaks.
                    NativeRet::Str(CStr::from_ptr(p).to_string_lossy().into_owned())
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
    }

    impl Host for MockHost {
        fn arg_count(&self) -> usize {
            self.kinds.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            // not exercised by these tests
            let _ = i;
            Err(HostError { message: "no int args".into() })
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
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError { message: "no map args".into() })
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
        let f =
            Cffi::new("libm.so.6", "cos", vec![CType::Float], Some(CType::Float)).expect("dlopen cos");
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
        let err = f.call(&mut host).expect_err("NULL char* for a `str` return must fault");
        assert!(err.message.contains("returned NULL"), "{}", err.message);
    }

    #[test]
    fn missing_library_is_an_error() {
        let err = Cffi::new("libdoesnotexist.so.999", "cos", vec![], None).unwrap_err();
        assert!(err.message.contains("cannot load library"), "{}", err.message);
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
