//! Golden tests: std.cancel, overflow, string methods, example goldens via run_file, FFI goldens,
//! trace formatting, defaults.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

// ===== std.cancel TREE PROPAGATION (parent/child derivation) =====
//
// A child token derived from a parent observes the parent's cancellation/timeout transitively
// (root-to-leaves, one-directional). The link is LIVE (each node's `Shared` flag + the immediate
// parent's `Channel[Token]` registry cross a spawn as live cores), so a parent cancel is pushed
// into every already-derived descendant — including children that crossed `spawn`/`parallel:`.
// Cancelling a child never touches the parent.
//
// Each test runs an inline Chezzi snippet through the real module graph (`import std.cancel`
// needs `run_file`, not the standalone compile path). The `print`s are the assertion surface.

/// A derived child observes a manual parent cancel (transitive, live link). Child starts live.
#[test]
fn cancel_child_polls_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_polls_parent",
        "    p := cancel.manual()\n    c := p.derive()\n    print(\"c before: {c.cancelled()}\")\n    p.cancel()\n    print(\"c after: {c.cancelled()}\")\n    match c.reason():\n        ?r: print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n",
    );
    assert_eq!(out, "c before: false\nc after: true\nc reason: cancelled\n");
}

/// Cancelling a child is one-directional: the parent is untouched (flag + reason stay clear).
#[test]
fn cancel_child_cancel_does_not_touch_parent() {
    let out = run_cancel_snippet(
        "cancel_child_one_directional",
        "    p := cancel.manual()\n    c := p.derive()\n    c.cancel()\n    print(\"c: {c.cancelled()}\")\n    print(\"p: {p.cancelled()}\")\n    match p.reason():\n        ?r: print(\"p reason: {r}\")\n        None:    print(\"p reason: none\")\n",
    );
    assert_eq!(out, "c: true\np: false\np reason: none\n");
}

/// Cascade is transitive: cancelling the root cancels a grandchild with the inherited reason.
#[test]
fn cancel_transitive_grandchild() {
    let out = run_cancel_snippet(
        "cancel_transitive_grandchild",
        "    p := cancel.manual()\n    c := p.derive()\n    g := c.derive()\n    p.cancel()\n    print(\"g: {g.cancelled()}\")\n    match g.reason():\n        ?r: print(\"g reason: {r}\")\n        None:    print(\"g reason: none\")\n",
    );
    assert_eq!(out, "g: true\ng reason: cancelled\n");
}

/// A manual parent cancel makes a derived child's `done()` channel ready (registry fan-out).
#[test]
fn cancel_child_done_ready_after_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_done_fanout",
        "    p := cancel.manual()\n    c := p.derive()\n    p.cancel()\n    match c.done().try_recv():\n        ?v: print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascades TRANSITIVELY: a manual GRANDPARENT cancel makes a grandchild's `done()`
