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

/// The callable members of `std.ffi`.
pub const MEMBERS: &[(&str, NativeFn)] = &[("null", null), ("is_null", is_null)];

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
}
