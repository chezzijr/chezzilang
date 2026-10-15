//! C FFI unit tests. Moved out of `src/native/cffi.rs`; `super::*` is that module.

use super::*;
use crate::native::{Host, HostError, NativeRet};
use crate::test_support::build_callback_so;

/// TICKET-218 — a C width's range comes from Rust's primitive bounds; uint64 accepts any int at
/// run time (its bit pattern) but only `0..=i64::MAX` as a constant; float32 rejects a finite
/// value beyond f32 and passes NaN/inf; the wrap helpers are C casts.
#[test]
fn width_ranges_come_from_rust_bounds() {
    assert_eq!(CType::Int8.int_range(), Some((-128, 127)));
    assert_eq!(CType::UInt64.int_range(), Some((0, u64::MAX as i128)));
    assert_eq!(CType::Float32.int_range(), None);
    assert!(CType::UInt64.fits_int(-1, false));
    assert!(!CType::UInt64.fits_int(-1, true));
    assert!(CType::UInt64.fits_int(i64::MAX, true));
    assert!(!CType::Int8.fits_int(300, false));
    assert!(CType::Int8.fits_int(127, true) && !CType::Int8.fits_int(128, true));
    assert!(CType::Int8.fits_int(-128, false) && !CType::Int8.fits_int(-129, false));
    assert!(CType::Int64.fits_int(i64::MIN, true) && CType::Int64.fits_int(i64::MAX, false));
    assert!(CType::Float32.fits_f64(f32::MAX as f64));
    assert!(!CType::Float32.fits_f64(1e39));
    assert!(!CType::Float32.fits_f64(-1e39));
    assert!(CType::Float32.fits_f64(f64::NAN));
    assert!(CType::Float32.fits_f64(f64::INFINITY));
    assert_eq!(CType::Int8.wrap_int(300), 44);
    assert_eq!(CType::UInt32.wrap_int(-1), 4294967295);
    assert_eq!(CType::UInt64.wrap_int(-1), -1);
    assert_eq!(CType::Int16.wrap_int(40000), -25536);
    assert!(CType::Float32.wrap_f64(1e39).is_infinite());
    assert_eq!(CType::Int8.width_name(), Some("int8"));
    assert_eq!(CType::Int.width_name(), None);
    assert_eq!(CType::Int8.range_text(), "(-128..127)");
    assert_eq!(
        CType::Float32.range_text(),
        "(-3.4028234663852886e38..3.4028234663852886e38)"
    );
    assert_eq!(
        CType::Int8
            .check_fits(&NativeRet::Int(300))
            .unwrap_err()
            .message,
        "value 300 does not fit int8 (-128..127)"
    );
    assert!(CType::Int8.check_fits(&NativeRet::Int(-128)).is_ok());
    assert_eq!(
        CType::Float32
            .check_fits(&NativeRet::Float(1e39))
            .unwrap_err()
            .message,
        "value 1e39 does not fit float32 (-3.4028234663852886e38..3.4028234663852886e38)"
    );
    assert!(CType::UInt64.check_fits(&NativeRet::Int(-1)).is_ok());
    assert!(CType::Int.check_fits(&NativeRet::Int(i64::MAX)).is_ok());
}

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
#[derive(Clone, Copy)]
enum CbBehavior {
    /// `f(int) -> int`: double the int arg, +1 (so the C fixture's `f(x)+1` is testable end-to-end).
    DoubleIntPlusOne,
    /// `f(double) -> double`: negate the float arg.
    NegateFloat,
    /// Always return an `Err(HostError)` — the trampoline must stash + re-raise it.
    ReturnsError,
    /// Panic in the callback body — `catch_unwind` must catch it and re-raise a fault, not abort.
    Panics,
    /// `f(int) -> nil`: collect the int arg into `MockHost.collected`, return `Nil` (void).
    VoidCollect,
}