/// channel ready, not just `cancelled()`. `cancel()` drains each node's immediate-child registry and
/// recurses down, so the grandparent's cascade reaches it (a `wait:` waiter wakes).
#[test]
fn cancel_grandchild_done_ready_after_grandparent_cancel() {
    let out = run_cancel_snippet(
        "cancel_grandchild_done_fanout",
        "    gp := cancel.manual()\n    mid := gp.derive()\n    leaf := mid.derive()\n    gp.cancel()\n    match leaf.done().try_recv():\n        ?v: print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascade reaches any depth: a manual ROOT cancel makes a great-grandchild's (depth 3)
/// `done()` channel ready, proving the downward cascade recurses through every level to the leaves.
#[test]
fn cancel_great_grandchild_done_ready_after_root_cancel() {
    let out = run_cancel_snippet(
        "cancel_ggchild_done_fanout",
        "    root := cancel.manual()\n    a := root.derive()\n    b := a.derive()\n    leaf := b.derive()\n    root.cancel()\n    match leaf.done().try_recv():\n        ?v: print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// A child of an already-elapsed timeout parent inherits the tightest deadline: it is cancelled
/// at once, its reason is "timeout", its done() is ready, and its deadline equals the parent's.
#[test]
fn cancel_child_inherits_tightest_deadline() {
    let out = run_cancel_snippet(
        "cancel_child_tightest_deadline",
        "    p := cancel.timeout(0)\n    c := p.derive()\n    print(\"c cancelled: {c.cancelled()}\")\n    match c.reason():\n        ?r: print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n    match c.done().try_recv():\n        ?v: print(\"c done: {v}\")\n        None:    print(\"c not done\")\n    print(\"deadline match: {c.deadline_at() == p.deadline_at()}\")\n",
    );
    assert_eq!(
        out,
        "c cancelled: true\nc reason: timeout\nc done: true\ndeadline match: true\n"
    );
}

/// The live link survives a task boundary: a derived child crosses `spawn`/`parallel:`,
/// and a parent cancel done in the parent task BEFORE the nursery is observed by the spawned task.
/// (The name is historical — `Token` no longer has a `parent` field. The link is the child's OWN
/// `Shared` flag, which the parent's downward cascade already set, and it crosses as a LIVE core,
/// not a snapshot.) Deterministic: the parent cancelled before the spawn, so the worker only polls
/// the already-flipped flag.
#[test]
fn cancel_token_sendable_with_parent() {
    let src = "import Shared from std.concurrency\nimport std.cancel\n\nfn watch(c: cancel.Token, seen: Shared[bool]):\n    if c.cancelled():\n        seen.set(true)\n\nfn main():\n    p := cancel.manual()\n    c := p.derive()\n    seen := Shared(false)\n    p.cancel()\n    parallel:\n        spawn watch(c, seen)\n    print(\"observed cancel: {seen.get()}\")\n\nmain()\n";
    let out = run_cancel_src("cancel_token_sendable_parent", src);
    assert_eq!(out, "observed cancel: true\n");
}

/// The free-function form `cancel.derive(parent)` is equivalent to `parent.derive()`.
#[test]
fn cancel_derive_free_fn_form() {
    let out = run_cancel_snippet(
        "cancel_derive_free_fn",
        "    p := cancel.manual()\n    c := cancel.derive(p)\n    print(\"before: {c.cancelled()}\")\n    p.cancel()\n    print(\"after: {c.cancelled()}\")\n",
    );
    assert_eq!(out, "before: false\nafter: true\n");
}

/// The registration channel is live across a spawn in BOTH directions: a worker task derives a
/// GRANDCHILD on the FAR side of a `spawn` (a `send` into the crossed child's `kids`) and parks in
/// `wait: gk.done().recv()`, while a sibling cancels the ROOT on the near side (a drain of that same
/// channel, two levels up). Every round must wake — the strongest liveness proof for the downward
/// cascade, since neither half of the registry is owned by the task that reads it.
#[test]
fn cancel_cascade_crosses_tasks() {
    let src = "import std.cancel\n\
                   import std.time\n\
                   fn waiter(c: cancel.Token, out: Channel[bool]):\n\
                   \x20   gk := c.derive()\n\
                   \x20   wait:\n\
                   \x20       _ := gk.done().recv(): out.send(true)\n\
                   fn canceller(root: cancel.Token):\n\
                   \x20   time.sleep_ms(5)\n\
                   \x20   root.cancel()\n\
                   fn main():\n\
                   \x20   out := Channel[bool]()\n\
                   \x20   n := 20\n\
                   \x20   for i in 0..n:\n\
                   \x20       root := cancel.manual()\n\
                   \x20       kid := root.derive()\n\
                   \x20       parallel:\n\
                   \x20           spawn waiter(kid, out)\n\
                   \x20           spawn canceller(root)\n\
                   \x20   seen := 0\n\
                   \x20   for _ in 0..n:\n\
                   \x20       out.recv()\n\
                   \x20       seen = seen + 1\n\
                   \x20   print(seen)\n\
                   main()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "a far-side derive must be reached by a near-side cancel: {res:?}"
    );
    assert_eq!(
        out, "20\n",
        "every far-side grandchild must be woken by the near-side root cancel"
    );
}

/// `std.cancel` tree-propagation golden (VM): `examples/cancel_tree.chz` byte-matches `.expected`.
/// Deterministic (manual cancel + pre-ready/zero timers, same task) → a golden test.
#[test]
fn golden_cancel_tree_via_run_file() {
    let path = fixture("examples/cancel_tree.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_tree.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// C-ABI FFI golden: the `extern "lib":` block calls
/// `cos`/`sqrt` (libm) and `strlen` (libc) via dlopen+libffi on the VM, byte-matches `.expected`.
/// Runs wherever the `libc`/`libm` aliases resolve; otherwise prints a SKIP line naming the reason.
#[test]
fn golden_ffi_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_chz_via_run_file: {why}");
        return;
    }
    let path = fixture("examples/ffi.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// C-ABI opaque `ptr` handle golden: the `extern "lib":`
/// block opens a libc `FILE*` (`fopen -> ptr`), hands the handle back to `fclose`, and detects a
/// NULL handle via `std.ffi.is_null` / `== std.ffi.null()`. Byte-matches `.expected`.
/// Runs wherever the `libc`/`libm` aliases resolve; otherwise prints a SKIP line naming the reason.
#[test]
fn golden_ffi_ptr_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_ptr_chz_via_run_file: {why}");
        return;
    }
    let path = fixture("examples/ffi_ptr.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_ptr.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// C struct BY VALUE golden: the `extern "lib":`
/// block binds `div_t div(int, int)` — a libc fn taking two scalars and returning a small POD
/// struct BY VALUE — to a Chezzi `struct DivT{quot, rem}`. `div(17, 5) == {3, 2}` (pure, always
/// present). Byte-matches `.expected`. The `--parallel`/M:N engine is NOT driven here; the returned
/// `NativeRet::Struct` is a plain heap struct that crosses a spawn by handle like any other, so the
/// path is exercised, just not by this golden. Runs wherever the `libc`/`libm` aliases resolve;
/// otherwise prints a SKIP line naming the reason.
#[test]
fn golden_ffi_struct_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_struct_chz_via_run_file: {why}");
        return;
    }
    let path = fixture("examples/ffi_struct.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_struct.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// CAPSTONE C-buffer FFI golden: sort a Chezzi list
/// with libc `qsort`, composing `ffi.alloc` + `store_int64_at` + a Chezzi `qsort` comparator
/// callback (`load_int64` both `const void*` sides) + `load_int64_at` read-back + `defer ffi.free`.
/// The marquee proof that callbacks + deref + alloc all compose. Byte-matches `.expected`. Runs
/// wherever the `libc`/`libm` aliases resolve, otherwise prints a SKIP line naming the reason; the
/// fixed 8-byte int64 stride matches the comparator on every LP64 unix.
#[test]
fn golden_ffi_qsort_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_qsort_chz_via_run_file: {why}");
        return;
    }
    let path = fixture("examples/ffi_qsort.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_qsort.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    // Also exercise the M:N `--parallel` engine: the qsort comparator re-enters the VM as a
    // libffi callback under worker pinning — the highest-risk callback+alloc composition, and
    // the one path the single-file golden above does not cover. Keeps the engine
    // matrix honest (the sibling deref test runs `--parallel` too).
    let (par_out, _par_err, par_res, _) =
        run_file_with(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, expected,
        "M:N --parallel diverged for ffi_qsort.chz"
    );
}

/// C-ABI deeper `str` returns golden: `strdup -> owned_str`
/// (owned `char*` copied into a Chezzi `str` AND freed) and `getenv -> str?` (NULL → `None`). Byte-
/// matches `.expected`. Runs wherever the `libc`/`libm` aliases resolve; otherwise prints a SKIP
/// line naming the reason.
#[test]
fn golden_ffi_str_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_str_chz_via_run_file: {why}");
        return;
    }
    let path = fixture("examples/ffi_str.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_str.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// C-ABI fixed-width integer marshalling golden: `atoi
/// -> int32` (sign-extend), `htonl(uint32) -> uint32` (zero-extend, high-bit positive), `abs(int8)
/// -> int8` (signed round-trip + an explicit `cast_int8` wrap). Byte-matches `.expected`. Runs
/// wherever the `libc`/`libm` aliases resolve; otherwise prints a SKIP line naming the reason.
// The example's `htonl` lines encode little-endian oracles, so the golden skips at runtime on a
// big-endian host instead of `#[cfg]`-gating.
#[test]
fn golden_ffi_int_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_int_chz_via_run_file: {why}");
        return;
    }
    if cfg!(target_endian = "big") {
        eprintln!(
            "SKIP golden_ffi_int_chz_via_run_file: the example's htonl oracles are little-endian"
        );
        return;
    }
    let path = fixture("examples/ffi_int.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_int.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
/// the VM, byte-matches `.expected`.
#[test]
fn golden_overflow_via_run_file() {
    // The integer-overflow policy, end-to-end: every overflow (arith, neg, div, math.abs) is a
    // recoverable fault, identical.
    let path = fixture("examples/overflow.chz");
    let expected = std::fs::read_to_string(fixture("examples/overflow.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

#[test]
fn golden_stats_app_via_run_file() {
    let path = fixture("examples/stats.chz");
    let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// G3 golden: `examples/stdlib_cmp.chz` — `import std.cmp`, generic `min`/`max`/`clamp` over
/// int/float/str/struct, and `list.sort()` over Comparable structs. Byte-matches `.expected`
/// and stays identical on interp + VM.
#[test]
fn golden_stdlib_cmp_via_run_file() {
    let path = fixture("examples/stdlib_cmp.chz");
    let expected = std::fs::read_to_string(fixture("examples/stdlib_cmp.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.flag: `--name value` + `--count=3` `=`-form + a trailing positional.
#[test]
fn flag_parse_value_and_eq_form_parity() {
    let out = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.str_flag(\"name\", \"def\", \"the name\")\n\
         fs.int_flag(\"count\", 0, \"how many\")\n\
         match fs.parse([\"--name\", \"alice\", \"--count=3\", \"pos1\"]):\n\
         \x20   ?p:\n\
         \x20       print(fs.get_str(\"name\"))\n\
         \x20       print(fs.get_int(\"count\"))\n\
         \x20       print(p[0])\n\
         \x20   !e:\n\
         \x20       print(e.message())\n",
    );
    assert_eq!(out, "alice\n3\npos1\n");
}

/// std.flag: bool-as-presence + the `--` terminator makes later dash tokens positional.
#[test]
fn flag_bool_presence_and_terminator_parity() {
    let out = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.bool_flag(\"verbose\", false, \"v\")\n\
         match fs.parse([\"--verbose\", \"--\", \"--notaflag\", \"x\"]):\n\
         \x20   ?p:\n\
         \x20       print(fs.get_bool(\"verbose\"))\n\
         \x20       print(\" \".join(fs.positionals()))\n\
         \x20   !e:\n\
         \x20       print(e.message())\n",
    );
    assert_eq!(out, "true\n--notaflag x\n");
}

/// std.flag: unknown flag / missing value / non-int are clean `Err`, never a fault.
#[test]
fn flag_error_paths_parity() {
    let unknown = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         match fs.parse([\"--nope\"]):\n\
         \x20   ?p: print(\"ok\")\n\
         \x20   !e: print(e.message())\n",
    );
    assert_eq!(unknown, "unknown flag --nope\n");

    let missing = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.str_flag(\"name\", \"\", \"n\")\n\
         match fs.parse([\"--name\"]):\n\
         \x20   ?p: print(\"ok\")\n\
         \x20   !e: print(e.message())\n",
    );
    assert_eq!(missing, "flag --name: missing value\n");

    let badint = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.int_flag(\"count\", 0, \"c\")\n\
         match fs.parse([\"--count\", \"abc\"]):\n\
         \x20   ?p: print(\"ok\")\n\
         \x20   !e: print(\"errored\")\n",
    );
    assert_eq!(badint, "errored\n");
}

/// std.flag: `usage()` is a byte-exact multi-line string in REGISTRATION order (parity-safe).
#[test]
fn flag_usage_deterministic_parity() {
    let out = golden_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.str_flag(\"name\", \"def\", \"the name\")\n\
         fs.bool_flag(\"verbose\", false, \"be loud\")\n\
         fs.int_flag(\"count\", 5, \"how many\")\n\
         print(fs.usage())\n",
    );
    assert_eq!(
        out,
        "  --name (str) default=def: the name\n  \
         --verbose (bool) default=false: be loud\n  \
         --count (int) default=5: how many\n"
    );
}

/// std.flag golden: `examples/flag_demo.chz` exercises every case in one program, byte-matches
/// `.expected`, and stays identical on the serial + M:N engines.
#[test]
fn golden_flag_demo_via_run_file() {
    let path = fixture("examples/flag_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/flag_demo.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.log golden: `examples/log_demo.chz` exercises gating, all 4 level formats, set_level, an
/// injectable prefix, the pure `format_line`, and stderr-default + stdout target in one program.
/// Log lines land on STDERR, so `.expected` pins the STDERR stream; discrimination asserts prove the
/// stream routing against both streams.
#[test]
fn golden_log_demo_via_run_file() {
    let path = fixture("examples/log_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/log_demo.expected")).unwrap();
    let (out, err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(err, expected); // the pinned STDERR stream
    // Discrimination: the pure format_line + the stdout-target logger land on STDOUT, the default
    // (stderr) logger's messages do NOT — and vice versa.
    assert!(out.contains("WARN pure") && out.contains("INFO stdout-line"));
    assert!(!out.contains("served") && !err.contains("stdout-line"));
}

/// std.string helpers golden: `examples/str_more.chz` — the additive ends_with/index_of/count/
/// replace/strip_prefix/strip_suffix funcs, end-to-end on the VM, byte-identical to `.expected`
/// and the interpreter.
#[test]
fn golden_str_more_via_run_file() {
    let path = fixture("examples/str_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/str_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// str receiver-method golden: `examples/str_methods.chz` — the gap #1 forwarder methods
/// (ends_with/replace/repeat/reverse/pad_left/index_of/count/strip_prefix/strip_suffix/
/// split_lines/strip) + gap #7 safe parse (to_int/to_float), end-to-end on the VM,
/// byte-identical to `.expected` and the interpreter.
#[test]
fn golden_str_methods_via_run_file() {
    let path = fixture("examples/str_methods.chz");
    let expected = std::fs::read_to_string(fixture("examples/str_methods.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `iter.reduce` has no seed, so an empty list has no accumulator to start from. It used to leak the
/// std module's own internal index error (`index 0 out of bounds (len 0)` at a std/iter.chz line
/// number) — an implementation detail, and a confusing one, since the user never wrote that index.
/// It faults with a named, user-facing message instead (Python: `TypeError: reduce() of empty
/// iterable with no initial value`). Still an ordinary recoverable fault, never a host panic.
#[test]
fn reduce_empty_list_faults_with_named_message() {
    let src = "import std.iter as iter\ne: List[int] = []\nprint(iter.reduce(e, fn(a: int, b: int) -> int: a + b))\n";
    // Faults identically (`golden_entry_fault` asserts against a literal expectation).
    let e = golden_entry_fault(src);
    assert!(
        e.contains("reduce: empty list with no initial value"),
        "the message names the fn and the cause: {e}"
    );
    assert!(
        !e.contains("index 0 out of bounds"),
        "the std module's internal index error must not leak: {e}"
    );
}

#[test]
fn reduce_empty_is_recoverable_and_nonempty_still_works() {
    // The fault is catchable by `recover:` (not a host panic), and the BOUNDARY — an ordinary
    // non-empty reduce — is unaffected.
    let src = r#"
import std.iter as iter

fn main():
    e: List[int] = []
    r := recover:
        iter.reduce(e, fn(a: int, b: int) -> int: a + b)
    match r:
        ?v: print("no fault {v}")
        !err: print("caught: {err.message()}")
    print(iter.reduce([1, 2, 3], fn(a: int, b: int) -> int: a + b))
    print(iter.reduce([7], fn(a: int, b: int) -> int: a + b))
main()
"#;
    // `golden_entry` runs the graph path on BOTH engines and asserts byte-identical stdout.
    let out = golden_entry(src);
    assert!(
        out.contains("caught: reduce: empty list with no initial value"),
        "{out:?}"
    );
    assert!(
        out.ends_with("6\n7\n"),
        "non-empty reduce unaffected: {out:?}"
    );
}

/// std.iter helpers golden: `examples/iter_more.chz` — the additive take/drop/any/all/find/
/// flatten funcs, end-to-end on the VM, byte-identical to `.expected` and the interpreter.
#[test]
fn golden_iter_more_via_run_file() {
    let path = fixture("examples/iter_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/iter_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

// --- lazy iterator adapters (gaps §3) — count/repeat/cycle/chain/islice/imap/ifilter ---
// Each RUNS inline source through the graph path on BOTH engines (serial + M:N) via golden_entry
// and asserts byte-identical stdout. The count+islice test is the laziness canary: an infinite
// source pulled through a finite prefix MUST terminate — if it hangs, laziness broke.

#[test]
fn test_lazy_count_islice_terminates() {
    // Infinite count() → islice prefix of 5. Must terminate.
    let out = golden_entry(
        "import std.iter\nfn main():\n  out:=[]\n  for x in iter.islice(iter.count(), 5): out.push(x)\n  print(out)\nmain()\n",
    );
    assert_eq!(out, "[0, 1, 2, 3, 4]\n");
    // count(start, step) arithmetic + stop<=0 = empty.
    let out2 = golden_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.count(10, 3), 4): a.push(x)\n  b:=[]\n  for x in iter.islice(iter.count(), 0): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out2, "[10, 13, 16, 19]\n[]\n");
}

#[test]
fn test_lazy_islice_consumes_exactly_stop() {
    // islice pulls EXACTLY `stop` from a shared live source (CPython itertools.islice contract), not
    // stop+1. Slice 3 off a live count(), then slice 2 more off the SAME generator: exact ⇒ [0,1,2]
    // then [3,4]; over-consuming by one would leave the source at 4 ⇒ [4,5].
    let out = golden_entry(
        "import std.iter\nfn main():\n  g:=iter.count()\n  a:=[]\n  for x in iter.islice(g, 3): a.push(x)\n  b:=[]\n  for x in iter.islice(g, 2): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[0, 1, 2]\n[3, 4]\n");
}

// std.string.count with an EMPTY substring matches Python/Go (`len(s)+1`), not 0 (the old guard drifted
// from both ancestors + from its own sibling `index_of(s,"")==0`). Both engines agreed on the wrong value
// before the fix, so the parity oracle was blind — this pins the corrected value.
#[test]
fn string_count_empty_substring_matches_python() {
    let out = golden_entry(
        "import std.string\nfn main():\n  print(string.count(\"abc\", \"\"))\n  print(string.count(\"hello\", \"\"))\n  print(string.count(\"\", \"\"))\n  print(string.count(\"abc\", \"b\"))\nmain()\n",
    );
    // Python: "abc".count("")==4, "hello".count("")==6, "".count("")==1, "abc".count("b")==1.
    assert_eq!(out, "4\n6\n1\n1\n");
}

#[test]
fn test_lazy_repeat() {
    let out = golden_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.repeat(7), 3): a.push(x)\n  b:=[]\n  for x in iter.repeat(9, 2): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[7, 7, 7]\n[9, 9]\n");
}

#[test]
fn test_lazy_cycle_and_empty() {
    let out = golden_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.cycle([1,2,3]), 7): a.push(x)\n  b:=[]\n  for x in iter.cycle([]): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    // cycle of empty list terminates to [] (not an infinite spin).
    assert_eq!(out, "[1, 2, 3, 1, 2, 3, 1]\n[]\n");
}

#[test]
fn test_lazy_chain_order() {
    let out = golden_entry(
        "import std.iter\nfn main():\n  out:=[]\n  for x in iter.chain([1,2],[3,4]): out.push(x)\n  print(out)\nmain()\n",
    );
    assert_eq!(out, "[1, 2, 3, 4]\n");
}

#[test]
fn test_lazy_imap_ifilter_compose() {
    // Lazy map/filter over an infinite count(), sliced — must compose and terminate.
    let out = golden_entry(
        "import std.iter\nfn double(x:int)->int: return x*2\nfn is_even(x:int)->bool: return x%2==0\nfn main():\n  a:=[]\n  for x in iter.islice(iter.imap(iter.count(1), double), 4): a.push(x)\n  b:=[]\n  for x in iter.islice(iter.ifilter(iter.count(), is_even), 3): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[2, 4, 6, 8]\n[0, 2, 4]\n");
}

/// std.rand goldens — run as ONE test so they execute sequentially. The PRNG is a process-global
/// (shared by the VM + every test in the run); two *separate* `#[test]`s seeding-then-drawing
/// would interleave on that global under the test harness's parallel runner and diverge. Drawn
/// sequentially within one test body, each program's `rand.seed()`-then-draw stream is
/// deterministic and byte-identical against `.expected`.
/// (This is the same concurrent-draw limit documented in docs/reference/stdlib/ §std.rand — the goldens
/// avoid it by serializing.)
///
/// - `rand_seeded`: all four scalar fns (seed/int/float/bool), seeded → deterministic values.
/// - `rand_shape`: UNSEEDED (OS-entropy path); asserts only range/shape, prints fixed "ok" lines
///   (value-independent, so byte-identical against `.expected` despite the nondeterministic seed).
/// - `rand_iter`: std.iter `shuffle`/`choice`/`sample` (pure Chezzi over native `rand.int`).
#[test]
fn golden_rand_via_run_file() {
    // Serialize against the rand unit tests (shared process-global RNG); see TEST_RNG_LOCK.
    let _g = crate::native::rand::TEST_RNG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for rel in [
        "examples/rand_seeded.chz",
        "examples/rand_shape.chz",
        "examples/rand_iter.chz",
    ] {
        let path = fixture(rel);
        let expected =
            std::fs::read_to_string(fixture(&format!("{}.expected", &rel[..rel.len() - 4])))
                .unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{rel}: {res:?}");
        assert_eq!(out, expected, "stdout mismatch for {rel}");
    }
}

/// std.path golden: `examples/path.chz` — the pure-Chezzi unix path-STRING ops
/// (is_abs/is_rel/basename/dirname/split/ext/stem/with_ext/normalize/join), end-to-end on the
/// VM, byte-identical to `.expected`.
#[test]
fn golden_path_via_run_file() {
    let path = fixture("examples/path.chz");
    let expected = std::fs::read_to_string(fixture("examples/path.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.datetime golden: `examples/datetime.chz` — the pure-Chezzi civil-calendar module
/// (from_epoch/to_epoch/formatters/duration/leap/weekday on FIXED epochs only, no clock),
/// end-to-end on the VM, byte-identical to `.expected`.
#[test]
fn golden_datetime_via_run_file() {
    let path = fixture("examples/datetime.chz");
    let expected = std::fs::read_to_string(fixture("examples/datetime.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.concurrency.collection golden: `examples/concurrent_collection.chz` — the pure-Chezzi
/// thread-safe `ConcurrentMap`/`ConcurrentCounter` wrappers over `RwShared[Map[...]]`. The program
/// is DETERMINISTIC by construction (single-write-lock RMW for the counter, each-own-key for the
/// map), so its output is identical on the VM against `.expected`, and (verified
/// manually) `--parallel`.
#[test]
fn golden_concurrent_collection_via_run_file() {
    let path = fixture("examples/concurrent_collection.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/concurrent_collection.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.collections golden: `examples/collections.chz` — the pure-Chezzi Heap/Deque/Counter
/// module (min/max heap + from_list, two-stack deque both ends, Counter most_common ties),
/// end-to-end on the VM, byte-identical to `.expected`.
#[test]
fn golden_collections_via_run_file() {
    let path = fixture("examples/collections.chz");
    let expected = std::fs::read_to_string(fixture("examples/collections.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// M8-M5 golden: `examples/json_decode.chz` — type-directed `json.decode[T]` into struct /
/// typed map / list / scalar, with Option fields, extra-key tolerance, and an error case.
/// Byte-identical on interp + VM.
#[test]
fn golden_json_decode_via_run_file() {
    let path = fixture("examples/json_decode.chz");
    let expected = std::fs::read_to_string(fixture("examples/json_decode.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// std.process polish golden: `examples/process_polish.chz` — the structured `run`/`run_args`
/// forms. Proves (a) a non-zero exit is `Ok(ProcResult)` carrying both streams + code (stdout NOT
/// discarded), (b) `run_args` runs WITHOUT a shell so `$(...)`/`;`/`&&` are passed literally
/// (injection-safe), (c) a spawn failure is a catchable `Err`. Byte-identical on the VM + interp.
#[test]
fn golden_process_polish_via_run_file() {
    let path = fixture("examples/process_polish.chz");
    let expected = std::fs::read_to_string(fixture("examples/process_polish.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// M8+ golden: `examples/fs_mutations.chz` — the std.fs mutation surface (mkdir/append/rename/
/// copy/remove_file/remove_dir) as a self-cleaning round-trip. The script writes into the
/// gitignored `examples/.fs_scratch` and tears it down; the test pre/post-removes the scratch so a
/// prior crash cannot poison the run, and asserts byte-identical stdout on the VM + interp.
#[test]
fn golden_fs_mutations_via_run_file() {
    // Serialize against the interp twin: both write the single fixed examples/.fs_scratch.
    let _g = crate::native::fs::FS_SCRATCH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let scratch = fixture("examples/.fs_scratch");
    let _ = std::fs::remove_dir_all(&scratch); // guard against a stale dir from a crashed run
    let path = fixture("examples/fs_mutations.chz");
    let expected = std::fs::read_to_string(fixture("examples/fs_mutations.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    // The round-trip removes its own scratch; assert nothing leaked into the working tree.
    assert!(
        !scratch.exists(),
        "fs_mutations.chz left examples/.fs_scratch behind"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// std.encoding + std.crypto goldens — deterministic str<->str codecs / digests (no entropy), so
/// they need no lock and are byte-identical on VM + interp + `.expected`.
#[test]
fn golden_encoding_crypto_via_run_file() {
    for rel in ["examples/encoding.chz", "examples/crypto.chz"] {
        let path = fixture(rel);
        let expected =
            std::fs::read_to_string(fixture(&format!("{}.expected", &rel[..rel.len() - 4])))
                .unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{rel}: {res:?}");
        assert_eq!(out, expected, "stdout mismatch for {rel}");
    }
}

/// std.uuid golden — `examples/uuid_shape.chz` uses `uuid_seed(N)` so the v4() stream is
/// deterministic and byte-identical on VM + interp. The UUID RNG is a process-global shared by
/// VM + interp + every test; serialize against the uuid unit tests (see TEST_UUID_LOCK) so a
/// concurrent seeded draw can't interleave on the global and diverge (the documented `--parallel`
/// concurrent-draw limit, surfacing in the parallel test runner — same as std.rand's golden).
#[test]
fn golden_uuid_via_run_file() {
    let _g = crate::native::uuid::TEST_UUID_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = fixture("examples/uuid_shape.chz");
    let expected = std::fs::read_to_string(fixture("examples/uuid_shape.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Comprehensions golden: `examples/comprehensions.chz` — list/set/map comprehensions, a guard,
/// and a range source. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_comprehensions_via_run_file() {
    let path = fixture("examples/comprehensions.chz");
    let expected = std::fs::read_to_string(fixture("examples/comprehensions.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Nested-clause comprehensions golden: `examples/comprehensions_nested.chz` — 2- and 3-clause
/// list comps, a guard after a non-final clause, a later clause referencing an earlier variable,
/// and nested set + map forms. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_comprehensions_nested_via_run_file() {
    let path = fixture("examples/comprehensions_nested.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/comprehensions_nested.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Stateful-iterator comprehension golden: `examples/comprehension_iter_state.chz` — a
/// comprehension whose element/guard read a struct iterator's LIVE, per-`next()` state. The
/// iterator must be driven lazily (interleaved with the body), so this byte-matches `.expected`
/// (regression for the eager-drain bug).
#[test]
fn golden_comprehension_iter_state_via_run_file() {
    let path = fixture("examples/comprehension_iter_state.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/comprehension_iter_state.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Radix-literal golden: `examples/hex.chz` — hex/binary/octal literals feeding bitwise +
/// arithmetic. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_hex_via_run_file() {
    let path = fixture("examples/hex.chz");
    let expected = std::fs::read_to_string(fixture("examples/hex.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// List concat/extend + map merge/update golden: `examples/concat_merge.chz`. New-vs-mutate
/// semantics + arg-wins-on-key-clash. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_concat_merge_via_run_file() {
    let path = fixture("examples/concat_merge.chz");
    let expected = std::fs::read_to_string(fixture("examples/concat_merge.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Tuple-destructuring `for` + `std.iter` golden: `examples/for_tuple.chz` — destructure a list
/// of tuples, one-var whole-tuple, triples, `enumerate`/`zip`, comprehension combo. Byte-matches
/// `.expected`, identical on every run (a two-name loop over a list of tuples is the ordinary tuple unpack).
#[test]
fn golden_for_tuple_via_run_file() {
    let path = fixture("examples/for_tuple.chz");
    let expected = std::fs::read_to_string(fixture("examples/for_tuple.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Optional chaining + null-coalescing golden: `examples/optchain.chz` — `?.field`, `?.method()`,
/// `??`, chaining + None short-circuit. Desugared to `match`; byte-matches `.expected`, identical
/// on interp + VM.
#[test]
fn golden_optchain_via_run_file() {
    let path = fixture("examples/optchain.chz");
    let expected = std::fs::read_to_string(fixture("examples/optchain.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Runtime stack trace: a faulting nested call reports the error line + the call chain (innermost
/// first) with each call's line, and its formatted rendering matches a literal golden. Asserts
/// against that golden rather than a SECOND run of the same file (TICKET-059): with `--serial`
/// deleted 2026-08-16 there is only one engine, so running the file twice and diffing the two traces
/// against each other proves nothing (`CLAUDE.md`'s one-engine rule).
#[test]
fn stack_trace_reports_call_chain() {
    let path = fixture("examples/stack_trace.chz");
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "division by zero");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["divide", "compute", "main"]);
    // Call-site lines, innermost first.
    let lines: Vec<u32> = e.trace.iter().map(|f| f.span.line).collect();
    assert_eq!(lines, vec![15, 18, 20]);
    // The rendered trace names each file relative to the working directory, so its literal golden
    // is `examples/stack_trace.expected`, which `tests/examples_golden.rs` runs from the repo root.
}

/// W13-20: a fault raised inside a generator's body, driven by a `for` loop in an OUTER function,
/// must report the FULL call chain (generator frames AND driver frames), not just one side. The
/// generator swaps its own private frame stack into the live `Vm` while it runs (`swap_gen_ctx`),
/// and `generator_next` restores the host frames before propagating the fault — so a fault captured
/// while `self.frames` held only the generator's frames drops the driver's frames on the floor.
#[test]
fn generator_fault_trace_includes_driver_frames() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn drive():\n    for x in g():\n        print(x)\nfn main():\n    drive()\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("u02.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 3 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["h", "g", "drive", "main"]);
}

/// W14-35c: a suspended generator's body frame reports the resume that FAULTED, not the first
/// `.next()` that drove it. Here `g.next()` (line 10) is the first resume and `List(g)` (line 11)
/// the faulting one; CPython's traceback names line 11.
#[test]
fn generator_fault_trace_names_the_faulting_drain_site() {
    let src = "fn inner() -> Iterator[int]:\n    yield 1\n    xs := [1]\n    yield xs[5]\nfn outer() -> Iterator[int]:\n    for v in inner():\n        yield v * 2\nfn main():\n    g := outer()\n    print(g.next())\n    print(List.from_iter(g))\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("gf2.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 5 out of bounds (len 1)");
    let line_of = |name: &str| {
        e.trace
            .iter()
            .find(|f| f.function == name)
            .unwrap_or_else(|| panic!("no frame named {name}: {:?}", e.trace))
            .span
            .line
    };
    assert_eq!(
        line_of("outer"),
        11,
        "outer is called at the faulting List.from_iter(g)"
    );
    assert_eq!(line_of("inner"), 6, "inner is driven by outer's for loop");
}

/// W13-20: the other half of the merge-guard bug — a `for` loop driving the generator directly in
/// `main` must keep `main`'s own frame instead of losing it to the generator's frames.
#[test]
fn generator_fault_trace_keeps_the_driver_when_main_drives() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn main():\n    for x in g():\n        print(x)\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("y18.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 3 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["h", "g", "main"]);
}

/// W13-20: the explicit `.next()` driver path (`src/vm/call.rs`) must merge the same way as the
/// `for`-loop driver path (`src/vm/stmt.rs`).
#[test]
fn generator_fault_trace_includes_driver_frames_for_explicit_next() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn drive():\n    it := g()\n    print(it.next())\nfn main():\n    drive()\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("u09.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 3 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["h", "g", "drive", "main"]);
}

/// W13-20: a generator driving another generator must report every frame in the chain.
#[test]
fn nested_generator_fault_trace_lists_every_frame() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn inner() -> Iterator[int]:\n    yield h()\nfn outer() -> Iterator[int]:\n    for v in inner():\n        yield v\nfn drive():\n    for x in outer():\n        print(x)\nfn main():\n    drive()\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nested.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 3 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["h", "inner", "outer", "drive", "main"]);
}

/// W13-20 guard: a generator fault CAUGHT by the driver's own `recover:` must not leave a stale
/// prefix behind to decorate a later, unrelated fault.
#[test]
fn a_generator_fault_caught_by_the_driver_leaves_no_frames_behind() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn drive() -> int:\n    r := recover:\n        for x in g():\n            print(x)\n    print(\"caught\")\n    ys: List[int] = []\n    return ys[7]\nfn main():\n    print(drive())\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("caught.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 7 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["drive", "main"]);
}

/// W13-20 guard: a generator fault raised by a DEFERRED `.next()` call, during an unwind a
/// `recover:` catches, must not leave `Vm::gen_fault_prefix` set for a later, unrelated fault to
/// pick up. `unwind_deferred` runs the deferred `.next()` outside the dispatch loop's own error
/// arm, so the prefix `generator_next` parks on that inner fault is never taken there — it must be
/// cleared when the OUTER fault is caught instead.
#[test]
fn a_deferred_generator_fault_does_not_leak_its_prefix_to_a_later_fault() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn f():\n    it := g()\n    defer it.next()\n    ys: List[int] = []\n    print(ys[5])\nfn main():\n    r := recover:\n        f()\n    print(\"caught\")\n    zs: List[int] = []\n    print(zs[9])\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("deferred_leak.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 9 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["main"]);
}

/// Review finding (2026-09-15): the prior fix closed the leak only for a `defer it.next()` OUTSIDE
/// the `recover:` block. A `defer it.next()` DIRECTLY INSIDE the `recover:` block still leaked: the
/// clear used to run BEFORE the recover block's own defer drain (`drain_frame_to(h.defer_len)` in
/// `exec.rs`), so a deferred generator fault drained AFTER the clear set the prefix again and it
/// survived to decorate the next uncaught fault.
#[test]
fn a_defer_inside_the_recover_block_does_not_leak_its_prefix_to_a_later_fault() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn main():\n    it := g()\n    r := recover:\n        defer it.next()\n        ys: List[int] = []\n        print(ys[5])\n    print(\"caught\")\n    zs: List[int] = []\n    print(zs[9])\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("defer_in_recover_leak.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 9 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["main"]);
}

/// Review finding (2026-09-15), the `?` catch path sibling: a recover-scoped `?` (`stmt.rs`'s
/// `do_try`, which drains the recover block's own defers at its own boundary) had NO clear site at
/// all — the same leak shape as the `recover:`-catch path, one level lower.
#[test]
fn a_defer_inside_a_recover_scoped_question_mark_does_not_leak_its_prefix() {
    let src = "fn h() -> int:\n    xs: List[int] = []\n    return xs[3]\nfn g() -> Iterator[int]:\n    yield h()\nfn maybe() -> int!str:\n    return !\"boom\"\nfn main():\n    it := g()\n    r := recover:\n        defer it.next()\n        maybe()?\n    print(\"caught\")\n    zs: List[int] = []\n    print(zs[9])\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_gen_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("defer_in_question_mark_recover_leak.chz");
    std::fs::write(&path, src).unwrap();
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "index 9 out of bounds (len 0)");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["main"]);
}

/// Helper: write deep-infinite-recursion source to a temp file and return its path. The recursion
/// hits `MAX_CALL_DEPTH` → a `recursion limit exceeded` fault with a ~10_000-frame raw trace.
#[cfg(test)]
fn deep_recursion_fixture(tag: &str) -> std::path::PathBuf {
    let src =
        "fn rec(n: int) -> int:\n    return rec(n + 1)\nfn main():\n    print(rec(0))\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_deep_recursion_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    // Unique per-test filename so concurrently-running tests never truncate each other's source
    // mid-read (a shared path would race: one test reads an empty/partial file → no fault).
    let path = dir.join(format!("rec_{tag}.chz"));
    std::fs::write(&path, src).unwrap();
    path
}

/// Gap #8: an infinite-recursion fault must NOT print one frame per call (~10_001 lines). The
/// renderer collapses consecutive identical frames into a `× N` form and caps the printed trace,
/// so the output stays a couple dozen lines while the raw captured trace is still ~10_000 frames.
#[test]
fn recursion_trace_is_bounded_and_collapsed() {
    let path = deep_recursion_fixture("bounded");
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("deep recursion should fault");
    // Raw trace really is huge.
    assert!(
        e.trace.len() > 1000,
        "expected a deep raw trace, got {}",
        e.trace.len()
    );
    let fmt = format_trace(&e);
    let nlines = fmt.lines().count();
    assert!(
        nlines < 30,
        "trace should be bounded, got {nlines} lines:\n{fmt}"
    );
    // Collapse marker present (the `× N more identical frames` run elision).
    assert!(
        fmt.contains("identical frames"),
        "expected a collapse marker, got:\n{fmt}"
    );
    // Innermost frame still visible.
    assert!(
        fmt.contains("at rec (called at"),
        "innermost frame must be shown, got:\n{fmt}"
    );
    // Outermost frame (main) still visible.
    assert!(
        fmt.contains("at main (called at"),
        "outermost frame must be shown, got:\n{fmt}"
    );
}

/// Gap #8: the bounded/collapsed trace must be byte-identical against the golden.
#[test]
fn recursion_trace_parity_vm_vs_interp() {
    let path = deep_recursion_fixture("parity");
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("deep recursion should fault");
    let vm_fmt = format_trace(&e);
    let (_o2, _e2, ip_res, _) = run_file(&path);
    let ie = ip_res.expect_err("deep recursion should fault");
    let ip_fmt = format_trace(&ie);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must produce the same bounded/collapsed trace"
    );
}

/// Build a synthetic `RunError` for a hand-rolled trace (no real run, no real `Program` — `files`
/// is empty, so `format_trace` falls back to the historical `line N, col M` form for every span).
#[cfg(test)]
fn synthetic_run_error(message: &str, span: Span, trace: Vec<TraceFrame>) -> RunError {
    RunError {
        message: message.to_string(),
        span,
        trace,
        files: Vec::new(),
    }
}

/// Gap #8 cap path: a deep chain of DISTINCT-named frames (no collapse possible) is still bounded
/// by the head/tail cap with a `frames elided` marker, and stays byte-identical across engines.
#[test]
fn format_trace_caps_distinct_name_chain() {
    let span = Span::RUNTIME;
    let trace: Vec<TraceFrame> = (0..50)
        .map(|n| TraceFrame {
            function: format!("f{n}"),
            span,
        })
        .collect();
    let vm_fmt = format_trace(&synthetic_run_error("boom", span, trace));
    let nlines = vm_fmt.lines().count();
    // 1 header + 10 head + 1 elision marker + 10 tail = 22.
    assert_eq!(nlines, 22, "got:\n{vm_fmt}");
    assert!(vm_fmt.contains("frames elided"), "got:\n{vm_fmt}");
    assert!(vm_fmt.contains("at f0 (called at"), "head, got:\n{vm_fmt}");
    assert!(vm_fmt.contains("at f49 (called at"), "tail, got:\n{vm_fmt}");
    // No collapse marker — all names distinct.
    assert!(!vm_fmt.contains("identical frames"), "got:\n{vm_fmt}");

    // Parity: interp renders the identical synthetic trace.
    let ip_trace: Vec<TraceFrame> = (0..50)
        .map(|n| TraceFrame {
            function: format!("f{n}"),
            span,
        })
        .collect();
    let ip_fmt = format_trace(&synthetic_run_error("boom", span, ip_trace));
    assert_eq!(vm_fmt, ip_fmt, "engines must agree on the capped trace");
}

/// Gap #8 cap boundary: with many short same-name runs the collapse emits `× N` markers; the cap
/// must never split a marker from its `at` line (markers stay inside their entry). Every `× N`
/// line must be immediately preceded by its `at` line, head and tail alike.
#[test]
fn format_trace_cap_never_orphans_collapse_marker() {
    let span = Span::RUNTIME;
    // 25 names × 2 consecutive frames each → 25 collapsed entries (each an `at` line + `× 1`
    // marker), > TRACE_HEAD + TRACE_TAIL so the cap fires across marker-bearing entries.
    let trace: Vec<TraceFrame> = (0..25)
        .flat_map(|n| {
            let f = format!("g{n}");
            [
                TraceFrame {
                    function: f.clone(),
                    span,
                },
                TraceFrame { function: f, span },
            ]
        })
        .collect();
    let vm_fmt = format_trace(&synthetic_run_error("boom", span, trace));
    let lines: Vec<&str> = vm_fmt.lines().collect();
    assert!(
        vm_fmt.contains("frames elided"),
        "cap must fire, got:\n{vm_fmt}"
    );
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("… (×") {
            let prev = lines.get(i - 1).copied().unwrap_or("");
            assert!(
                prev.trim_start().starts_with("at "),
                "orphaned collapse marker at line {i}; prev={prev:?}\n{vm_fmt}"
            );
        }
    }
    // Parity with interp on the same synthetic trace.
    let ip_trace: Vec<TraceFrame> = (0..25)
        .flat_map(|n| {
            let f = format!("g{n}");
            [
                TraceFrame {
                    function: f.clone(),
                    span,
                },
                TraceFrame { function: f, span },
            ]
        })
        .collect();
    let ip_fmt = format_trace(&synthetic_run_error("boom", span, ip_trace));
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must agree on the capped collapsed trace"
    );
}

/// A `recover:`-caught fault leaves no stale frames: a *later* uncaught fault's trace shows only
/// its own chain, not the recovered call.
#[test]
fn recovered_fault_does_not_pollute_later_trace() {
    let src = "fn boom() -> int:\n    return 1 / 0\nfn safe() -> int:\n    r := recover:\n        boom()\n    return 7\nfn deeper() -> int:\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(safe())\n    print(deeper())\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_recover_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rt.chz");
    std::fs::write(&path, src).unwrap();
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("should fault");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        names,
        vec!["deeper", "main"],
        "no stale 'boom'/'safe' frames"
    );
}

/// A `defer`red call that itself faults supersedes the original fault (Go semantics); the trace
/// must reflect the DEFERRED fault's chain (deeper, includes the deferred fn) —
/// not the original body fault's chain.
#[test]
fn deferred_fault_trace_supersedes_on_both_engines() {
    let src = "fn boom() -> int:\n    return 1 / 0\nfn worker() -> int:\n    defer boom()\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(worker())\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_defer_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dt.chz");
    std::fs::write(&path, src).unwrap();
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("should fault");
    let vm_names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        vm_names,
        vec!["boom", "worker", "main"],
        "deferred fault's chain"
    );
    let vm_fmt = format_trace(&e);
    let (_o2, _e2, ip_res, _) = run_file(&path);
    let ie = ip_res.expect_err("should fault");
    let ip_fmt = format_trace(&ie);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must agree on a deferred-fault trace"
    );
}

/// gaps.md B4: an uncaught fault thrown from an `Executor.submit(...)` closure must print the SAME
/// backtrace frames. Previously the cooperative engine drained the submitted task INLINE on the
/// entry `Vm`, so the task's callee frames survived into `fault_trace` and printed `at boom` /
/// `at <closure>` / `at main`, while M:N ran each task on an isolated worker `Vm` and discarded that
/// worker's trace, printing only `at main`. That printed `[main]` — matching a plain
/// nursery-task panic (also `at main`, the neighbor guard below).
///
/// TICKET-195 changed all three to the FAULTING party's frames: the job's `[boom, <closure>]`, the
/// child's `[boom, <spawned task>]`, at a join or at the exit drain alike (Go prints the panicking
/// goroutine's stack; CPython the job's traceback). TICKET-289 then moved a job's fault into its Task: a
/// job fault stays in its Task, so the Executor cells below assert a clean run.
#[test]
fn executor_task_fault_trace_matches_on_both_engines() {
    let dir = std::env::temp_dir().join("chezzi_b4_executor_trace");
    std::fs::create_dir_all(&dir).unwrap();

    // Executor.submit path (TICKET-289): the job's fault is its Task's outcome. The run does not
    // fault and `t.get()` gives the message, on both runs.
    let ex_src = "import Executor from std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    t := ex.submit(fn(): boom())\n    ex.shutdown()\n    match t.get():\n        ?_: print(\"value\")\n        !e: print(\"task {e.message()}\")\nmain()\n";
    let ex_path = dir.join("ex.chz");
    std::fs::write(&ex_path, ex_src).unwrap();
    let (so, _e, se, _) = run_file(&ex_path);
    let (mo, _e, mn, _) = run_file(&ex_path);
    se.expect("a job fault does not end the run");
    mn.expect("M:N: a job fault does not end the run");
    assert_eq!(so, "task kaboom\n", "the fault is in the Task");
    assert_eq!(so, mo, "same output on both runs");

    // Neighbor guard: a plain nursery-task panic (non-Executor) prints the child's frames too.
    let nu_src = "fn boom():\n    panic(\"kaboom\")\nfn main():\n    parallel:\n        spawn: boom()\nmain()\n";
    let nu_path = dir.join("nu.chz");
    std::fs::write(&nu_path, nu_src).unwrap();
    let (_o, _e, nse, _) = run_file(&nu_path);
    let (_o, _e, nmn, _) = run_file(&nu_path);
    let nse = nse.expect_err("nursery should fault");
    let nmn = nmn.expect_err("M:N nursery should fault");
    let nse_names: Vec<&str> = nse.trace.iter().map(|f| f.function.as_str()).collect();
    let nmn_names: Vec<&str> = nmn.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        nse_names,
        vec!["boom", "<spawned task>"],
        "nursery trace == the child's"
    );
    assert_eq!(
        nmn_names,
        vec!["boom", "<spawned task>"],
        "M:N nursery trace == the child's"
    );

    // B4 review edge 1 (TICKET-289): IMPLICIT end-of-program drain (no `ex.shutdown()`). The drain
    // joins the executor and raises no job fault, so the run ends clean.
    let im_src = "import Executor from std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    _ := ex.submit(fn(): boom())\nmain()\n";
    let im_path = dir.join("implicit.chz");
    std::fs::write(&im_path, im_src).unwrap();
    let (_o, _e, ise, _) = run_file(&im_path);
    let (_o, _e, imn, _) = run_file(&im_path);
    ise.expect("the implicit drain raises no job fault");
    imn.expect("M:N: the implicit drain raises no job fault");

    // B4 review edge 2 (TICKET-289): `defer ex.shutdown()` while `main` is unwinding from an outer
    // panic. The dropped job's fault stays in its Task, so `main`'s own fault is the report.
    let df_src = "import Executor from std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    _ := ex.submit(fn(): boom())\n    defer ex.shutdown()\n    panic(\"outer\")\nmain()\n";
    let df_path = dir.join("defer_unwind.chz");
    std::fs::write(&df_path, df_src).unwrap();
    let (_o, _e, dse, _) = run_file(&df_path);
    let (_o, _e, dmn, _) = run_file(&df_path);
    let dse = dse.expect_err("defer-unwind should fault");
    let dmn = dmn.expect_err("M:N defer-unwind should fault");
    assert_eq!(dse.message, "outer", "main's fault is the report");
    assert_eq!(dse.message, dmn.message, "defer-unwind same fault message");
    assert_eq!(dse.span, dmn.span, "defer-unwind same fault location");
}

/// Non-constant default golden: `examples/default_expr.chz` — defaults that are arithmetic on
/// literals, a global times a literal, and a function call (free fns + struct fields). Byte-matches
/// `.expected`, identical on interp + VM.
#[test]
fn golden_default_expr_via_run_file() {
    let path = fixture("examples/default_expr.chz");
    let expected = std::fs::read_to_string(fixture("examples/default_expr.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Function-typed field call golden: `examples/fn_field.chz` — `recv.f(args)` where `f` is a
/// `fn`-typed field resolves to field-access-then-call (on `self` and on an external receiver),
/// not a method. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_fn_field_via_run_file() {
    let path = fixture("examples/fn_field.chz");
    let expected = std::fs::read_to_string(fixture("examples/fn_field.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `sort_by_key` golden: `examples/sort_by_key.chz` — sort in place by a derived key (int/str
/// keys, stable, descending-via-negation, and a Comparable *struct* key). Byte-matches
/// `.expected`, identical on interp + VM.
#[test]
fn golden_sort_by_key_via_run_file() {
    let path = fixture("examples/sort_by_key.chz");
    let expected = std::fs::read_to_string(fixture("examples/sort_by_key.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `const T` golden: `examples/const_binding.chz` — the immutable binding modifier. A module-global
/// const, a runtime-initialized const (const ≠ constexpr), reading + aliasing a const, SHALLOW
/// mutation of a `const` List (name frozen, contents mutable), and a const local. `const` is
/// compile-time-only (never lowered), so the VM engines are byte-identical by construction.
#[test]
fn golden_const_binding_via_run_file() {
    let path = fixture("examples/const_binding.chz");
    let expected = std::fs::read_to_string(fixture("examples/const_binding.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Tuple destructuring + match-on-tuple + guards golden: `examples/tuple_match.chz` — `a, b :=
/// fn()`, typed tuple value + `.0`/`.1`, `match` literal/binding/guard arms, `Some((a, b))`.
/// Coverage for behavior that already worked. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_tuple_match_via_run_file() {
    let path = fixture("examples/tuple_match.chz");
    let expected = std::fs::read_to_string(fixture("examples/tuple_match.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.os.exit(code)` golden: `examples/exit.chz` halts at the negative branch with status 2.
/// Byte-matches `.expected` and both report the same exit code.
#[test]
fn golden_exit_via_run_file() {
    let path = fixture("examples/exit.chz");
    let expected = std::fs::read_to_string(fixture("examples/exit.expected")).unwrap();
    let (vo, _ve, vr, vc) = run_file(&path);
    let (io, _ie, ir, ic) = run_file(&path);
    assert!(
        vr.is_ok() && ir.is_ok(),
        "exit is a clean halt: vm={vr:?} interp={ir:?}"
    );
    assert_eq!(vo, expected, "vm stdout");
    assert_eq!(io, expected, "interp stdout");
    assert_eq!(vc, Some(2), "vm exit code");
    assert_eq!(ic, Some(2), "interp exit code");
}

/// M8-M2 golden: `examples/json_dynamic.chz` — `import std.json`, the pure-Chezzi `Json` enum
/// parse/stringify round-trip + accessors + unicode escapes + an error case. Byte-matches
/// `.expected` and stays identical on interp + VM.
#[test]
fn golden_json_dynamic_via_run_file() {
    let path = fixture("examples/json_dynamic.chz");
    let expected = std::fs::read_to_string(fixture("examples/json_dynamic.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// M9 golden: `examples/regex_demo.chz` — `import std.regex` (is_match / find with capture
/// groups / find_all / replace_all / split + a bad-pattern Err). Byte-matches `.expected` and
/// stays identical on interp + VM.
#[test]
fn golden_regex_demo_via_run_file() {
    let path = fixture("examples/regex_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/regex_demo.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Golden: `examples/knapsack.chz` fills an int DP table with `cmp.max` (std.cmp generic over
/// Comparable). Runs on the VM, byte-matches `.expected`, and stays identical to the interp.
#[test]
fn golden_knapsack_via_run_file() {
    let path = fixture("examples/knapsack.chz");
    let expected = std::fs::read_to_string(fixture("examples/knapsack.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `Iterator[T]` golden: a generic fn bounded `[S: Iterator[T], T]` over list/str/set/struct,
/// with the element type flowing into returns.
#[test]
fn golden_iterator_bound_via_run_file() {
    let path = fixture("examples/iterator_bound.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterator_bound.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Lazy iterator adapters (Take/Mapped over an infinite Count) — the no-`yield` story. The inner
/// `self.inner.next()` recovers the element type through the `I: Iterator[T]` bound.
#[test]
fn golden_iter_adapters_via_run_file() {
    let path = fixture("examples/iter_adapters.chz");
    let expected = std::fs::read_to_string(fixture("examples/iter_adapters.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `Iterable[T]` + `.iter()` — a list flows into the Take/Mapped adapter pipeline, `.iter()`+manual
/// `.next()` on every collection, a pure-`Iterable` struct drives `for`, an `[S: Iterable[T]]` fn
/// over a list AND a struct iterator, empty/idempotent cursors, `List(xs.iter())` round-trip. Byte-
/// identical across runs.
#[test]
fn golden_iterable_via_run_file() {
    let path = fixture("examples/iterable.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterable.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

#[test]
fn golden_multi_file_project_via_vm() {
    let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/proj/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, expected);
}

/// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
/// constant must resolve against *its own* module, not the caller — even when the caller
/// defines a same-named global with a different value.
#[test]
fn imported_fn_uses_home_globals() {
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, "from-lib\nfrom-main\n");
}

/// Whole multi-file project is byte-identical under GC stress.
#[test]
fn multi_file_identical_under_gc_stress() {
    // The fixture is small; run it under stress by routing through the entry graph manually.
    let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
    let graph = crate::resolver::build_graph(&fixture("tests/fixtures/proj/main.chz")).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.run().unwrap();
    assert_eq!(captured(vm.out), expected);
}

// ----- map / dictionary parity (gap #5) -----

#[test]
fn parity_map_missing_key_read_errors() {
    // Both engines must error identically on a missing key.
    let src = "m := {\"a\": 1}\nprint(m[\"z\"])\n";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 2, col 7): key not found: 'z'",
        "for:\n{src}"
    );
    assert!(
        vm_outcome(src).unwrap_err().contains("key not found"),
        "{:?}",
        vm_outcome(src)
    );
}

#[test]
fn parity_map_compound_assign_missing_key_errors() {
    // Compound on a missing key is an error (consistent with read-missing).
    let src = "m := {\"a\": 1}\nm[\"z\"] += 1\n";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 2, col 1): key not found: 'z'",
        "for:\n{src}"
    );
    assert!(
        vm_outcome(src).unwrap_err().contains("key not found"),
        "{:?}",
        vm_outcome(src)
    );
}
