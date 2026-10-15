//! VM golden tests: C FFI programs run end to end (callbacks, `ptr` returns into `str` args,
//! `ffi.load`/`store`, the C buffer layer). Split out of `src/native/cffi/tests.rs`, because a run
//! needs the front end and the VM, and `native` sits below both.

use crate::test_support::{CB_SO_COUNTER, build_callback_so};

#[test]
fn a_c_fn_that_writes_through_a_str_arg_does_not_poison_the_retained_buffer() {
    // `strtok` WRITES a NUL into its retained buffer to punch the token boundary. Chezzi `str` is
    // immutable, so a later call marshalling the SAME literal must see the ORIGINAL bytes, not
    // the mutated ones -- `retained_lookup` restores the buffer on a mismatch.
    let src = [
        "import std.ffi",
        "extern \"libc\":",
        "    fn strtok(s: str, d: str) -> ptr",
        "    fn strlen(s: str) -> int",
        "p := strtok(\"uv,wx\", \",\")",
        "print(ffi.load_str(p))",
        "print(strlen(\"uv,wx\"))",
        "",
    ]
    .join("\n");
    let entry = write_deref_chz(&src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, "uv\n5\n",
        "a str arg mutated by the C callee must be restored before the next call reuses it"
    );
}

#[test]
fn faulting_callback_is_invoked_only_once_per_extern_call() {
    // TICKET-060: libc `qsort` calls the comparator repeatedly regardless of the FIRST
    // invocation's result. A callback that faults on its first call must not be re-entered by
    // later invocations from the same `qsort` -- one fault is enough to abandon the extern call.
    // As currently implemented, `callback_trampoline` stashes only the LAST fault and keeps
    // re-entering the engine on every subsequent C-side callback invocation, so `count` is the
    // number of comparisons `qsort` performs (15 for this 10-element input), not 1.
    let src = [
        "import std.ffi",
        "extern \"libc\":",
        "    fn qsort(base: ptr, n: int, size: int, cmp: fn(ptr, ptr) -> int)",
        "count := 0",
        "fn cmp(a: ptr, b: ptr) -> int:",
        "    count += 1",
        "    return 1 / 0",
        "data := [5, 2, 9, 1, 7, 3, 8, 4, 6, 0]",
        "n := data.len()",
        "buf := ffi.alloc(n * 8)",
        "defer ffi.free(buf)",
        "for i in 0..n:",
        "    ffi.store_int64_at(buf, i * 8, data[i])",
        "r := recover: qsort(buf, n, 8, cmp)",
        "print(count)",
        "print(r)",
        "",
    ]
    .join("\n");
    let entry = write_deref_chz(&src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, "1\n!division by zero\n",
        "the comparator must fault exactly once, not be re-invoked by qsort's later comparisons"
    );
}

#[test]
fn str_arg_backing_cstring_must_outlive_a_ptr_returned_into_it() {
    // TICKET-060: `Cffi::call` frees the `CString` marshalled for a `str` param when the extern
    // call returns (`cstrings`, built at the `CType::Str` arm, dropped at the end of `call`). Any
    // C fn that returns a pointer INTO its `str` argument (`strchr` here) hands Chezzi a `ptr`
    // that dangles the instant `call` returns -- `ffi.load_str` on it then reads freed memory.
    // The corrupted content is heap-layout-dependent (varies run to run, confirmed non-`" world"`
    // over 10+ runs), so this asserts against the CORRECT value with a fixed custom message
    // rather than the (unstable) garbage `Cffi::call` currently produces.
    let src = "import std.ffi\nimport int32 from std.ffi\nextern \"libc\":\n    fn strchr(s: str, c: int32) -> ptr\np := strchr(\"hello world\", 32)\nprint(ffi.load_str(p))\n";
    let entry = write_deref_chz(src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, " world\n",
        "a ptr returned into a str argument must not dangle once the extern call returns"
    );
}