/// A standalone `Host` over fixed args, for unit-testing the FFI marshalling in isolation.
#[derive(Default)]
struct MockHost {
    ints: Vec<i64>,
    floats: Vec<f64>,
    strs: Vec<String>,
    ptrs: Vec<usize>,
    /// Struct args: each is its fields as engine-neutral [`NativeRet`]s (declaration order); a
    /// nested struct field is itself a `NativeRet::List`.
    structs: Vec<Vec<NativeRet>>,
    /// Callback args: each is a [`CbBehavior`] the host's `invoke_callback` applies to the C args.
    callbacks: Vec<CbBehavior>,
    /// Where `CbBehavior::VoidCollect` pushes each int arg it is invoked with.
    collected: Vec<i64>,
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
    fn arg_c_value(&mut self, i: usize) -> Result<NativeRet, HostError> {
        let (k, idx) = self.kinds[i];
        Ok(match k {
            'i' => NativeRet::Int(self.ints[idx]),
            'f' => NativeRet::Float(self.floats[idx]),
            's' => NativeRet::Str(self.strs[idx].clone()),
            'p' => NativeRet::Ptr(self.ptrs[idx]),
            'S' => NativeRet::List(self.structs[idx].clone()),
            _ => {
                return Err(HostError {
                    message: format!("arg {i} is not a C value"),
                });
            }
        })
    }
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[NativeRet],
    ) -> Result<NativeRet, HostError> {
        let (_, idx) = self.kinds[arg_index];
        let b = self.callbacks[idx];
        match b {
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
            CbBehavior::VoidCollect => match args.first() {
                Some(NativeRet::Int(n)) => {
                    self.collected.push(*n);
                    Ok(NativeRet::Nil)
                }
                other => Err(HostError {
                    message: format!("callback expected int, got {other:?}"),
                }),
            },
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

/// TICKET-217: `float32` is C `float` at the boundary. The C reference (`cc ref.c -lm`) prints
/// `sqrtf(2)=1.4142135381698608 fabsf(-2.5)=2.5`; a `double` CIF would give 1.4142135623730951.
#[test]
fn float32_libm_sqrtf_and_fabsf_match_c() {
    let sqrtf = Cffi::new("libm", "sqrtf", vec![CType::Float32], Some(CType::Float32))
        .expect("dlopen sqrtf");
    let mut host = MockHost::default().float(2.0);
    assert_eq!(
        sqrtf.call(&mut host),
        Ok(NativeRet::Float(1.4142135381698608))
    );
    let fabsf = Cffi::new("libm", "fabsf", vec![CType::Float32], Some(CType::Float32))
        .expect("dlopen fabsf");
    let mut host = MockHost::default().float(-2.5);
    assert_eq!(fabsf.call(&mut host), Ok(NativeRet::Float(2.5)));
}

/// A `float32` struct field is 4 bytes: `[Float32, Float32]` (raylib `Vector2`) is size 8 with
/// offsets `[0, 4]`, and a field round-trips through the buffer rounded to f32.
#[test]
fn float32_struct_fields_are_four_bytes() {
    let fields = [CType::Float32, CType::Float32];
    let (_ty, size, _align, offsets) = struct_layout(&fields);
    assert_eq!(size, 8);
    assert_eq!(offsets, vec![0, 4]);
    let mut buf = vec![0u8; size];
    write_field(
        &mut buf,
        offsets[1],
        &CType::Float32,
        &NativeRet::Float(0.1),
    )
    .unwrap();
    assert_eq!(
        read_field(&buf, offsets[1], &CType::Float32),
        NativeRet::Float(0.10000000149011612)
    );
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
    let open = Cffi::new("libc.so.6", "tmpfile", vec![], Some(CType::Ptr)).expect("dlopen tmpfile");
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
    let f =
        Cffi::new("libc.so.6", "abs", vec![CType::Int32], Some(CType::Int32)).expect("dlopen abs");
    let mut host = MockHost::default().int(-5);
    assert_eq!(f.call(&mut host), Ok(NativeRet::Int(5)));
}

#[test]
fn int8_param_out_of_range_faults() {
    // TICKET-218 — a Chezzi i64 outside the C width is a recoverable fault at the boundary,
    // never a silent wrap (255 used to reach C as -1). `ffi.cast_int8` is the explicit wrap.
    let f =
        Cffi::new("libc.so.6", "abs", vec![CType::Int8], Some(CType::Int8)).expect("dlopen abs");
    let mut host = MockHost::default().int(255);
    assert_eq!(
        f.call(&mut host),
        Err(HostError {
            message: "value 255 does not fit int8 (-128..127)".to_string()
        })
    );
    let mut host = MockHost::default().int(-128);
    assert_eq!(f.call(&mut host), Ok(NativeRet::Int(-128)));
}

#[test]
fn int32_return_sign_extends_negative() {
    // `atoi("-1") -> int` returns the C int -1; declared int32 it must SIGN-extend back to the
    // Chezzi i64 -1 (not 0x00000000FFFFFFFF == 4294967295).
    let f =
        Cffi::new("libc.so.6", "atoi", vec![CType::Str], Some(CType::Int32)).expect("dlopen atoi");
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
    write_field(&mut buf, offsets[0], &CType::Int8, &NativeRet::Int(-1)).unwrap();
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

/// The struct PARAM marshal loop reads its fields through `Host::arg_c_value` in declaration
/// order: prove the MockHost reader surfaces the fields in order (the cffi `call` loop then writes
/// them into the C buffer at the libffi offsets, covered by `struct_param_mixed_fields_marshals`).
#[test]
fn struct_param_host_reads_fields_in_order() {
    let mut host = MockHost::default().strukt(vec![NativeRet::Int(3), NativeRet::Int(2)]);
    let v = host.arg_c_value(0).unwrap();
    assert_eq!(
        v,
        NativeRet::List(vec![NativeRet::Int(3), NativeRet::Int(2)])
    );
}

/// TICKET-217: a C variadic call through `ffi_prep_cif_var`. The C reference prints
/// `snprintf n=9 buf=x-42-3.14` for `snprintf(buf, 64, "%s-%ld-%.2f", "x", 42L, 3.14159)`. The
/// `%.2f` cell needs the variadic CIF: on x86-64 a fixed-arity CIF leaves `%al` unset.
#[test]
#[allow(clippy::approx_constant)] // `3.14159` is the C reference's literal, not a stand-in for PI.
fn snprintf_with_mixed_varargs_matches_c() {
    let f = Cffi::new(
        "libc",
        "snprintf",
        vec![CType::Ptr, CType::Int, CType::Str],
        Some(CType::Int32),
    )
    .expect("dlopen snprintf")
    .with_c_variadic(true);
    let mut buf = vec![0u8; 64];
    let mut host = MockHost::default()
        .ptr(buf.as_mut_ptr() as usize)
        .int(64)
        .string("%s-%ld-%.2f")
        .string("x")
        .int(42)
        .float(3.14159);
    assert_eq!(f.call(&mut host), Ok(NativeRet::Int(9)));
    let s = CStr::from_bytes_until_nul(&buf).unwrap();
    assert_eq!(s.to_str().unwrap(), "x-42-3.14");
}

/// TICKET-217: C's default argument promotions (C11 6.5.2.2p6), one row per `CType` variant.
/// `float` becomes `double`; `_Bool` and every integer narrower than `int` become `int`; a
/// struct, callback or return-only string form cannot be a C vararg at all.
#[test]
fn vararg_promotion_table_matches_c_default_promotions() {
    let st = CType::Struct {
        name: "S".into(),
        field_names: vec!["a".into()],
        fields: vec![CType::Int],
    };
    let cb = CType::Callback {
        params: vec![],
        ret: None,
    };
    let rows: Vec<(CType, Option<CType>)> = vec![
        (CType::Int, Some(CType::Int)),
        (CType::Float, Some(CType::Float)),
        (CType::Float32, Some(CType::Float)),
        (CType::Bool, Some(CType::Int32)),
        (CType::Int8, Some(CType::Int32)),
        (CType::Int16, Some(CType::Int32)),
        (CType::UInt8, Some(CType::Int32)),
        (CType::UInt16, Some(CType::Int32)),
        (CType::Int32, Some(CType::Int32)),
        (CType::UInt32, Some(CType::UInt32)),
        (CType::Int64, Some(CType::Int64)),
        (CType::UInt64, Some(CType::UInt64)),
        (CType::Ptr, Some(CType::Ptr)),
        (CType::Str, Some(CType::Str)),
        (CType::OwnedStr, None),
        (CType::OptStr, None),
        (CType::OptOwnedStr, None),
        (st, None),
        (cb, None),
    ];
    for (ct, want) in rows {
        assert_eq!(ct.vararg_promoted(), want, "{ct:?}");
    }
}

/// TICKET-217: a nested struct field sits at `parent offset + inner offset`, all from libffi.
/// `Deep {Out {In {int a, b}, int c}, double d}`: the C reference prints `sizeof Deep=24 off
/// Deep.d=16`; `In.b` is at 4 and `Out.c` at 8. A value shaped as nested `NativeRet::List`s
/// writes and reads back as nested `NativeRet::Struct`s, and a wrong field count is an error.
#[test]
fn nested_struct_offsets_are_parent_plus_inner() {
    let st = |name: &str, names: &[&str], fields: Vec<CType>| CType::Struct {
        name: name.into(),
        field_names: names.iter().map(|n| n.to_string()).collect(),
        fields,
    };
    let inn = st("In", &["a", "b"], vec![CType::Int32, CType::Int32]);
    let out = st("Out", &["i", "c"], vec![inn, CType::Int32]);
    let deep = st("Deep", &["o", "d"], vec![out, CType::Float]);
    let CType::Struct { fields, .. } = &deep else {
        unreachable!()
    };
    let (_ty, size, _align, offsets) = struct_layout(fields);
    assert_eq!(offsets, vec![0, 16]);
    assert_eq!(size, 24);
    use NativeRet::{Float, Int, List};
    let v = List(vec![
        List(vec![List(vec![Int(1), Int(2)]), Int(3)]),
        Float(0.5),
    ]);
    let mut buf = vec![0u8; size];
    write_field(&mut buf, 0, &deep, &v).unwrap();
    assert_eq!(read_field(&buf, 4, &CType::Int32), Int(2));
    assert_eq!(read_field(&buf, 8, &CType::Int32), Int(3));
    let s = |name: &str, fs: Vec<(&str, NativeRet)>| NativeRet::Struct {
        name: name.into(),
        fields: fs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    };
    assert_eq!(
        read_field(&buf, 0, &deep),
        s(
            "Deep",
            vec![
                (
                    "o",
                    s(
                        "Out",
                        vec![
                            ("i", s("In", vec![("a", Int(1)), ("b", Int(2))])),
                            ("c", Int(3))
                        ]
                    )
                ),
                ("d", Float(0.5)),
            ]
        )
    );
    let short = List(vec![List(vec![List(vec![Int(1)]), Int(3)]), Float(0.5)]);
    assert!(write_field(&mut buf, 0, &deep, &short).is_err());
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
                ret: Some(Box::new(CType::Int32)),
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
                ret: Some(Box::new(CType::Int32)),
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
                ret: Some(Box::new(CType::Float)),
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
                ret: Some(Box::new(CType::Int32)),
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
fn a_str_arg_is_retained_only_when_a_pointer_can_come_back() {
    // `strlen` never hands back a pointer into its `str` arg -- retaining it would be a pure
    // leak with no correctness benefit, so nothing should land in the retention table.
    let f = Cffi::new("libc.so.6", "strlen", vec![CType::Str], Some(CType::Int))
        .expect("dlopen strlen");
    let key = "TICKET-060 not retained probe";
    let mut host = MockHost::default().string(key);
    assert_eq!(f.call(&mut host), Ok(NativeRet::Int(key.len() as i64)));
    let mut with_nul = key.as_bytes().to_vec();
    with_nul.push(0);
    assert!(
        retained_lookup(&with_nul).is_none(),
        "strlen never returns a pointer into its str arg -- nothing should be retained"
    );

    // `strchr` CAN hand back a pointer into its `str` arg. It must be retained, and keyed by
    // content: two calls marshalling the SAME text must retain and return the SAME buffer.
    let g = Cffi::new(
        "libc.so.6",
        "strchr",
        vec![CType::Str, CType::Int32],
        Some(CType::Ptr),
    )
    .expect("dlopen strchr");
    let key2 = "TICKET-060 retained probe";
    let mut host2 = MockHost::default().string(key2).int(32);
    let r1 = g.call(&mut host2).expect("strchr call 1");
    let mut with_nul2 = key2.as_bytes().to_vec();
    with_nul2.push(0);
    assert!(
        retained_lookup(&with_nul2).is_some(),
        "strchr can return a pointer into its str arg -- it must be retained"
    );
    let mut host3 = MockHost::default().string(key2).int(32);
    let r2 = g.call(&mut host3).expect("strchr call 2");
    assert_eq!(
        r1, r2,
        "two calls marshalling the same text must retain and return the SAME buffer, not one per call"
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
                ret: Some(Box::new(CType::Int32)),
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
fn callback_void_return_writes_no_result() {
    // TICKET-061: a void-returning callback (`ret: None`) must write NOTHING into libffi's
    // result buffer — `each`'s `f` param collects each int arg and returns `Nil`.
    let so = build_callback_so();
    let f = Cffi::new(
        so.to_str().unwrap(),
        "each",
        vec![
            CType::Int32,
            CType::Callback {
                params: vec![CType::Int64],
                ret: None,
            },
        ],
        None,
    )
    .expect("dlopen each");
    let mut host = MockHost::default().int(3).callback(CbBehavior::VoidCollect);
    assert_eq!(f.call(&mut host), Ok(NativeRet::Nil));
    assert_eq!(host.collected, vec![0, 1, 2]);
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