#[test]
fn first_callback_fault_is_the_one_reraised() {
    // TICKET-060: `callback_trampoline` currently overwrites `*ctx.fault` on every invocation, so
    // C's later re-invocations (qsort keeps comparing after the comparator already faulted)
    // clobber the first fault with the last one. Each invocation here raises a DIFFERENT index
    // error (`count` grows every call), so the re-raised message pins down which invocation won.
    let src = [
        "import std.ffi",
        "extern \"libc\":",
        "    fn qsort(base: ptr, n: int, size: int, cmp: fn(ptr, ptr) -> int)",
        "count := 0",
        "fn cmp(a: ptr, b: ptr) -> int:",
        "    count += 1",
        "    return [1][count + 4]",
        "data := [5, 2, 9, 1, 7, 3, 8, 4, 6, 0]",
        "n := data.len()",
        "buf := ffi.alloc(n * 8)",
        "defer ffi.free(buf)",
        "for i in 0..n:",
        "    ffi.store_int64_at(buf, i * 8, data[i])",
        "r := recover: qsort(buf, n, 8, cmp)",
        "print(r)",
        "",
    ]
    .join("\n");
    let entry = write_deref_chz(&src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, "!index 5 out of bounds (len 1)\n",
        "the FIRST callback fault must be the one re-raised, not the last"
    );
}

#[test]
fn ptr_returned_into_a_str_arg_survives_a_later_extern_call() {
    // TICKET-060: a ptr returned into a str arg's C buffer must outlive not just the call that
    // returned it, but subsequent extern calls too (each of which allocates + frees its own
    // marshalled `str` args, potentially reusing the freed slot).
    let src = [
        "import std.ffi",
        "import int32 from std.ffi",
        "extern \"libc\":",
        "    fn strchr(s: str, c: int32) -> ptr",
        "    fn strlen(s: str) -> int",
        "p := strchr(\"ticket 060 alpha beta\", 32)",
        "print(strlen(\"a fresh allocation to churn the heap after the call\") > 0)",
        "print(strlen(\"and a second one, longer than the first by some margin\") > 0)",
        "print(ffi.load_str(p))",
        "",
    ]
    .join("\n");
    let entry = write_deref_chz(&src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, "true\ntrue\n 060 alpha beta\n",
        "a ptr returned into a str arg must survive later extern calls, not just the call that returned it"
    );
}

#[test]
fn interior_ptr_written_into_a_ptr_out_param_survives_the_call() {
    // TICKET-060: a `ptr` PARAM may be a C out-param the callee writes an interior pointer into
    // (strtol's endptr) -- that pointer must remain valid after the call returns too.
    let src = [
        "import std.ffi",
        "import int32 from std.ffi",
        "extern \"libc\":",
        "    fn strtol(s: str, endptr: ptr, base: int32) -> int",
        "buf := ffi.alloc(8)",
        "defer ffi.free(buf)",
        "r := strtol(\"  -0x1fzz\", buf, 16)",
        "print(r)",
        "print(ffi.load_str(ffi.load_ptr(buf)))",
        "",
    ]
    .join("\n");
    let entry = write_deref_chz(&src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(
        out, "-31\nzz\n",
        "an interior pointer written into a ptr out-param must survive the call"
    );
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
fn callback_void_return_runs() {
    // TICKET-061 W10-7: a callback param whose return is `nil` (void) — the commonest C
    // callback shape (foreach/twalk-style). `each(3, show)` prints "0", "1", "2".
    let so = build_callback_so();
    let src = format!(
        "extern \"{}\":\n    fn each(n: int, f: fn(int) -> None)\n\nfn show(i: int):\n    print(i)\n\neach(3, show)\n",
        so.to_str().unwrap()
    );
    let out = crate::vm::run_capture(&src).expect("run");
    assert_eq!(out, "0\n1\n2\n");
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
        "for i in 0..data.len():\n",
        "    ffi.store_int64_at(p, i * 8, data[i])\n",
        "for i in 0..data.len():\n",
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
        "for i in 0..4:\n",
        "    print(ffi.load_int64_at(p, i * 8))\n",
        "ffi.free(p)\n",
    );
    let entry = write_deref_chz(src);
    let (out, _e, res, _) = crate::vm::run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(out, "0\n0\n0\n0\n");
}
