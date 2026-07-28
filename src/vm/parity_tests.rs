// Extracted from vm/mod.rs (test module). `super::` == the `vm` module.
//! Cross-engine parity: the serial VM (`parallel=false`) and the M:N VM (`parallel=true`) must
//! agree on stdout *and* error for every program. NB: both drive the same `Vm` bytecode, so for a
//! sequential program this is a determinism check on one engine; the differential bite is on
//! concurrent programs (scheduler/airlock/fault-report), where the two paths genuinely differ.
//! (Historically these compared the VM against a separate tree-walk interpreter, since removed.)
use super::*;
use std::path::PathBuf;

/// Outcome of a run in the sink's RAW BYTES — what the ORACLE compares (see
/// [`assert_outcome_parity`]). `from_utf8_lossy` is not injective, so a decoded compare would pass a
/// run whose two engines emitted DIFFERENT invalid UTF-8 (W6-9b).
fn parallel_outcome_bytes(src: &str) -> Result<Vec<u8>, String> {
    run_capture_parallel_bytes(src).map_err(|e| e.to_string())
}
fn vm_outcome_bytes(src: &str) -> Result<Vec<u8>, String> {
    run_capture_bytes(src).map_err(|e| e.to_string())
}

/// The decoded shape, for SINGLE-ENGINE assertions only — `assert_eq!(vm_outcome(src).unwrap(),
/// "6\n")`, `.contains(..)`, the fn-pointer arrays. These are NOT oracles: they pin one engine's
/// output against a literal, so the decode can't hide anything. The cross-engine oracle is
/// [`assert_outcome_parity`], which compares bytes.
fn parallel_outcome(src: &str) -> Result<String, String> {
    parallel_outcome_bytes(src).map(captured)
}
fn vm_outcome(src: &str) -> Result<String, String> {
    vm_outcome_bytes(src).map(captured)
}

/// Cross-engine compare of two capture outcomes: text first (a readable failure), then the RAW
/// BYTES on top — the compare that catches a divergence the lossy decode erases. Mirrors
/// [`assert_file_parity`]/[`assert_stream_parity`].
fn assert_outcome_parity(vm: &Result<Vec<u8>, String>, mn: &Result<Vec<u8>, String>, src: &str) {
    let text = |r: &Result<Vec<u8>, String>| {
        r.as_ref()
            .map(|b| captured(b.clone()))
            .map_err(String::clone)
    };
    assert_eq!(text(vm), text(mn), "VM/interp divergence for:\n{src}");
    assert_eq!(
        vm, mn,
        "VM/interp BYTE divergence for:\n{src} (equal only after a lossy decode)"
    );
}

fn assert_parity(src: &str) {
    assert_outcome_parity(&vm_outcome_bytes(src), &parallel_outcome_bytes(src), src);
}

/// W6-9b — the capture oracle must diff BYTES. Two engines emitting different invalid UTF-8
/// (`ff fe` vs `fe ff`) decode to the same U+FFFD run, so a `String` compare reports parity OK on a
/// byte-divergent run. Direct on the helper: `run_capture*` compiles standalone (no module graph,
/// hence no `import std.io`), so no real program can reach this path with non-UTF-8 output — the
/// file oracle's end-to-end proof is `file_parity_catches_a_byte_only_divergence`.
#[test]
fn outcome_parity_catches_a_byte_only_divergence() {
    let a: Result<Vec<u8>, String> = Ok(vec![0xff, 0xfe]);
    let b: Result<Vec<u8>, String> = Ok(vec![0xfe, 0xff]);
    assert!(
        std::panic::catch_unwind(|| assert_outcome_parity(&a, &b, "<synthetic>")).is_err(),
        "a byte-only divergence must FAIL the capture parity oracle"
    );
}

/// Native-prelude phase 2b — the GENERIC / reserved-type container CTORS (range/List/Map/Set) are
/// now sourced from the synthetic PRELUDE table (`Intrinsic::Ctor`) for their `CallBuiltin`
/// DISPATCH, exactly as 2a did for the scalar ctors. Their generic type-identity resolution is
/// untouched, so every construction + generic-inference path must stay byte-identical on BOTH
/// engines (VM == interp): range overloads, empty/turbofished containers, dedup, iteration.
#[test]
fn container_ctor_parity() {
    let src = r#"
fn main():
    print(range(5).len())
    for i in range(1, 10, 2):
        print(i)
    xs := List[int]()
    xs.push(10)
    xs.push(20)
    print(xs.len())
    print(xs[0])
    ss := List[str]()
    ss.push("a")
    print(ss[0])
    ys := List()
    print(ys.len())
    m := Map()
    m["k"] = 1
    print(m["k"])
    m2 := Map[str, int]()
    print(m2.len())
    s := Set([1, 1, 2, 3, 3])
    print(s.len())
    s2 := Set[int]()
    print(s2.len())

main()
"#;
    assert_parity(src);
}

/// M19 SSO — string ops must stay byte-identical across both engines for strings that straddle
/// the `ChzStr` inline/heap boundary (`INLINE_CAP` = 22 bytes), including multi-byte UTF-8.
/// Exercises concat, split/join, indexing, iteration, `==`, `.chars()`, and string map keys.
#[test]
fn sso_boundary_string_ops_parity() {
    let src = r#"
fn main():
    a := "aaaaaaaaaaaaaaaaaaaaa"      # 21 bytes (inline)
    b := "bbbbbbbbbbbbbbbbbbbbbb"     # 22 bytes (inline boundary)
    c := "ccccccccccccccccccccccc"    # 23 bytes (heap)
    print(a.len())
    print(b.len())
    print(c.len())

    # concat crosses the boundary in both directions
    ab := a + b
    print(ab.len())
    print(ab)
    print(a + "z")                    # 22, still inline
    print(b + "z")                    # 23, spills to heap

    # equality across storage (built two ways)
    print(b == "b" + "bbbbbbbbbbbbbbbbbbbbb")
    print((a + b) == (a + b))

    # indexing + iteration over a heap-length string
    print(c[0])
    print(c[22])
    n := 0
    for ch in c:
        n += 1
    print(n)

    # slice producing results on both sides of the boundary (slice itself allocs a str)
    print(c[0:22])                    # 22 bytes — inline
    print(c[0:23])                    # 23 bytes — heap
    print(ab[0:22])

    # case-fold can change byte length, straddling the boundary either way
    print(b.upper())                  # 22 ascii → 22, inline
    print(c.lower())                  # 23 → 23, heap
    print("héllo-wörld-straße".upper())   # multibyte fold (ß→SS grows length)

    # split / join round trip straddling the boundary
    joined := "left-segment-twelve,right-side-thirteen"
    bits := joined.split(",")
    print(bits[0])
    print(bits[1])
    print(",".join(bits) == joined)

    # f-string interpolation growing past the boundary
    i := 0
    while i < 5:
        print("prefix-pad-prefix-pad-{i}")   # ~22-23 bytes, straddles
        i += 1

    # multi-byte UTF-8: short (inline by bytes) and long (heap)
    m := "héllo wörld"                # 13 bytes, 11 chars — inline
    print(m.len())
    print(m.chars().len())
    big := "ñññññññññññññññ"          # 15 chars × 2 bytes = 30 — heap
    print(big.len())
    print(big.chars().len())

    # string map keys straddling the boundary
    mm := {a: 1, c: 2}
    print(mm[a])
    print(mm[c])

main()
"#;
    assert_parity(src);
}

use std::sync::atomic::{AtomicUsize, Ordering};
static PARITY_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = PARITY_TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_par_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// ONE-WAY RATCHET (supersedes commit 9829f94): a reserved builtin TYPE name used as a user
/// generic type parameter is REJECTED at check time with `type 'X' is reserved (builtin)` — it is
/// NOT silently shadowed into a usable type param. 9829f94 made these shadow-and-run (the
/// behavior this guards-against now), which violated the reserved-name discipline (a scalar param
/// is dead/unreferenceable; a container/enum-builtin param silently shadows the builtin). The
/// reject happens in `check_graph` (the gate the bare `run_capture` helpers skip), so the program
/// never reaches any engine — three-engine parity is by construction (no run needed). Asserted via
/// the full `build_graph` + `check_graph` CLI path.
#[test]
fn type_param_named_like_reserved_rejected_at_check() {
    for name in [
        "Executor",
        "ptr",
        "Socket",
        "Listener",
        "Writer",
        "Reader",
        "owned_str",
    ] {
        let src = format!(
            "fn id[{name}](x: {name}) -> {name}:\n    return x\nfn main():\n    print(id(1))\nmain()\n"
        );
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let graph = crate::resolver::build_graph(&entry).expect("resolve");
        match crate::checker::check_graph(&graph) {
            Ok(()) => panic!("type param named {name} must be rejected as reserved (builtin)"),
            Err(errs) => assert!(
                errs.iter()
                    .any(|e| e.message.contains("reserved (builtin)")),
                "type param named {name} must error reserved (builtin), got: {errs:?}"
            ),
        }
    }
}

/// Cross-engine compare of ONE captured stream (stdout or stderr): text first (a readable failure),
/// then the RAW BYTES on top — the compare that catches a divergence the lossy `captured` decode
/// erases (`ff fe` vs `fe ff` both decode to two U+FFFD). Shared by every file-based oracle
/// (`assert_parity_file`, `parity_entry_cfg`, `assert_file_parity`) so they can't drift apart.
fn assert_stream_parity(a: &[u8], b: &[u8], what: &str, label: &str) {
    let text = |x: &[u8]| String::from_utf8_lossy(x).into_owned();
    assert_eq!(text(a), text(b), "{what} divergence {label}");
    assert_eq!(
        a, b,
        "{what} BYTE divergence {label} (equal only after a lossy decode)"
    );
}

/// Run a multi-file program (one or more `.chz` files) through BOTH engines via `run_file`,
/// assert they agree on stdout and on ok/err, and return the agreed stdout. `files` is
/// `(relative_path, contents)`; `entry` names the file to run. Needed because the single-file
/// `assert_parity` can't exercise imports (and std modules require the import path).
fn assert_parity_file(files: &[(&str, &str)], entry: &str) -> String {
    let t = TmpDir::new();
    let mut entry_path = None;
    for (rel, contents) in files {
        let p = t.write(rel, contents);
        if *rel == entry {
            entry_path = Some(p);
        }
    }
    let entry_path = entry_path.expect("entry must be one of the files");
    // RAW BYTES on both legs (W6-9b): `run_file_p`/`run_file` hand back the lossily-decoded capture,
    // which would fold a genuine non-UTF-8 divergence (`ff` vs `fe`) into equal U+FFFDs. Same
    // arguments as those two wrappers (default `HostConfig`, no entry fn, no pinned root).
    let raw = |parallel| {
        crate::vm::run_file_bytes(
            &entry_path,
            crate::native::HostConfig::default(),
            parallel,
            None,
            None,
        )
    };
    let (io, ie_out, ir, _) = raw(true);
    let (vo, ve_out, vr, _) = raw(false);
    let label = format!("(interp vs vm) for entry {entry}");
    assert_stream_parity(&io, &vo, "stdout", &label);
    assert_stream_parity(&ie_out, &ve_out, "stderr", &label);
    match (&ir, &vr) {
        (Ok(()), Ok(())) => {}
        (Err(ie), Err(ve)) => {
            assert_eq!(
                ie.to_string(),
                ve.to_string(),
                "error divergence (interp vs vm)"
            );
        }
        _ => panic!("ok/err divergence: interp={ir:?} vm={vr:?}"),
    }
    captured(io)
}

/// Convenience: a single entry file (the common std-module case).
fn parity_entry(src: &str) -> String {
    assert_parity_file(&[("main.chz", src)], "main.chz")
}

/// W6-9b, end-to-end — the FILE oracle on a REAL byte-divergent program (the fixture
/// `tests/check_parity.rs::check_parity_reports_a_byte_only_divergence` already pins through the
/// CLI). The channel orders the two tasks, so each engine's byte order is deterministic: serial
/// prints live (`fe ff`), M:N flushes each task's slot in task order (`ff fe`). Both decode to two
/// U+FFFD, so only a byte-level diff sees it. CANARY: if M:N slot ordering ever changes so the
/// engines agree, this flips to failing — fix the ordering or the fixture, do NOT weaken the
/// compare (the CLI pin would move with it).
#[test]
fn file_parity_catches_a_byte_only_divergence() {
    let src = "import std.io\n\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            _ := ch.recv()\n            _ := io.stdout().write_bytes(b\"\\xff\")\n        spawn:\n            _ := io.stdout().write_bytes(b\"\\xfe\")\n            ch.send(1)\nmain()\n";
    let src = src.to_string();
    assert!(
        std::panic::catch_unwind(move || parity_entry(&src)).is_err(),
        "a byte-only divergence must FAIL the file parity oracle"
    );
}

/// Like [`parity_entry`], but for a program that must FAULT on BOTH engines: runs the graph path on
/// the serial + M:N engines, asserts both error with an identical message, and returns that message
/// so the caller can assert its content.
#[cfg(test)]
fn parity_entry_fault(src: &str) -> String {
    let t = TmpDir::new();
    let p = t.write("main.chz", src);
    let (_io, _ie, ir, _) = run_file_p(&p);
    let (_vo, _ve, vr, _) = run_file(&p);
    let ie = ir.expect_err("M:N engine must fault");
    let ve = vr.expect_err("serial engine must fault");
    assert_eq!(ve.to_string(), ie.to_string(), "serial == M:N fault");
    ve.to_string()
}

// ----- Task 2 (option a): user protocol existentials cross the airlock -----

/// A concrete struct widened to a user protocol existential rides a `Channel[Proto]` to a spawned
/// task, which calls a protocol method and prints — byte-identical serial == M:N. (Was checker-
/// rejected pre-change: `Channel[Drawable]` "must be sendable".)
#[test]
fn protocol_value_crosses_channel_three_engine() {
    let src = "protocol Drawable:\n    fn draw(self) -> str\nstruct Sq:\n    s: int\n    fn draw(self) -> str:\n        return \"sq\"\nfn main():\n    ch := Channel[Drawable]()\n    d: Drawable = Sq(1)\n    ch.send(d)\n    parallel:\n        spawn:\n            print(ch.recv().draw())\nmain()\n";
    assert_parity(src);
}

/// An FFI fn (`Cffi`) crosses the wire-value airlock BY VALUE (its shared `Arc<Cffi>`, the same the
/// snapshot path already ships across M:N workers) — a spawn fn-ARG. `cos` reaches the task and
/// computes `cos(0.0) == 1.0`, byte-identical serial == M:N. (Was runtime-rejected pre-fix: an extern
/// fn's type is `fn(float)->float` (`Ty::Func`, checker-sendable), so the runtime airlock was the sole
/// — and wrong — gate; a Cffi is pure code, not a heap-local handle, so it now crosses.)
#[test]
fn ffi_handle_crosses_airlock_three_engine() {
    assert_parity_out(
        "extern \"libm.so.6\":\n    fn cos(x: float) -> float\nfn use_fn(g: fn(float) -> float):\n    print(g(0.0))\nf := cos\nparallel:\n    spawn use_fn(f)\n",
        "1.0\n",
    );
}

/// An FFI fn sent over a `Channel` crosses by value and is callable after `recv` — `sqrt(9.0) == 3.0`,
/// byte-identical on both engines (previously rejected at the `channel_method` "send" value-store).
#[test]
fn ffi_handle_crosses_channel_send() {
    assert_parity_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nch := Channel[fn(float) -> float]()\nch.send(sqrt)\nprint(ch.recv()(9.0))\n",
        "3.0\n",
    );
}

/// An FFI fn stored in a `Shared` box at CONSTRUCTION crosses by value and is callable after `get` —
/// `sqrt(16.0) == 4.0` on both engines (previously rejected at the ctor value-store).
#[test]
fn ffi_handle_crosses_shared_ctor() {
    assert_parity_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nimport std.concurrency\nbox := Shared(sqrt)\nprint(box.get()(16.0))\n",
        "4.0\n",
    );
}

/// An FFI fn stored via `Shared.set` — the STORE path distinct from the ctor — crosses by value and
/// is callable after `get` — `sqrt(25.0) == 5.0` on both engines (previously rejected at the store).
#[test]
fn ffi_handle_crosses_shared_set() {
    assert_parity_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nimport std.concurrency\nbox := Shared(fn(x: float) -> float: x)\nbox.set(sqrt)\nprint(box.get()(25.0))\n",
        "5.0\n",
    );
}

/// The FFI-over-`Channel` send now SUCCEEDS (Ok), byte-identical on both engines: `recover:` sees an
/// `Ok` → prints `false`. (Was `true` when the send rejected; the flip proves the airlock accepts it.)
#[test]
fn ffi_handle_send_succeeds() {
    assert_parity_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nch := Channel[fn(float) -> float]()\nr := recover: ch.send(sqrt)\nmatch r:\n    Ok(v): print(\"false\")\n    Err(e): print(\"true\")\n",
        "false\n",
    );
}

/// MUST-PRESERVE: a legit `Shared` handle STILL crosses a `Channel` (maps to `WireValue::Shared`,
/// `has_handle()` == false) — the guard must not over-reject shared-core handles.
#[test]
fn positive_shared_handle_crosses_channel() {
    assert_parity_out(
        "import std.concurrency\ns := Shared(42)\nch := Channel[Shared[int]]()\nch.send(s)\nprint(ch.recv().get())\n",
        "42\n",
    );
}

/// MUST-PRESERVE: a nested `Shared[Shared[int]]` still constructs (the inner handle stores into the
/// outer box) on both engines — the guard must not over-reject a shared-core store.
#[test]
fn positive_nested_shared_constructs() {
    assert_parity_out(
        "import std.concurrency\nprint(Shared(Shared(1)).get().get())\n",
        "1\n",
    );
}

// ----- named-factory-import member resolution: RUNTIME unaffected (gap #4) -----
// These runs bypass the checker (compile+run directly), so they pass pre- AND post-fix — they lock
// that the checker-only member-resolution fix leaves the runtime output byte-identical on both
// engines. The checker gate itself is covered by the `checker::tests::checker_named_*` battery.

/// A named import of a factory FUNCTION (not its return type) still constructs and dispatches the
/// returned struct's method at runtime → `11`, byte-identical on both engines.
#[test]
fn named_fn_import_factory_struct_method_runtime() {
    let out = assert_parity_file(
        &[
            (
                "lib.chz",
                "struct Widget:\n    n: int\n    fn bump(self) -> int:\n        return self.n + 1\nfn make() -> Widget:\n    return Widget(10)\n",
            ),
            (
                "main.chz",
                "import make from lib\nw := make()\nprint(w.bump())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "11\n");
}

/// Stdlib: `import manual from std.cancel; manual().cancelled()` runs on both engines (documented
/// Token API reachable off a named-imported factory result).
#[test]
fn named_fn_import_stdlib_cancel_runtime() {
    let out = parity_entry("import manual from std.cancel\nt := manual()\nprint(t.cancelled())\n");
    assert_eq!(out, "false\n");
}

/// Stdlib (gaps.md cancel-derive fix): a derived token's `done()` must not fire before its
/// `cancelled()`/`reason()` flip — the Go-context invariant "once done() unblocks, the token is
/// cancelled". `derive()` truncated the child timer's remaining-ms toward zero, arming it up to ~1ms
/// early; the `+ 1` ms margin keeps done() at-or-after the absolute deadline. Deterministic on both
/// engines (the margin makes it load-independent): a task woken by `done()` reads `cancelled()==true`.
#[test]
fn derived_cancel_token_done_implies_cancelled_runtime() {
    let out = parity_entry(
        "import std.cancel\nc := cancel.timeout(10).derive()\n_ := c.done().recv()\nprint(\"{c.cancelled()} {c.reason()}\")\n",
    );
    assert_eq!(out, "true Some(timeout)\n");
}

/// Stdlib: `import min_heap from std.collections; min_heap().push(3)` runs on both engines.
#[test]
fn named_fn_import_stdlib_collections_runtime() {
    let out = parity_entry(
        "import min_heap from std.collections\nh := min_heap()\nh.push(3)\nprint(h.len())\n",
    );
    assert_eq!(out, "1\n");
}

/// gap #4 (satisfies path): a named-fn-imported factory result passed through a protocol-bounded
/// generic constructs and dispatches on both engines → `W10`. The checker gate is covered in
/// `checker::tests::named_fn_import_satisfies_protocol_*`; this locks the RUNTIME output byte-identical.
#[test]
fn named_fn_import_protocol_bound_runtime() {
    let out = assert_parity_file(
        &[
            (
                "lib.chz",
                "struct Widget:\n    n: int\n    fn describe(self) -> str:\n        return \"W{self.n}\"\nfn make() -> Widget:\n    return Widget(10)\n",
            ),
            (
                "main.chz",
                "import make from lib\nprotocol Describable:\n    fn describe(self) -> str\nfn show[T: Describable](x: T):\n    print(x.describe())\nshow(make())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "W10\n");
}

// ----- module-scoped user types: cross-module construction parity -----

/// `import geo; geo.Point(1,2)` constructs and prints `Point(x=1, y=2)` (BARE name, no `::`),
/// byte-identical on both engines.
#[test]
fn qualified_struct_ctor_parity() {
    let out = assert_parity_file(
        &[
            ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
            (
                "main.chz",
                "import geo\np := geo.Point(1, 2)\nprint(p)\nprint(p.x)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "Point(x=1, y=2)\n1\n");
}

/// `import S from types` (struct) constructs bare.
#[test]
fn from_import_struct_ctor_parity() {
    let out = assert_parity_file(
        &[
            ("types.chz", "struct S:\n    n: int\n"),
            ("main.chz", "import S from types\nprint(S(7))\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "S(n=7)\n");
}

/// Task 3/5: the module-owned synthetic structs (`Match`/`Response`/`ProcResult`) are now
/// USER-CONSTRUCTIBLE once their module is imported — the VM and the frozen interp must build +
/// field-read them BYTE-IDENTICALLY (the layout seeded in `Compiler::new` ↔ the interp's native
/// seed must agree). Qualified ctor + bare ctor on whole-module import.
#[test]
fn synthetic_struct_qualified_ctor_parity() {
    let out = parity_entry(
        "import std.regex\nm := regex.Match(\"hi\", 0, 2, [\"a\"])\nprint(m.text + str(m.start) + str(m.end) + \",\".join(m.groups))\n",
    );
    assert_eq!(out, "hi02a\n");
}

#[test]
fn synthetic_struct_bare_ctor_on_import_parity() {
    let out = parity_entry(
        "import std.regex\nm: Match = Match(\"yo\", 1, 3, [])\nprint(m.text + str(m.end))\n",
    );
    assert_eq!(out, "yo3\n");
}

/// `Match.start`/`.end` are CODEPOINT offsets, so slicing the subject by them reproduces `.text` on
/// non-ASCII input too (Chezzi slicing is codepoint-indexed; the regex crate's byte spans are
/// converted at the native seam). Both engines must agree — they were consistently WRONG together
/// before ("lo"), which is why parity alone never caught this.
#[test]
fn regex_offsets_are_codepoint_slicable_parity() {
    let src = "import std.regex\n\
                   s := \"héllo\"\n\
                   match regex.find(\"l+\", s):\n\
                   \x20   Ok(opt):\n\
                   \x20       match opt:\n\
                   \x20           Some(m):\n\
                   \x20               print(s[m.start:m.end])\n\
                   \x20               print(str(s[m.start:m.end] == m.text))\n\
                   \x20           None: print(\"none\")\n\
                   \x20   Err(e): print(e)\n";
    let out = parity_entry(src);
    assert_eq!(out, "ll\ntrue\n");
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (mn_out, _e, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("regex codepoint-offset program should run on the M:N engine");
    assert_eq!(
        mn_out, out,
        "M:N diverged from serial on regex codepoint offsets"
    );
}

/// Phase 4b — the whole regex SIGNATURE (the `native struct Match` + the 5 `native fn`s) now comes
/// from the file-backed `std/regex.chz` (harvested into std.regex's ModuleSig via
/// `harvest_native_module`), replacing BOTH the phase-4a companion stub and the hand-built
/// `native_module_sig` arm. This must be a ZERO observable behavior change on ALL THREE engines:
/// producing a `Match` (regex.find), reading every field, `from std.regex import Match`, and
/// qualified `regex.Match(...)` are byte-identical on interp, the cooperative VM, AND the M:N
/// OS-thread engine. This is the regression net proving the native-seam pure-type `bind_import` skip
/// still holds in both engines (`from std.regex import Match` must NOT bind a runtime value / fault).
/// The origin=Builtin force in `harvest_native_module` is what keeps that skip correct.
#[test]
fn regex_match_file_backed_three_engine_parity() {
    let src = "import std.regex\n\
                   import Match from std.regex\n\
                   fn describe(m: Match) -> str:\n\
                   \x20   return m.text + \"@\" + str(m.start) + \"-\" + str(m.end) + \":\" + \",\".join(m.groups)\n\
                   lit: Match = regex.Match(\"lit\", 9, 12, [\"g\"])\n\
                   print(describe(lit))\n\
                   match regex.find(\"[0-9]+\", \"a12b\"):\n\
                   \x20   Ok(opt):\n\
                   \x20       match opt:\n\
                   \x20           Some(m): print(describe(m))\n\
                   \x20           None: print(\"none\")\n\
                   \x20   Err(e): print(e)\n";
    // VM(serial) + interp byte-identical (the standard two-engine parity gate).
    let out = parity_entry(src);
    assert_eq!(out, "lit@9-12:g\n12@1-3:\n");
    // …and the M:N OS-thread engine agrees (needs the graph path — write a file, drive run_file_*).
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (mn_out, _e, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("regex Match stub program should run on the M:N engine");
    assert_eq!(
        mn_out, out,
        "M:N output diverged from VM/interp on regex Match stub"
    );
}

/// from-import `Response` (the `import Name from module` selective form).
#[test]
fn synthetic_struct_from_import_ctor_parity() {
    let out = parity_entry(
        "import Response from std.request\nr := Response(200, \"ok\", {\"k\": \"v\"})\nprint(str(r.status) + r.body + r.headers[\"k\"])\n",
    );
    assert_eq!(out, "200okv\n");
}

#[test]
fn synthetic_procresult_qualified_ctor_parity() {
    let out = parity_entry(
        "import std.process\np := process.ProcResult(\"out\", \"err\", 7)\nprint(p.stdout + p.stderr + str(p.code))\n",
    );
    assert_eq!(out, "outerr7\n");
}

/// Phase 4f — std.process + std.request are now FILE-BACKED (`std/process.chz`, `std/request.chz`),
/// their whole signature (fields-only `native struct` ProcResult/Response + the `native fn`s, incl.
/// request's OPTIONAL trailing `timeout_ms`) harvested via `harvest_native_module`, retiring both the
/// `native_module_sig` fn arms AND the `export_struct` arms. This must be a ZERO observable behavior
/// change on ALL THREE engines: constructing ProcResult/Response, reading every field, `from std.X
/// import Type`, qualified `X.Type(...)`, and request.get with AND without the optional `timeout_ms`
/// are byte-identical on interp, the cooperative VM, AND the M:N OS-thread engine. (Only ctors +
/// field reads are exercised at runtime — request.get would need a live server — but the optional
/// `timeout_ms` arg still TYPECHECKS both ways here, proving the harvested optional-tail sig.)
#[test]
fn process_request_file_backed_three_engine_parity() {
    let src = "import std.process\n\
                   import std.request\n\
                   import ProcResult from std.process\n\
                   import Response from std.request\n\
                   fn describe_proc(p: ProcResult) -> str:\n\
                   \x20   return p.stdout + \"|\" + p.stderr + \"|\" + str(p.code)\n\
                   fn describe_resp(r: Response) -> str:\n\
                   \x20   return str(r.status) + \"|\" + r.body + \"|\" + r.headers[\"k\"]\n\
                   pr: ProcResult = process.ProcResult(\"out\", \"err\", 7)\n\
                   rp: Response = request.Response(200, \"body\", {\"k\": \"v\"})\n\
                   print(describe_proc(pr))\n\
                   print(describe_resp(rp))\n";
    // VM(serial) + interp byte-identical.
    let out = parity_entry(src);
    assert_eq!(out, "out|err|7\n200|body|v\n");
    // …and the M:N OS-thread engine agrees.
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (mn_out, _e, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("process/request ctor program should run on the M:N engine");
    assert_eq!(
        mn_out, out,
        "M:N output diverged from VM/interp on process/request ctors"
    );
}

/// Phase 4f — the pure TYPE `import ProcResult from std.process` (no runtime module-member value)
/// must NOT fault on EITHER engine (the both-engine `bind_import` skip). A single-engine fault = red.
#[test]
fn pure_type_import_no_fault_both_engines() {
    let out = parity_entry("import ProcResult from std.process\nprint(1)\n");
    assert_eq!(out, "1\n");
    let out2 = parity_entry("import Response from std.request\nprint(2)\n");
    assert_eq!(out2, "2\n");
}

/// A user `struct Response` WITHOUT `import std.request` is the user's OWN type on both engines —
/// the synthetic name is freed (the `Builtin`-origin seed is shadowed by the user declaration).
#[test]
fn user_struct_shadows_synthetic_name_parity() {
    assert_eq!(
        parity_entry("struct Response:\n    code: int\nr := Response(7)\nprint(str(r.code))\n"),
        "7\n"
    );
    assert_eq!(
        parity_entry("struct Match:\n    score: int\nm := Match(3)\nprint(str(m.score))\n"),
        "3\n"
    );
}

/// `import geo` then `geo.Color.Red` (nullary) and `geo.Shape.Circle(5)` (payload) construct.
#[test]
fn qualified_enum_ctor_parity() {
    let out = assert_parity_file(
        &[
            (
                "geo.chz",
                "enum Color:\n    Red\n    Green\nenum Shape:\n    Circle(int)\n",
            ),
            (
                "main.chz",
                "import geo\nc := geo.Color.Red\ns := geo.Shape.Circle(5)\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Green: print(\"green\")\nmatch s:\n    Shape.Circle(r): print(r)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "red\n5\n");
}

/// `import Color from types` (enum) constructs bare.
#[test]
fn from_import_enum_ctor_parity() {
    let out = assert_parity_file(
        &[
            ("types.chz", "enum Color:\n    Red\n    Green\n"),
            (
                "main.chz",
                "import Color from types\nc := Color.Green\nmatch c:\n    Color.Red: print(\"r\")\n    Color.Green: print(\"g\")\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "g\n");
}

/// Two modules both declare `struct Point` with DIFFERENT layouts, both reachable in the entry
/// program: a REAL collision. Each constructs correctly; the entry/first keeps the bare key
/// (`Point(...)`), the other is disambiguated. Byte-identical on both engines.
#[test]
fn real_collision_two_modules_same_struct() {
    let out = assert_parity_file(
        &[
            ("a.chz", "struct Point:\n    x: int\n"),
            ("b.chz", "struct Point:\n    y: int\n    z: int\n"),
            (
                "main.chz",
                "import a\nimport b\npa := a.Point(1)\npb := b.Point(2, 3)\nprint(pa.x)\nprint(pb.y)\nprint(pb.z)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n2\n3\n");
}

/// Blocker C: two modules declare `enum Color`, both reachable. `cb.classify` MATCHES on its own
/// `Color.Red`/`Color.Green`. The disambiguated enum's variants must MATCH (not just construct):
/// the match-pattern side must use the SAME module-scoped runtime key as construction. Identical
/// on both engines.
#[test]
fn enum_collision_match_in_declaring_module() {
    let out = assert_parity_file(
        &[
            ("ca.chz", "enum Color:\n    Red\n    Green\n"),
            (
                "cb.chz",
                "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\n",
            ),
            (
                "cmain.chz",
                "import ca\nimport cb\nprint(cb.classify(cb.Color.Red))\nprint(cb.classify(cb.Color.Green))\n",
            ),
        ],
        "cmain.chz",
    );
    assert_eq!(out, "1\n2\n");
}

/// Blocker C (no-collision twin): a SINGLE-module enum matched within its own declaring module
/// must stay byte-identical (the common-case key is BARE). Guards the match-side key resolution
/// from regressing the non-colliding path.
#[test]
fn enum_match_same_module_no_collision() {
    let out = parity_entry(
        "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\nprint(classify(Color.Red))\nprint(classify(Color.Green))\n",
    );
    assert_eq!(out, "1\n2\n");
}

/// Regression: a `from`-imported FUNCTION named like SOME OTHER module's type must still bind +
/// call as a function — the from-import type-skip is keyed on the TARGET module's types, NOT a
/// program-wide type-name set, and the bare ctor only fires for a bare-VISIBLE type (not one
/// merely present in the global table). Without both gates, `Foo()` wrongly hit B's struct ctor.
#[test]
fn from_imported_fn_named_like_another_modules_type() {
    let out = assert_parity_file(
        &[
            ("a.chz", "fn Foo() -> int:\n    return 42\n"),
            ("b.chz", "struct Foo:\n    x: int\n"),
            ("main.chz", "import Foo from a\nimport b\nprint(Foo())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "42\n");
}

/// Blocker C (variant-key double-resolution): a module BOTH imports a colliding enum (`win.E`,
/// the load-order winner keyed BARE) AND declares its OWN same-named loser enum (`mid::E`), then
/// QUALIFIED-constructs the imported one (`win.E.A`) and passes it to the importer's fn, which
/// MATCHES it in `win`'s context. The construction's variant_id must key on `win`'s E (the call
/// site's already-resolved key), NOT be re-derived from the currently-compiled module (`mid`)'s
/// `bare_types` — else the producer bakes `mid::E::A` and `win.pick`'s match never fires. Must be
/// byte-identical on both engines (interp was already correct; this guards VM/serial).
#[test]
fn enum_collision_construct_in_other_declaring_module() {
    let out = assert_parity_file(
        &[
            (
                "win.chz",
                "enum E:\n    A\n    B\n\nfn pick(e: E) -> int:\n    match e:\n        E.A: return 1\n        E.B: return 2\n",
            ),
            (
                "mid.chz",
                "import win\n\nenum E:\n    A\n    B\n\nfn go() -> int:\n    return win.pick(win.E.A)\n",
            ),
            ("main.chz", "import win\nimport mid\nprint(mid.go())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n");
}

/// No-collision twin: same layout, but the constructing module does NOT declare its own `E`
/// (single declarer `win`, bare key). Proves the common path (qualified construct of an imported
/// enum from an importer) is untouched. Passes before AND after the fix (regression guard).
#[test]
fn enum_no_collision_construct_in_importing_module() {
    let out = assert_parity_file(
        &[
            (
                "win.chz",
                "enum E:\n    A\n    B\n\nfn pick(e: E) -> int:\n    match e:\n        E.A: return 1\n        E.B: return 2\n",
            ),
            (
                "mid.chz",
                "import win\n\nfn go() -> int:\n    return win.pick(win.E.A)\n",
            ),
            ("main.chz", "import win\nimport mid\nprint(mid.go())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n");
}

/// A spawned task constructs an imported struct AND a value crosses a Channel (cross-airlock,
/// data-not-time): identical on interp, default VM, and `--parallel`.
#[test]
fn imported_struct_across_airlock_three_engine() {
    let files = [
        ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import geo\nch := Channel[geo.Point]()\nparallel:\n    spawn:\n        ch.send(geo.Point(3, 4))\np := ch.recv()\nprint(p.x + p.y)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "7\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

// ----- ROOT REDESIGN: always-qualified identity key + bare display name -----

/// THE BUG (collision-loser decode against the WRONG layout): the entry module declares its OWN
/// `Point{a,b}` while a dep also declares `Point{x}`. A bare `json.decode[Point]` must decode
/// against the ENTRY's layout (the bare name resolves to the entry's `Point`), printing 5 — not
/// fail `decode: missing key 'x'` against the dep's layout. Byte-identical on both engines.
#[test]
fn decode_collision_loser_against_correct_layout() {
    let out = assert_parity_file(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\")\nmatch p:\n    Ok(v): print(v.a)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "5\n");
}

/// Twin: a QUALIFIED `json.decode[dep.Point]` decodes against the dep's layout (`x`), printing 7.
#[test]
fn decode_qualified_target() {
    let out = assert_parity_file(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[dep.Point](\"{{\\\"x\\\":7}}\")\nmatch p:\n    Ok(v): print(v.x)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "7\n");
}

/// BARE DISPLAY ON COLLISION: two modules both declare `struct Point` with different layouts;
/// printing each value shows the BARE name (`Point(x=1)` / `Point(y=2)`) on BOTH — never the
/// module-qualified identity key. Byte-identical on both engines.
#[test]
fn collision_prints_bare_for_both() {
    let out = assert_parity_file(
        &[
            ("a.chz", "struct Point:\n    x: int\n"),
            ("b.chz", "struct Point:\n    y: int\n"),
            (
                "main.chz",
                "import a\nimport b\nprint(a.Point(1))\nprint(b.Point(2))\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "Point(x=1)\nPoint(y=2)\n");
}

/// NESTED-COLLISION FIELD DECODE: `dep` has `Wrap{inner: Inner}` + `Inner{k}`; the entry declares
/// its OWN `Inner{other}`. `json.decode[dep.Wrap]` must expand the nested `Inner` field in dep's
/// DEFINING scope (`dep::Inner` with `k`), not the entry's. Prints 3. Byte-identical both engines.
#[test]
fn decode_nested_struct_field_in_defining_module() {
    let out = assert_parity_file(
        &[
            (
                "dep.chz",
                "struct Inner:\n    k: int\nstruct Wrap:\n    inner: Inner\n",
            ),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Inner:\n    other: int\np := json.decode[dep.Wrap](\"{{\\\"inner\\\":{{\\\"k\\\":3}}}}\")\nmatch p:\n    Ok(w): print(w.inner.k)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "3\n");
}

/// AIRLOCK 3-ENGINE PARITY: the collision-decode repro must produce identical output under interp,
/// the default VM, and the `--parallel` OS-thread engine.
#[test]
fn decode_collision_three_engine() {
    let files = [
        ("dep.chz", "struct Point:\n    x: int\n"),
        (
            "main.chz",
            "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\nmatch json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\"):\n    Ok(v): print(v.a)\n    Err(e): print(e)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "5\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

/// BLOCKER 2/3 — a bare match-pattern enum qualifier (`Color.Red`) whose enum is reachable only
/// via WHOLE-MODULE import must be resolved from the SCRUTINEE'S static enum, not re-guessed by
/// iterating the (RandomState-seeded) import map. Two whole-imported same-named enums; scrutinee
/// from `a`; the `Color.Red`/`Color.Blue` arms must resolve against `a::Color`. Looped 50x to
/// defeat the nondeterminism (one pass could luck-pass) and asserted identical across all three
/// engines (interp / default VM / `--parallel`).
#[test]
fn match_arm_scrutinee_driven_three_engine() {
    let files = [
        ("a.chz", "enum Color:\n    Red\n    Blue\n"),
        ("b.chz", "enum Color:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import a\nimport b\nc := a.Color.Red\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Blue: print(\"blue\")\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    for _ in 0..50 {
        let (io, _, ir, _) = run_file_p(&entry);
        let (vo, _, vr, _) = run_file(&entry);
        let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert!(
            ir.is_ok() && vr.is_ok() && pr.is_ok(),
            "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
        );
        assert_eq!(io, "red\n", "interp output wrong");
        assert_eq!(io, vo, "interp vs vm");
        assert_eq!(io, po, "interp vs --parallel");
    }
}

/// BLOCKER 2/3 divergence case — `import b` FIRST, then `import a`; scrutinee is `a.Color.Blue`,
/// a variant that exists ONLY in `a`. A scrutinee-blind import-iterating resolver picked `b`
/// (which has no `Blue`) and crashed (`--serial`/`--parallel`) or matched the wrong arm (VM). The
/// match-arm key MUST come from the scrutinee's `a::Color`. Looped 50x across all three engines.
#[test]
fn match_arm_only_in_a_variant_three_engine() {
    let files = [
        ("a.chz", "enum Color:\n    Red\n    Blue\n"),
        ("b.chz", "enum Color:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import b\nimport a\nc := a.Color.Blue\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Blue: print(\"blue\")\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    for _ in 0..50 {
        let (io, _, ir, _) = run_file_p(&entry);
        let (vo, _, vr, _) = run_file(&entry);
        let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert!(
            ir.is_ok() && vr.is_ok() && pr.is_ok(),
            "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
        );
        assert_eq!(io, "blue\n", "interp output wrong");
        assert_eq!(io, vo, "interp vs vm");
        assert_eq!(io, po, "interp vs --parallel");
    }
}

/// AIRLOCK: a colliding struct value sent through a Channel survives the wire round-trip and
/// prints its BARE name on the other side. Identical on interp, default VM, and `--parallel`.
#[test]
fn collision_struct_sent_across_task() {
    let files = [
        ("a.chz", "struct Point:\n    x: int\n"),
        ("b.chz", "struct Point:\n    y: int\n"),
        (
            "main.chz",
            "import a\nimport b\nch := Channel[b.Point]()\nparallel:\n    spawn:\n        ch.send(b.Point(9))\np := ch.recv()\nprint(p)\nprint(p.y)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "Point(y=9)\n9\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

// ----- C5 (A2): program-exit auto-drain is skipped on a hard `os.exit` -----

#[test]
fn executor_autodrain_skipped_on_os_exit() {
    // `os.exit` is a hard halt — like `defer`, the program-exit auto-drain is skipped, so a
    // submitted-but-un-shut executor's work must NOT run. Driven through the file path on both
    // engines (parity), since it imports std.os.
    let out = parity_entry(
        "import std.os\nfn j():\n    print(\"RAN\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    print(\"before exit\")\n    os.exit(0)\nmain()\n",
    );
    assert_eq!(out, "before exit\n");
}

// ----- Executor.submit crosses a submitted closure BY VALUE on BOTH engines (serial == M:N) -----
// The cooperative engine used to queue the submitted closure's own heap Handle (captures shared by
// reference, bypassing the airlock), while M:N wired it by value. That broke serial==M:N. Now both
// route through `wire_callable`/`to_wire`, so the generator airlock enforcement runs and captures
// isolate identically on both engines.

#[test]
fn executor_submit_generator_capturing_closure_crosses_by_value() {
    // F3 path C: a submitted closure captures a LIVE local generator (`it`, Pending). The by-value
    // airlock (`to_wire`) serializes it and the submitted task drives its own copy. WAS: both
    // faulted the generator crossing; NOW: both run it, byte-identical on serial and M:N.
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    fn task():
        for x in it:
            print(x)
    ex := Executor()
    ex.submit(task)
    ex.shutdown()
main()";
    assert_eq!(parity_entry(src), "1\n");
}

#[test]
fn executor_submit_mutating_closure_isolated_parity() {
    // Repro #3: a submitted closure captures + mutates a local list, observed after the drain. WAS a
    // SILENT value divergence: serial shared by reference (prints `2`), M:N isolated by value
    // (prints `1`). NOW both isolate at submit → both print `1`, byte-identical.
    let out = parity_entry(
        "fn main():\n    box := [0]\n    ex := Executor()\n    ex.submit(fn(): box.push(1))\n    ex.shutdown()\n    print(box.len())\nmain()\n",
    );
    assert_eq!(out, "1\n");
}

#[test]
fn executor_submit_module_global_inplace_mutation_isolates_parity() {
    // Task 1 (Executor path) — a submitted closure mutating a MODULE-GLOBAL aggregate in place must
    // isolate on BOTH engines, exactly like the nursery path. The cooperative Executor drain snapshots
    // the module globals per task (mirroring M:N's `drain_executor_on_pool` → `install_snapshot`), so
    // the parent's post-shutdown read sees the PRE-task value. Before the fix the serial inline drain
    // ran the task against the LIVE shell globals (leaked → serial=4) while M:N isolated (→ 3).
    let out = parity_entry(
        "xs := [1, 2, 3]\nfn main():\n    ex := Executor()\n    ex.submit(fn(): xs.push(99))\n    ex.shutdown()\n    print(xs.len())\nmain()\n",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn executor_submit_module_global_callee_reassign_isolates_parity() {
    // Task 1 (Executor path) — a submitted closure whose free-fn callee REASSIGNS a module global
    // (`bump()` does `count = count + 1`). Pre-dated the diff and still diverged (serial=2 / M:N=0)
    // because the serial Executor drain aliased the shell globals. Now each submitted task runs against
    // its own module-global copy → the parent reads the frozen 0 on both engines.
    let out = parity_entry(
        "count := 0\nfn bump():\n    count = count + 1\nfn main():\n    ex := Executor()\n    ex.submit(fn(): bump())\n    ex.submit(fn(): bump())\n    ex.shutdown()\n    print(count)\nmain()\n",
    );
    assert_eq!(out, "0\n");
}

#[test]
fn executor_submit_atomic_visible_to_parent_parity() {
    // Task 1 (Executor escape hatch) — an `Atomic` module global crosses the Executor drain by shared
    // `Arc` (via `to_snap`), NOT deep-copied, so a task-side `add` IS visible to the parent. Guards that
    // the per-task serial snapshot does not clone the Arc away (trap #1) on the Executor path too.
    let out = parity_entry(
        "import std.concurrency\na := Atomic(0)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): a.add(1))\n    ex.submit(fn(): a.add(1))\n    ex.shutdown()\n    print(a.load())\nmain()\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn executor_submit_sendable_closure_runs_parity() {
    // Control: a captured `Channel` is a genuinely-shared handle (crosses as its shared Arc, NOT
    // deep-copied), so a value sent from inside the submitted job is visible to the parent's `recv`.
    // Must stay `7` on both engines — the by-value collapse must not over-isolate a shared handle.
    let out = parity_entry(
        "fn main():\n    ch := Channel[int]()\n    ex := Executor()\n    ex.submit(fn(): ch.send(7))\n    ex.shutdown()\n    print(ch.recv())\nmain()\n",
    );
    assert_eq!(out, "7\n");
}

// ----- M9: std.regex parity (exercises NativeRet::Struct lowering on both engines) -----

#[test]
fn regex_find_all_replace_split_parity() {
    let out = parity_entry(
        r##"import std.regex
match regex.find_all("[0-9]+", "a1 22 333"):
    Ok(ms):
        for m in ms:
            print(m.text)
    Err(e): print(e)
match regex.replace_all("[0-9]+", "a1b22c", "#"):
    Ok(s): print(s)
    Err(e): print(e)
match regex.split(",", "a,b,c"):
    Ok(parts): print("|".join(parts))
    Err(e): print(e)
"##,
    );
    assert_eq!(out, "1\n22\n333\na#b#c\na|b|c\n");
}

/// `std.request` against a loopback server, run through BOTH engines (exercises `NativeRet::Map`
/// lowering on each). The server serves one canned response per connection; interp and vm each
/// open one, so it accepts twice.
#[test]
fn request_get_parity_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "pong";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    Ok(resp):\n        print(str(resp.status))\n        print(resp.body)\n        print(resp.headers[\"x-test\"])\n    Err(e): print(e)\n"
    );
    let out = parity_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "200\npong\nhi\n");
}

/// `std.request` new verbs + custom headers, run through BOTH engines against a loopback server
/// that records every request's wire bytes. Each engine issues a `put` and a header-carrying
/// `request("DELETE", …)`, so the server accepts 4 times (2 per engine). Asserts (a) identical
/// stdout across VM and interp and (b) the right method line + custom header reached the wire —
/// locking the off-heap `NativeArg::Map` headers path and the verb wrappers under parity.
#[test]
fn request_verbs_and_headers_parity_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = Arc::clone(&seen);
    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            seen_srv
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            // `Connection: close` so ureq's thread-local pool (shared across both engine runs on
            // this test thread) never reuses a server-closed socket — one fresh conn per request.
            let resp = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    // Each engine: a PUT verb wrapper, then a header-carrying general DELETE.
    let src = format!(
        "import std.request\nmatch request.put(\"http://{addr}/\", \"payload\"):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\nmatch request.request(\"DELETE\", \"http://{addr}/\", \"\", {{\"X-Custom\": \"value\"}}):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\n"
    );
    let out = parity_entry(&src);
    server.join().unwrap();
    assert_eq!(
        out, "200\n200\n",
        "VM/interp must agree and both requests succeed"
    );

    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 4, "two requests per engine");
    let puts = reqs.iter().filter(|r| r.starts_with("PUT ")).count();
    let deletes = reqs.iter().filter(|r| r.starts_with("DELETE ")).count();
    let with_header = reqs
        .iter()
        .filter(|r| r.contains("X-Custom: value"))
        .count();
    assert_eq!(puts, 2, "both engines must send PUT");
    assert_eq!(deletes, 2, "both engines must send DELETE");
    assert_eq!(
        with_header, 2,
        "the custom header must reach the wire on both engines"
    );
}

#[test]
fn regex_find_groups_and_span_parity() {
    let out = parity_entry(
        r#"import std.regex
match regex.find("([a-z]+)@([a-z]+)", "xx ann@host"):
    Ok(opt):
        match opt:
            Some(m): print(m.text + " " + str(m.start) + " " + ",".join(m.groups))
            None: print("none")
    Err(e): print(e)
"#,
    );
    assert_eq!(out, "ann@host 3 ann,host\n");
}

// ----- break / continue parity (both engines must agree AND produce the right output) -----

/// Assert both engines agree AND that the (shared) stdout equals `expect`. A hang here means a
/// `continue` is landing on the wrong target (re-test without advancing → infinite loop).
fn assert_parity_out(src: &str, expect: &str) {
    assert_parity(src);
    assert_eq!(
        vm_outcome(src).expect("program should run"),
        expect,
        "for:\n{src}"
    );
}

// ----- Phase 0 scalar fills: bool(x) truthiness cast + Result-returning parse variants -----

#[test]
fn parse_int_parity() {
    // `s.parse_int() -> Result[int, str]` — the error-message-carrying sibling of `to_int() -> int?`.
    let src = r#"
fn main():
    print("42".parse_int())
    print("  7 ".parse_int())
    print("x".parse_int())

main()
"#;
    assert_parity_out(src, "Ok(42)\nOk(7)\nErr(cannot parse 'x' as an integer)\n");
}

#[test]
fn parse_float_parity() {
    // `s.parse_float() -> Result[float, str]` — the sibling of `to_float() -> float?`.
    let src = r#"
fn main():
    print("3.14".parse_float())
    print("x".parse_float())

main()
"#;
    assert_parity_out(src, "Ok(3.14)\nErr(cannot parse 'x' as a float)\n");
}

#[test]
fn bitwise_ops_parity() {
    assert_parity_out(
        "print(5 & 3)\nprint(5 | 2)\nprint(5 ^ 3)\nprint(1 << 4)\nprint(255 >> 4)\n",
        "1\n7\n6\n16\n15\n",
    );
}

#[test]
fn bitwise_precedence_below_comparison_parity() {
    // `5 & 3 == 1` is `(5 & 3) == 1` (bitwise binds tighter than `==`, Python-style).
    assert_parity_out("print(5 & 3 == 1)\n", "true\n");
}

#[test]
fn xor_fold_single_number_parity() {
    assert_parity_out(
        "xs := [4,1,2,1,4,2,7]\nacc := 0\nfor x in xs:\n    acc = acc ^ x\nprint(acc)\n",
        "7\n",
    );
}

#[test]
fn shift_out_of_range_error_parity() {
    // Dynamic shift the checker can't catch — both engines must raise the same runtime error.
    assert_parity("print(1 << 64)\n");
    assert_parity("print(1 << -1)\n");
}

#[test]
fn match_tuple_pattern_parity() {
    assert_parity_out(
        "p := (3, 4)\nmatch p:\n    (0, y): print(y)\n    (x, y): print(x + y)\n",
        "7\n",
    );
}

#[test]
fn match_tuple_literal_arm_parity() {
    assert_parity_out(
        "p := (1, 9)\nlabel := match p:\n    (1, n): \"one {n}\"\n    _: \"other\"\nprint(label)\n",
        "one 9\n",
    );
}

#[test]
fn match_nested_variant_in_tuple_parity() {
    assert_parity_out(
        "o: (int, int)? = Some((10, 20))\nmatch o:\n    None: print(\"none\")\n    Some((a, b)): print(a + b)\n",
        "30\n",
    );
}

#[test]
fn match_nested_heap_payload_gc_stress() {
    // Nested pattern binding heap values (strings) inside a tuple inside a variant; a GC mid-bind
    // must not collect the still-referenced payload.
    let src = "o: (str, str)? = Some((\"a\" + \"b\", \"c\" + \"d\"))\nmatch o:\n    None: print(\"none\")\n    Some((x, y)): print(x + y)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "abcd\n");
    assert_eq!(
        run_capture_stress(src),
        "abcd\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn match_guard_fallthrough_parity() {
    // The first arm's pattern binds but its guard is false → fall through to the next arm.
    assert_parity_out(
        "n := 5\nmatch n:\n    x if x < 0: print(\"neg\")\n    x if x > 10: print(\"big\")\n    _: print(\"mid\")\n",
        "mid\n",
    );
}

#[test]
fn match_guard_first_true_parity() {
    assert_parity_out(
        "n := -2\nlabel := match n:\n    x if x < 0: \"neg\"\n    _: \"nonneg\"\nprint(label)\n",
        "neg\n",
    );
}

#[test]
fn match_range_boundaries_parity() {
    // Half-open: start is inclusive, end is exclusive.
    assert_parity_out(
        "fn b(n: int) -> str:\n    return match n:\n        0..10: \"lo\"\n        10..20: \"hi\"\n        _: \"out\"\nprint(b(0))\nprint(b(9))\nprint(b(10))\nprint(b(19))\nprint(b(20))\n",
        "lo\nlo\nhi\nhi\nout\n",
    );
}

#[test]
fn match_range_with_literal_mix_parity() {
    // A match mixing int literals and ranges still routes through the literal path.
    assert_parity_out(
        "fn f(n: int) -> str:\n    return match n:\n        0: \"zero\"\n        1..100: \"small\"\n        _: \"big\"\nprint(f(0))\nprint(f(50))\nprint(f(500))\n",
        "zero\nsmall\nbig\n",
    );
}

#[test]
fn for_over_map_keys_parity() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m:\n    print(k)\n",
        "a\nb\nc\n",
    );
}

/// Regression (review #1): a struct field named `decode` must still be indexable — the
/// `.decode[…]` JSON form is only stolen when a real `[Type](arg)` follows.
#[test]
fn field_named_decode_is_indexable_parity() {
    let out = parity_entry(
        "struct Box:\n    decode: List[int]\nb := Box([10, 20, 30])\nprint(b.decode[1])\nprint(b.decode[0] + b.decode[2])\n",
    );
    assert_eq!(out, "20\n40\n");
}

/// Regression (review #2): malformed and out-of-range numbers come back as `Err` / stringify
/// cleanly — they must never abort the host (no uncaught `float()`/`int()` panic).
#[test]
fn json_malformed_numbers_are_errors_parity() {
    let out = parity_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK \" + json.stringify(j)\n        Err(e): return \"ERR\"\nprint(tp(\"1e\"))\nprint(tp(\"1.\"))\nprint(tp(\"100000000000000000000\"))\n",
    );
    assert_eq!(out, "ERR\nERR\nOK 1e+20\n");
}

/// BUG 1: `json.stringify` must `\u00XX`-escape control chars U+0000..U+001F (Go/RFC-8259
/// policy) instead of emitting the raw byte (invalid JSON). Round-trips back to the original
/// string via `json.parse`'s `\u` handling.
#[test]
fn json_control_char_escape_roundtrip_parity() {
    let out = parity_entry(
        "import std.json\ns := \"x\" + chr(1) + chr(31) + chr(0) + \"y\"\nout := json.stringify(Json.Str(s))\ncodes := List[int]()\nfor c in out.chars():\n    codes.push(ord(c))\nprint(codes)\nmatch json.parse(out):\n    Ok(j):\n        match j:\n            Json.Str(back):\n                if back == s:\n                    print(\"RT_OK\")\n                else:\n                    print(\"RT_FAIL\")\n            _: print(\"NOT_STR\")\n    Err(e): print(\"PARSE_ERR\")\n",
    );
    assert_eq!(
        out,
        "[34, 120, 92, 117, 48, 48, 48, 49, 92, 117, 48, 48, 49, 102, 92, 117, 48, 48, 48, 48, 121, 34]\nRT_OK\n"
    );
}

/// BUG 2: `json.parse` rejects leading-zero integers (RFC-8259 / Python `json.loads`): a `0`
/// immediately followed by another digit is an error. Lone `0`/`-0`/`0.5`/`0e1` stay valid.
#[test]
fn json_leading_zero_numbers_are_errors_parity() {
    let out = parity_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK \" + json.stringify(j)\n        Err(e): return \"ERR\"\nprint(tp(\"01\"))\nprint(tp(\"007\"))\nprint(tp(\"-01\"))\nprint(tp(\"01.5\"))\nprint(tp(\"08\"))\nprint(tp(\"0\"))\nprint(tp(\"-0\"))\nprint(tp(\"0.5\"))\nprint(tp(\"10\"))\nprint(tp(\"0e1\"))\n",
    );
    assert_eq!(
        out,
        "ERR\nERR\nERR\nERR\nERR\nOK 0\nOK 0\nOK 0.5\nOK 10\nOK 0\n"
    );
}

/// OPTIONAL: a RAW control char (e.g. a literal newline, ord 10) inside a JSON string literal is
/// rejected (RFC/Python); a proper `\n` ESCAPE sequence still parses — proving the `\` arm is
/// untouched.
#[test]
fn json_raw_control_char_in_string_is_error_parity() {
    let out = parity_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK\"\n        Err(e): return \"ERR\"\nraw := \"\\\"x\" + chr(10) + \"y\\\"\"\nprint(tp(raw))\nprint(tp(\"\\\"a\\\\nb\\\"\"))\n",
    );
    assert_eq!(out, "ERR\nOK\n");
}

/// Finding C: `json.stringify` FAULTS (recoverable, Go-style) on a non-finite `Json.Num`
/// (NaN / +inf / -inf) instead of emitting the invalid bare tokens `NaN`/`inf`/`-inf` — which are
/// not valid JSON and are rejected by Chezzi's own `json.parse`. The fault is catchable under
/// `recover:` with a byte-identical message on both engines.
#[test]
fn json_stringify_non_finite_faults_parity() {
    let out = parity_entry(
        "import std.json\nfn tp(x: float) -> str:\n    doc := Json.Obj({\"v\": Json.Num(x)})\n    r := recover:\n        json.stringify(doc)\n    match r:\n        Ok(s): return \"OK \" + s\n        Err(e): return e.message()\nprint(tp(1.0 / 0.0))\nprint(tp(-1.0 / 0.0))\nprint(tp(0.0 / 0.0))\n",
    );
    assert_eq!(
        out,
        "cannot serialize non-finite float to JSON\ncannot serialize non-finite float to JSON\ncannot serialize non-finite float to JSON\n"
    );
}

/// Python-parity float `str()`/`print()`: scientific notation when the decimal exponent is `< -4`
/// or `>= 16`, otherwise fixed. Byte-identical on both engines (serial == M:N).
#[test]
fn python_float_repr_str_parity() {
    let out = parity_entry(
        "print(str(1e16))\nprint(1.5e300)\nprint(0.00001)\nprint(str(0 - 2.5e-8))\nprint(1.0)\nprint(0.0001)\nprint(1e15)\nprint(1e100)\n",
    );
    assert_eq!(
        out,
        "1e+16\n1.5e+300\n1e-05\n-2.5e-08\n1.0\n0.0001\n1000000000000000.0\n1e+100\n"
    );
}

/// Finding C regression guard: FINITE floats (including large magnitudes OUTSIDE the ±9e15
/// int-collapse range like 1e300), whole-number floats, negatives, ints, strings and nested
/// Arr/Obj are COMPLETELY unaffected — they stringify byte-identically and round-trip through
/// `json.parse`. Proves the non-finite guard does NOT reuse the ±9e15 range check.
#[test]
fn json_stringify_finite_roundtrip_unchanged_parity() {
    let out = parity_entry(
        "import std.json\ndoc := Json.Arr([Json.Num(3.0), Json.Num(1.5), Json.Num(1e300), Json.Num(0.0 - 2.5), Json.Str(\"hi\"), Json.Obj({\"k\": Json.Num(42.0)})])\ns := json.stringify(doc)\nprint(s)\nmatch json.parse(s):\n    Ok(v): print(\"roundtrip \" + json.stringify(v))\n    Err(e): print(\"ERR \" + e.message())\n",
    );
    // 1e300 is FINITE but far outside the ±9e15 int-collapse range: it must stringify normally
    // (via `str(f)`), NOT fault. `str(1e300)` renders in scientific notation (CPython repr parity,
    // exponent >= 16): `1e+300`.
    let line = "[3,1.5,1e+300,-2.5,\"hi\",{\"k\":42}]".to_string();
    assert_eq!(out, format!("{line}\nroundtrip {line}\n"));
}

#[test]
fn json_decode_struct_parity() {
    let out = parity_entry(
        "import std.json\nstruct P:\n    x: int\n    y: int\nmatch json.decode[P](\"{{\\\"x\\\":1,\\\"y\\\":2}}\"):\n    Ok(p): print(p.x + p.y)\n    Err(e): print(e)\n",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn json_decode_error_parity() {
    let out = parity_entry(
        "import std.json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"y\\\":2}}\"):\n    Ok(p): print(p.x)\n    Err(e): print(e)\n",
    );
    assert_eq!(out, "decode: missing key 'x' at $\n");
}

/// `json.as_int` is a total `-> Option[int]`: an in-range finite value (incl. the i64::MAX/MIN
/// boundaries) returns `Some`, an out-of-i64-range-but-finite number returns `None` (never faults),
/// and a fractional number still truncates. (Non-finite numbers can no longer arrive here — `parse`
/// rejects `1e400` at the source, so that input takes the `PARSEERR` arm below.)
#[test]
fn json_as_int_out_of_range_parity() {
    let out = parity_entry(
        "import std.json\nfn a(s: str) -> str:\n    match json.parse(s):\n        Ok(j):\n            match json.as_int(j):\n                Some(v): return \"SOME \" + str(v)\n                None: return \"NONE\"\n        Err(e): return \"PARSEERR\"\nprint(a(\"9999999999999999999\"))\nprint(a(\"42\"))\nprint(a(\"9223372036854775807\"))\nprint(a(\"-9223372036854775808\"))\nprint(a(\"2.5\"))\nprint(a(\"1e400\"))\n",
    );
    assert_eq!(
        out,
        "NONE\nSOME 42\nSOME 9223372036854775807\nSOME -9223372036854775808\nSOME 2\nPARSEERR\n"
    );
}

/// `json.parse` follows the Go `encoding/json` decode policy: a numeral whose magnitude overflows
/// f64 to a non-finite value (`1e400` → +inf, `-1e400` → -inf, and the same inside an array) is
/// REJECTED with `Err` at parse time — parse never manufactures a `Json.Num(inf/-inf/NaN)` that its
/// own `stringify` would then refuse to serialize. Finite numbers (incl. underflow-to-0 `1e-400`)
/// still parse `Ok`.
#[test]
fn json_parse_rejects_non_finite_parity() {
    let out = parity_entry(
        "import std.json\nfn p(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK\"\n        Err(e): return \"PARSEERR\"\nprint(p(\"1e400\"))\nprint(p(\"-1e400\"))\nprint(p(\"[1e400]\"))\nprint(p(\"1.5\"))\nprint(p(\"123\"))\nprint(p(\"1e-400\"))\n",
    );
    assert_eq!(out, "PARSEERR\nPARSEERR\nPARSEERR\nOK\nOK\nOK\n");
}

/// `str.split` honours the `pieces == separators + 1` invariant at *every* input including the
/// empty string: `"".split(",")` has zero separators, so it yields a one-element list holding the
/// empty string (length 1, `x[0] == ""`) — matching Python/Go/Rust/JS, never `[]`. (An empty list
/// and a list holding a single empty string both *render* as `[]` because `""` prints as nothing,
/// so this asserts the length + element rather than the debug rendering.)
#[test]
fn str_empty_split_returns_single_empty_element_parity() {
    let out = parity_entry(
        "x := \"\".split(\",\")\nprint(x.len())\nprint(x[0] == \"\")\nprint(\"\".split(\",\").len() == 1)\nprint(\"a,\".split(\",\").len())\n",
    );
    assert_eq!(out, "1\ntrue\ntrue\n2\n");
}

/// The i64::MIN boundary literal `-9223372036854775808` (previously unreachable — it lex-errored as
/// "number too large" because the positive magnitude 2^63 overflows i64 before the minus applies)
/// now evaluates to i64::MIN and behaves as an ordinary int under arithmetic and `match`, byte-
/// identically on the serial and M:N engines.
#[test]
fn i64_min_literal_runs_parity() {
    let out = parity_entry(
        "print(-9223372036854775808)\nprint(-9223372036854775808 + 1)\nprint(-9223372036854775807 - 1 == -9223372036854775808)\nmatch -9223372036854775808:\n    -9223372036854775808: print(\"min\")\n    _: print(\"other\")\n",
    );
    assert_eq!(
        out,
        "-9223372036854775808\n-9223372036854775807\ntrue\nmin\n"
    );
}

/// `json.decode[int]` rejects out-of-range integers with `Err` (never silently saturates), while
/// the exact i64::MAX / i64::MIN boundaries still decode to `Ok`.
#[test]
fn json_decode_int_out_of_range_parity() {
    let out = parity_entry(
        "import std.json\nfn d(s: str) -> str:\n    match json.decode[int](s):\n        Ok(v): return \"OK \" + str(v)\n        Err(e): return \"ERR\"\nprint(d(\"1000000000000000000000000000000\"))\nprint(d(\"-1000000000000000000000000000000\"))\nprint(d(\"18446744073709551615\"))\nprint(d(\"9223372036854775807\"))\nprint(d(\"-9223372036854775808\"))\nprint(d(\"42\"))\n",
    );
    assert_eq!(
        out,
        "ERR\nERR\nERR\nOK 9223372036854775807\nOK -9223372036854775808\nOK 42\n"
    );
}

/// Cross-site consistency: for the identical strictly-out-of-range input, `decode[int]` returns
/// `Err` and `as_int` returns `None` — both total, neither saturates, neither faults.
#[test]
fn json_int_boundary_consistency_parity() {
    let out = parity_entry(
        "import std.json\ns := \"9999999999999999999\"\nmatch json.decode[int](s):\n    Ok(v): print(\"decode OK\")\n    Err(e): print(\"decode ERR\")\nmatch json.parse(s):\n    Ok(j):\n        match json.as_int(j):\n            Some(v): print(\"as_int SOME\")\n            None: print(\"as_int NONE\")\n    Err(e): print(\"parse ERR\")\n",
    );
    assert_eq!(out, "decode ERR\nas_int NONE\n");
}

#[test]
fn process_cmd_ok_and_err_parity() {
    let out = parity_entry(
        "import std.process\nmatch process.cmd(\"printf abc\"):\n    Ok(s): print(\"ok:\" + s)\n    Err(e): print(\"err:\" + e)\nmatch process.cmd(\"exit 2\"):\n    Ok(s): print(\"ok\")\n    Err(e): print(\"err:\" + e)\n",
    );
    assert_eq!(out, "ok:abc\nerr:command exited with status 2\n");
}

#[test]
fn fs_predicates_parity() {
    let out = parity_entry(
        "import std.fs\nprint(fs.exists(\"Cargo.toml\"))\nprint(fs.exists(\"definitely_not_here.zzz\"))\nprint(fs.is_dir(\"src\"))\n",
    );
    assert_eq!(out, "true\nfalse\ntrue\n");
}

/// gaps §6 — fs.stat/fs.walk + the FileInfo native struct across BOTH engines. Proves the
/// reserved-type gate (`from fs import FileInfo`) + the `type_names` bind_import skip on serial + M:N,
/// and that stat/walk read the real filesystem identically. Scratch tree is self-owned (file-dependent
/// size — mtime is NOT asserted). Walk order is deterministic (per-dir sort) → exact stdout match.
#[test]
fn fs_stat_walk_fileinfo_parity() {
    let scratch = TmpDir::new();
    scratch.write("root/a.txt", "hello\n"); // 6 bytes
    scratch.write("root/b.txt", "b");
    scratch.write("root/sub/c.txt", "c");
    let file = scratch.0.join("root/a.txt");
    let file = file.to_string_lossy();
    let root = scratch.0.join("root");
    let root = root.to_string_lossy();
    // `import FileInfo from std.fs` proves the reserved-type gate + `type_names` bind_import skip on
    // BOTH engines (it runtime-traps if FileInfo is not registered). `import std.fs` licenses the
    // `fs.stat`/`fs.walk` qualified calls.
    let src = format!(
        "import std.fs\n\
         import FileInfo from std.fs\n\
         match fs.stat(\"{file}\"):\n\
         \x20   Ok(fi):\n\
         \x20       print(str(fi.size))\n\
         \x20       print(str(fi.is_file))\n\
         \x20       print(str(fi.is_dir))\n\
         \x20   Err(e): print(\"staterr\")\n\
         match fs.walk(\"{root}\"):\n\
         \x20   Ok(xs):\n\
         \x20       for p in xs:\n\
         \x20           print(p)\n\
         \x20   Err(e): print(\"walkerr\")\n",
    );
    let out = parity_entry(&src);
    let expected = format!(
        "6\ntrue\nfalse\n{a}\n{b}\n{sub}\n{c}\n",
        a = scratch.0.join("root/a.txt").to_string_lossy(),
        b = scratch.0.join("root/b.txt").to_string_lossy(),
        sub = scratch.0.join("root/sub").to_string_lossy(),
        c = scratch.0.join("root/sub/c.txt").to_string_lossy(),
    );
    assert_eq!(out, expected);
}

#[test]
fn time_format_parity() {
    let out = parity_entry(
        "import std.time\nprint(time.format(0))\nprint(time.format(1700000000))\nprint(time.now() > 0)\n",
    );
    assert_eq!(out, "1970-01-01 00:00:00\n2023-11-14 22:13:20\ntrue\n");
}

#[test]
fn set_dedup_and_algebra_parity() {
    assert_parity_out(
        "s := {3, 1, 3, 2, 1}\nprint(s.len())\nprint({1,2,3}.union({3,4}).len())\nprint({1,2,3}.intersection({2,3,4}).len())\nprint({1,2,3}.difference({2,3}).len())\nprint({1,2} == {2,1})\n",
        "3\n4\n2\n1\ntrue\n",
    );
}

#[test]
fn set_mutation_and_iteration_parity() {
    assert_parity_out(
        "s := Set()\ns.add(10)\ns.add(10)\ns.add(20)\nprint(s.len())\nprint(s.remove(10))\nprint(s.remove(10))\ntotal := 0\nfor x in {5, 15, 25}:\n    total += x\nprint(total)\n",
        "2\ntrue\nfalse\n45\n",
    );
}

#[test]
fn set_display_parity() {
    assert_parity_out("print({1, 2, 3})\nprint(Set())\n", "{1, 2, 3}\nSet()\n");
}

#[test]
fn str_chars_parity() {
    assert_parity_out(
        "cs := \"héllo\".chars()\nprint(cs.len())\nprint(cs[1])\n",
        "5\né\n",
    );
}

#[test]
fn for_over_str_parity() {
    assert_parity_out(
        "out := \"\"\nfor c in \"abc\":\n    out = out + c + \"-\"\nprint(out)\n",
        "a-b-c-\n",
    );
}

#[test]
fn for_over_empty_str_parity() {
    assert_parity_out("n := 0\nfor c in \"\":\n    n += 1\nprint(n)\n", "0\n");
}

#[test]
fn for_over_map_key_value_parity() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2}\ns := 0\nfor k, v in m:\n    print(\"{k}={v}\")\n    s += v\nprint(s)\n",
        "a=1\nb=2\n3\n",
    );
}

#[test]
fn for_over_map_kv_mutation_during_iteration_parity() {
    // The body reassigns a not-yet-visited key; both engines must agree (snapshot semantics:
    // the value bound is the one captured at loop start, like list iteration).
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nout := 0\nfor k, v in m:\n    m[\"c\"] = 99\n    out += v\nprint(out)\n",
        "6\n",
    );
}

#[test]
fn for_over_map_kv_remove_during_iteration_parity() {
    // Removing a future key mid-iteration must not crash one engine while the other succeeds.
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2}\nfirst := true\nsum := 0\nfor k, v in m:\n    if first:\n        m.remove(\"b\")\n        first = false\n    sum += v\nprint(sum)\n",
        "3\n",
    );
}

#[test]
fn for_over_map_break_continue_parity() {
    // break/continue still target the index increment over the keys sequence.
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3, \"d\": 4}\nfor k, v in m:\n    if v == 2: continue\n    if v == 4: break\n    print(k)\n",
        "a\nc\n",
    );
}

#[test]
fn cmp_max_int_parity() {
    // Generic min/max now live in std.cmp; abs stays in std.math. File/graph path required.
    let out = parity_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3, 5))\n    print(cmp.min(3, 5))\n    print(math.abs(-5))\nmain()\n",
    );
    assert_eq!(out, "5\n3\n5\n");
}

#[test]
fn cmp_max_float_parity() {
    let out = parity_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n",
    );
    assert_eq!(out, "5.0\n2.5\n");
}

#[test]
fn cmp_max_struct_parity() {
    // The generic max over a Comparable struct must be byte-identical on both engines.
    let src = "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, o: P) -> int:\n        return self.n - o.n\nfn main():\n    print(cmp.max(P(2), P(9)).n)\n    print(cmp.min(P(2), P(9)).n)\nmain()\n";
    assert_eq!(parity_entry(src), "9\n2\n");
}

#[test]
fn ord_chr_parity() {
    assert_parity_out("print(ord(\"A\"))\nprint(chr(97))\n", "65\na\n");
}

#[test]
fn ord_chr_roundtrip_parity() {
    assert_parity_out("print(chr(ord(\"z\")))\n", "z\n");
}

#[test]
fn ord_index_digit_value_parity() {
    // The digit-value idiom over an indexed char.
    assert_parity_out("s := \"7\"\nprint(ord(s[0]) - ord(\"0\"))\n", "7\n");
}

#[test]
fn ord_empty_string_error_parity() {
    // Runtime error (checker can't catch it) — message must match across engines.
    assert_parity("print(ord(\"\"))\n");
}

#[test]
fn chr_invalid_codepoint_error_parity() {
    assert_parity("print(chr(-1))\n");
    assert_parity("print(chr(2000000))\n");
}

#[test]
fn sort_by_descending_parity() {
    assert_parity_out(
        "xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: b - a)\nprint(xs)\n",
        "[3, 2, 1]\n",
    );
}

#[test]
fn sort_by_stable_by_key_parity() {
    // Equal keys (string length) must keep input order — stability is part of the contract.
    assert_parity_out(
        "ws := [\"bb\", \"a\", \"dd\", \"e\"]\nws.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nprint(ws)\n",
        "[a, e, bb, dd]\n",
    );
}

#[test]
fn sort_by_comparator_mutates_list_parity() {
    // A comparator that mutates an element being sorted must behave identically on both engines.
    // Both sort a snapshot taken at call time and overwrite the list with the sorted result, so
    // the in-comparator `xs[0] = 100` is discarded.
    let src = "xs := [3, 1, 2]\nfn cmp(a: int, b: int) -> int:\n    xs[0] = 100\n    return a - b\nxs.sort_by(cmp)\nprint(xs)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "[1, 2, 3]\n");
}

#[test]
fn sort_by_empty_and_singleton_parity() {
    assert_parity_out(
        "xs := [42]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\nprint(xs)\n",
        "[42]\n",
    );
}

#[test]
fn break_early_for_parity() {
    assert_parity_out(
        "s := 0\nfor i in 0..10:\n    if i == 5: break\n    s += i\nprint(s)\n",
        "10\n",
    );
}

#[test]
fn continue_for_terminates_parity() {
    // THE increment-landing guard: `continue` must reach the loop's `i += 1`, never the
    // condition (would re-test the same `i` forever). If this hangs, the target is wrong.
    assert_parity_out(
        "for i in 0..5:\n    if i == 1: continue\n    if i == 3: continue\n    print(i)\n",
        "0\n2\n4\n",
    );
}

#[test]
fn while_break_parity() {
    assert_parity_out(
        "i := 0\nwhile true:\n    if i == 3: break\n    i += 1\nprint(i)\n",
        "3\n",
    );
}

#[test]
fn while_continue_progresses_parity() {
    // The counter advances BEFORE the `continue`, so the `while` still terminates.
    assert_parity_out(
        "i := 0\ns := 0\nwhile i < 5:\n    i += 1\n    if i == 2: continue\n    s += i\nprint(s)\n",
        "13\n",
    );
}

#[test]
fn break_in_if_in_loop_parity() {
    assert_parity_out(
        "for i in 0..10:\n    if i > 2:\n        break\n    print(i)\n",
        "0\n1\n2\n",
    );
}

#[test]
fn return_from_loop_parity() {
    // `return` inside a loop still returns the whole function (break/continue don't intercept it).
    assert_parity_out(
        "fn f():\n    for i in 0..10:\n        if i == 2: return i\n    return -1\nprint(f())\n",
        "2\n",
    );
}

#[test]
fn nested_loop_inner_break_parity() {
    // Inner `break` does not break the outer loop: the outer runs all 3 iterations.
    assert_parity_out(
        "n := 0\nfor i in 0..3:\n    for j in 0..3:\n        break\n    n += 1\nprint(n)\n",
        "3\n",
    );
}

#[test]
fn continue_list_for_parity() {
    // `continue` over a LIST for-loop (not just range) advances to the next element.
    assert_parity_out(
        "for x in [1,2,3,4]:\n    if x % 2 == 0: continue\n    print(x)\n",
        "1\n3\n",
    );
}

#[test]
fn break_list_for_parity() {
    assert_parity_out(
        "for x in [10,20,30,40]:\n    if x == 30: break\n    print(x)\n",
        "10\n20\n",
    );
}

// ----- literal + wildcard match parity -----

#[test]
fn match_int_literals_stmt_parity() {
    assert_parity(
        "n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n",
    );
}

#[test]
fn match_str_literals_expr_parity() {
    assert_parity("c := \"x\"\ns := match c:\n    \"a\": \"first\"\n    _: \"other\"\nprint(s)\n");
}

#[test]
fn match_bool_literals_parity() {
    assert_parity(
        "b := false\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n",
    );
}

#[test]
fn match_literal_matched_arm_parity() {
    // The matching literal arm fires (wildcard not reached).
    assert_parity("n := 1\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
}

#[test]
fn match_wildcard_reached_parity() {
    // No literal matches → the `_` arm fires.
    assert_parity("n := 9\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
}

#[test]
fn match_variant_regression_parity() {
    // A variant match still lowers via the variant path unchanged.
    assert_parity(
        "o := Some(5)\nmatch o:\n    Some(v): print(\"got {v}\")\n    None: print(\"none\")\n",
    );
}

#[test]
fn parity_std_math() {
    let src = "\
import std.math
fn main():
    print(math.floor(2.7))
    print(math.ceil(2.1))
    print(math.sqrt(16.0))
    print(math.pow(2.0, 10.0))
    print(math.abs(0.0 - 3.5))
    print(math.round(2.5))
    print(math.pi)
main()";
    assert_eq!(
        parity_entry(src),
        "2.0\n3.0\n4.0\n1024.0\n3.5\n3.0\n3.141592653589793\n"
    );
}

#[test]
fn parity_std_math_sqrt_negative_is_nan() {
    // math.sqrt of a negative is IEEE NaN — never faults, identical on both engines.
    let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
    assert_eq!(parity_entry(src), "NaN\n");
}

#[test]
fn parity_float_ieee_div_mod() {
    // Float division/modulo by zero is total IEEE-754 on both engines: inf / NaN, never a fault.
    let src = "fn main():\n    print(1.0 / 0.0)\n    print(-1.0 / 0.0)\n    print(0.0 / 0.0)\n    print(5.0 % 0.0)\nmain()";
    assert_eq!(parity_entry(src), "inf\n-inf\nNaN\nNaN\n");
}

#[test]
fn parity_int_div_by_zero_still_faults() {
    // INTEGER division by zero still faults — caught + printed identically on both engines.
    let src = "fn run() -> int!:\n    r := recover:\n        1 / 0\n    match r:\n        Ok(v): return Ok(v)\n        Err(e): print(e.message())\n    return Ok(0)\nfn main():\n    _ := run()\nmain()";
    assert_eq!(parity_entry(src), "division by zero\n");
}

#[test]
fn parity_large_int_equality_is_exact() {
    // Two distinct i64 values above 2^53 must NOT compare equal (Python parity). Previously both
    // engines compared ints via `as_f64`, so 2^62+1 and 2^62+2 (both round to 2^62 in f64) wrongly
    // compared EQUAL. Cross-type `1 == 1.0` must STILL be true (int/float interop preserved).
    let src = "fn main():\n    a := 4611686018427387905\n    b := 4611686018427387906\n    print(a == b)\n    print(a != b)\n    print(a == a)\n    print(1 == 1.0)\n    print(2 == 3)\nmain()";
    assert_parity_out(src, "false\ntrue\ntrue\ntrue\nfalse\n");
}

#[test]
fn parity_large_int_map_keys_distinct() {
    // Two distinct large ints are distinct map keys (they were collapsed to one when eq was f64).
    // `1` and `1.0` still collapse to a single key (cross-type numeric key equality preserved).
    let src = "fn main():\n    m := {4611686018427387905: 1, 4611686018427387906: 2}\n    print(m.len())\n    n := {1: 10, 1.0: 20}\n    print(n.len())\nmain()";
    assert_parity_out(src, "2\n1\n");
}

#[test]
fn recover_tail_stmt_match_value_run_parity() {
    // A `recover:` whose TAIL is a statement-form `match` with value-producing arms yields
    // `Ok(<arm value>)` — the value is NOT dropped (the old bug wrapped `Ok(nil)`). v=100.
    let src = "fn main():\n    r := recover:\n        x := 3\n        match x:\n            3: 100\n            _: 200\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()";
    assert_parity_out(src, "v=100\n");
}

#[test]
fn recover_tail_stmt_if_value_run_parity() {
    // Trailing statement-form `if/else` analog: the taken branch's value is the `Ok` payload.
    let src = "fn main():\n    r := recover:\n        x := 3\n        if x == 3:\n            100\n        else:\n            200\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()";
    assert_parity_out(src, "v=100\n");
}

#[test]
fn recover_tail_stmt_match_value_defer_in_arm_run_parity() {
    // A `defer` inside a value-producing tail-match arm must run for effect WITHOUT clobbering the
    // trailing value that becomes the `Ok` payload (defers touch frame.deferred, never the stack).
    let src = "fn main():\n    r := recover:\n        x := 3\n        match x:\n            3:\n                defer print(\"cleanup\")\n                100\n            _: 200\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()";
    assert_parity_out(src, "cleanup\nv=100\n");
}

#[test]
fn recover_tail_match_value_catches_fault_is_err() {
    // Must-not-break: even though the block is now value-typed, a fault raised BEFORE the tail-match
    // is still caught and converted to `Err` (single-Result stack invariant preserved).
    let src = "fn main():\n    r := recover:\n        xs := [1, 2]\n        y := xs[9]\n        match y:\n            _: 100\n    match r:\n        Ok(v): print(\"ok={v}\")\n        Err(e): print(\"err\")\nmain()";
    assert_parity_out(src, "err\n");
}

#[test]
fn recover_tail_match_heterogeneous_arms_run_parity() {
    // REGRESSION GUARD: heterogeneous tail-match arms (str vs int) fall back to `Result[nil]` at the
    // checker but the compiler still compiles the tail as a value — whichever arm runs, its value is
    // `Ok`-wrapped and simply IGNORED by `Ok(_)`. The program must check + RUN identically on both
    // engines (the first cut of the feature rejected it at `check`).
    let src = "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        Ok(_): print(\"done\")\n        Err(e): print(\"failed\")\nfoo(\"a\")";
    assert_parity_out(src, "done\n");
}

#[test]
fn recover_tail_if_heterogeneous_branches_run_parity() {
    // The `if/else` analog of the heterogeneous fall-back: runs, value ignored, both engines agree.
    let src = "fn foo(n: int):\n    r := recover:\n        if n == 0:\n            \"zero\"\n        else:\n            n\n    match r:\n        Ok(_): print(\"done\")\n        Err(e): print(\"failed\")\nfoo(0)";
    assert_parity_out(src, "done\n");
}

#[test]
fn parity_std_math_predicates() {
    // is_nan / is_inf / is_finite — float predicates returning bool, identical on both engines.
    let src = "import std.math\nfn main():\n    print(math.is_nan(0.0 / 0.0))\n    print(math.is_inf(1.0 / 0.0))\n    print(math.is_finite(1.0))\n    print(math.is_finite(1.0 / 0.0))\nmain()";
    assert_eq!(parity_entry(src), "true\ntrue\ntrue\nfalse\n");
}

#[test]
fn parity_nan_ordered_compare_is_false() {
    // Ordered comparisons (`< <= > >=`) involving NaN are ALWAYS false on both engines — never a
    // fault — matching IEEE-754 / Python / Rust. Equality is untouched (`nan == nan` → false,
    // `nan != nan` → true). Normal float compares still work (regression guard).
    let src = "fn main():\n    nan := 0.0 / 0.0\n    print(nan < 1.0)\n    print(nan <= 1.0)\n    print(nan > 1.0)\n    print(nan >= 1.0)\n    print(1.0 < nan)\n    print(1.0 > nan)\n    print((1.0 / 0.0) < nan)\n    print(1.0 < 2.0)\n    print(2.0 > 1.0)\n    print(nan == nan)\n    print(nan != nan)\nmain()";
    assert_parity_out(
        src,
        "false\nfalse\nfalse\nfalse\nfalse\nfalse\nfalse\ntrue\ntrue\nfalse\ntrue\n",
    );
}

#[test]
fn parity_sort_by_key_nan_float_key_deterministic() {
    // `sort_by_key` with a float key that can be NaN sorts deterministically (total order, NaN to
    // one end) instead of faulting — consistent with `sort()`. The SIGN of `0.0/0.0` (hence
    // whether NaN ranks at the front or back) is platform-dependent (negative on x86 SSE2,
    // possibly positive elsewhere), so we do NOT bake an absolute position into a golden — that
    // would be a non-portable test. `assert_parity` proves the real guarantees portably: the sort
    // never faults and VM↔interp agree byte-identically on whatever order this machine produces.
    let src = "fn main():\n    xs := [1.0, 0.0 / 0.0, 2.0, 0.0 / 0.0, 0.5]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity(src);
}

#[test]
fn parity_sort_by_key_signed_zero_matches_sort() {
    // `sort_by_key` over a float key uses `total_cmp` for the WHOLE comparison, exactly like
    // `sort()` — so they agree even on `-0.0`/`+0.0`, which `partial_cmp` ranks Equal but
    // `total_cmp` orders `-0.0 < +0.0`. Signed-zero order is invisible to `==` (`-0.0 == +0.0`),
    // so observe it via `1.0/x` → `-inf` for `-0.0`, `+inf` for `+0.0`. Both sort paths must put
    // `-0.0` first ⇒ `-inf` then `inf`. Platform-independent (no NaN sign involved).
    let by_sort = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort()\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_parity_out(by_sort, "-inf\ninf\n");
    let by_key = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_parity_out(by_key, "-inf\ninf\n");
}

#[test]
fn parity_sort_by_key_normal_key_unchanged() {
    // Behavior-preserving guard: non-NaN float keys and int keys sort exactly as before — Part B
    // touches only the NaN float-key path.
    let fsrc = "fn main():\n    xs := [3.0, 1.0, 2.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity_out(fsrc, "1.0\n2.0\n3.0\n");
    let isrc = "fn main():\n    xs := [3, 1, 2]\n    xs.sort_by_key(fn(x: int) -> int: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity_out(isrc, "1\n2\n3\n");
}

#[test]
fn math_abs_min_overflows() {
    // `math.abs(i64::MIN)` has no representable result. Raw `i64::abs()` would panic (debug) or
    // wrap (release); the native fn must surface a recoverable overflow like every other op,
    // identically on both engines. i64::MIN is built as `-MAX - 1` (the literal
    // `9223372036854775808` overflows the lexer).
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    print(math.abs(x))\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let ie = run_file_p(&entry).2.unwrap_err().to_string();
    let ve = run_file(&entry).2.unwrap_err().to_string();
    assert_eq!(
        ie, ve,
        "abs-overflow error must be identical on both engines"
    );
    assert!(ie.contains("integer overflow in abs"), "{ie}");
}

#[test]
fn math_abs_min_overflow_is_recoverable() {
    // The overflow is a normal recoverable fault: `recover:` turns it into an Err, not a crash.
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    r := recover:\n        math.abs(x)\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()";
    let out = parity_entry(src);
    assert!(out.contains("integer overflow in abs"), "{out}");
}

#[test]
fn exit_threads_code_through_both_engines() {
    // `std.os.exit(code)` halts the program with that exit code on both engines: output before
    // the call is preserved, the statement after it never runs, and the run is not an error.
    let src = "import std.os\nfn main():\n    print(\"before\")\n    os.exit(3)\n    print(\"after\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file_p(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert_eq!(io, "before\n", "interp stdout");
    assert_eq!(vo, "before\n", "vm stdout");
    assert_eq!(ic, Some(3), "interp exit code");
    assert_eq!(vc, Some(3), "vm exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "exit is not a runtime error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn defer_top_level_skipped_by_os_exit() {
    // `std.os.exit` is a hard halt — top-level defers do NOT run through it (matches Go's
    // `os.Exit`, and the existing frame/recover bypass).
    let src = "import std.os\nfn log(s: str):\n    print(s)\ndefer log(\"cleanup\")\nprint(\"before\")\nos.exit(2)\nprint(\"after\")\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file_p(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert_eq!(io, "before\n", "interp: cleanup defer skipped by os.exit");
    assert_eq!(vo, "before\n", "vm: cleanup defer skipped by os.exit");
    assert_eq!(ic, Some(2), "interp exit code");
    assert_eq!(vc, Some(2), "vm exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "exit is not a runtime error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn exit_negative_code_masks_to_255() {
    // `os.exit(code)` reports the LOW 8 BITS of `code` (POSIX `exit(3)`/bash/Python/Go). A NEGATIVE
    // code used to be clamped UP to 0 — i.e. `os.exit(-1)`, the canonical failure idiom, reported
    // SUCCESS to the shell/CI. The process-status side of this rule is pinned by tests/exit_status.rs;
    // this pins the in-VM code on both engines (including the cross-thread re-store in sched.rs).
    for (code, want) in [("-1", 255), ("300", 44), ("-256", 0), ("0", 0)] {
        let src = format!("import std.os\nfn main():\n    os.exit({code})\nmain()");
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let (_io, _ie, ir, ic) = run_file_p(&entry);
        let (_vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(ic, Some(want), "M:N exit code for os.exit({code})");
        assert_eq!(vc, Some(want), "serial exit code for os.exit({code})");
        assert!(
            ir.is_ok() && vr.is_ok(),
            "exit is a clean halt, not an error"
        );
    }
}

// ----- re-entrant `.next()` on a currently-running generator -----

#[test]
fn generator_reentrant_next_faults() {
    // A `.next()` on a generator that is ALREADY RUNNING (re-entered from inside its own body) used
    // to hit the `GenState::Done` placeholder the resume path parks in the heap object while the
    // generator runs, and silently answered `None` — a live, non-exhausted generator reporting
    // EXHAUSTION, i.e. a silently-wrong `Option`. It must be a clean, recoverable fault instead
    // (Python: `ValueError: generator already executing`).
    let src = r#"
holder: List[Iterator[int]] = []

fn gen():
    yield 1
    print("reentrant: {holder[0].next()}")
    yield 2

fn main():
    g := gen()
    holder.push(g)
    for x in g:
        print(x)
main()
"#;
    assert_parity(src);
    let e = vm_outcome(src).expect_err("a re-entrant resume must fault, not answer None");
    assert!(e.contains("generator already running"), "{e}");
    // The value the consumer already pulled is neither lost nor duplicated, and the bogus
    // `reentrant: None` never prints. (`run_program` keeps stdout across the fault.)
    let (out, res) = run_program(src);
    assert!(res.is_err(), "the run faults");
    assert_eq!(
        out, "1\n",
        "only the first yielded value, then the fault: {out:?}"
    );
}

#[test]
fn generator_reentrancy_fault_is_recoverable() {
    // The fault is an ordinary RuntimeError: catchable by `recover:`, never a host panic. Also
    // fires when the re-entrancy is a `for` over the SAME generator (the for-loop intrinsic routes
    // through the same resume path as an explicit `.next()`).
    let src = r#"
holder: List[Iterator[int]] = []

fn gen():
    yield 1
    for y in holder[0]:
        print("never: {y}")
    yield 2

fn main():
    g := gen()
    holder.push(g)
    x := recover:
        for v in g:
            print(v)
        0
    match x:
        Ok(_): print("no fault")
        Err(e): print("caught: {e.message()}")
    print("still alive")
main()
"#;
    assert_parity(src);
    let out = run_capture(src).expect("the fault is CAUGHT — the program exits Ok, no host panic");
    assert!(out.contains("caught:"), "the fault is recoverable: {out:?}");
    assert!(
        out.contains("generator already running"),
        "the recovered error names the cause: {out:?}"
    );
    assert!(
        out.ends_with("still alive\n"),
        "execution continues: {out:?}"
    );
    assert!(
        !out.contains("never:"),
        "the re-entrant loop never yields: {out:?}"
    );
}

#[test]
fn generator_guard_clears_on_every_unwind_path() {
    // The guard is the VM's existing `active_generators` root list, pushed on resume and popped on
    // EVERY exit path — so it is self-clearing and can never poison a generator as permanently
    // "running". One case per unwind path.
    // (a) normal exhaustion, (b) an early `break` then a legitimate resume of the SAME generator,
    // (c) a generator whose body FAULTS inside a `recover:` — after recovery a fresh generator still
    //     runs to completion (and the faulted one is closed, like a Python generator: `.next()` → None),
    // (d) a generator driving a DIFFERENT generator (distinct GcRefs) — no over-rejection.
    let src = r#"
fn count(n: int):
    for i in 0..n:
        yield i

fn boom():
    yield 1
    print(1 / 0)
    yield 2

fn outer():
    for v in count(2):
        yield v * 10

fn main():
    # (a) fully consumed, then a second generator works
    for x in count(2):
        print("a {x}")
    for x in count(2):
        print("a2 {x}")

    # (b) break mid-`for`, then resume the SAME generator by hand — the guard is not stuck
    g := count(4)
    for x in g:
        print("b {x}")
        break
    print("b-resume {g.next()}")

    # (c) a faulting body, recovered — then a FRESH generator still runs; the faulted one is closed
    bad := boom()
    r := recover:
        for x in bad:
            print("c {x}")
        0
    match r:
        Ok(_): print("c no fault")
        Err(_): print("c caught")
    # a generator whose body faulted is CLOSED (like Python) — a later resume answers None
    print("c closed {bad.next()}")
    for x in count(2):
        print("c-after {x}")

    # (d) a generator driving another generator — distinct generators, both resume fine
    for x in outer():
        print("d {x}")
main()
"#;
    assert_parity(src);
    let out = run_capture(src).expect("no path leaves a generator poisoned as running");
    let want = "a 0\na 1\na2 0\na2 1\nb 0\nb-resume Some(1)\nc 1\nc caught\nc closed None\nc-after 0\nc-after 1\nd 0\nd 10\n";
    assert_eq!(out, want, "every unwind path clears the running guard");
}

#[test]
fn defer_top_level_runs_on_unhandled_error() {
    // An unhandled top-level `?` error still unwinds through the module body's defers (cleanup
    // runs before the program reports the error).
    let src = "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"nope\")\ndefer log(\"cleanup\")\nprint(\"before\")\nx := boom()?\nprint(\"after\")\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let (vo, _ve, vr, _vc) = run_file(&entry);
    assert_eq!(
        io, "before\ncleanup\n",
        "interp: top-level defer runs on unhandled error"
    );
    assert_eq!(
        vo, "before\ncleanup\n",
        "vm: top-level defer runs on unhandled error"
    );
    assert!(
        ir.is_err() && vr.is_err(),
        "unhandled `?` is an error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn exit_is_not_caught_by_recover() {
    // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
    let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, _ir, ic) = run_file_p(&entry);
    let (vo, _ve, _vr, vc) = run_file(&entry);
    assert_eq!(io, "", "interp: nothing after the recover runs");
    assert_eq!(vo, "", "vm: nothing after the recover runs");
    assert_eq!(ic, Some(7), "interp exit code");
    assert_eq!(vc, Some(7), "vm exit code");
}

#[test]
fn exit_in_spawned_child_aborts_siblings() {
    // B1/B2: `std.os.exit` inside a child is a hard halt for the PROGRAM — the post-`parallel:`
    // statement never runs and the exit code propagates. It is NOT a halt for the tasks the nursery
    // has already spawned: those are torn down through the scope cancel, and (cancellation points) a
    // spawned task always runs its straight-line prologue before it can observe that cancel. M:N has
    // no choice about this — a scope completes only at `done == total`, so `b`'s queued fiber is
    // popped and started after the exit trips the cancel, and it prints (measured 20/20). Serial's
    // cancel drain therefore starts `b` too, and the two engines AGREE: `{"a","b"}` on both. (Before
    // the N6 drain, serial abandoned the never-started sibling and printed only `"a"` — a
    // near-deterministic line-SET divergence this test used to bless as "expected".) Cross-task
    // ORDER stays nondeterministic on both engines, so the SET is what is asserted.
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file_p(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert!(
        vo.contains('a') && vo.contains('b'),
        "serial: the exiting child's output flushes and its already-spawned sibling still runs its prologue: got {vo:?}"
    );
    assert!(
        io.contains('a') && io.contains('b'),
        "M:N: same: got {io:?}"
    );
    assert!(
        !io.contains("after") && !vo.contains("after"),
        "the post-parallel statement never runs after os.exit: mn={io:?} serial={vo:?}"
    );
    assert_same_lines(&vo, &io);
    assert_eq!(vc, Some(3), "serial exit code");
    assert_eq!(ic, Some(3), "M:N exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "os.exit is a clean halt, not an error: mn={ir:?} serial={vr:?}"
    );
}

/// B3.4: a child `std.os.exit(code)` on the `--parallel` OS-thread pool is a clean hard-halt,
/// not a fault. Cross-thread: the worker's `pending_exit` propagates up the join to the parent
/// VM, the exiting child's buffered output is flushed, and the post-`parallel:` statement never
/// runs. The `--parallel` counterpart of `exit_in_spawned_child_aborts_siblings`.
#[test]
fn parallel_child_os_exit_halts_with_code() {
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(3),
        "child os.exit code propagates cross-thread to the parent"
    );
    assert!(
        res.is_ok(),
        "os.exit is a clean halt, not an error: {res:?}"
    );
    assert!(
        out.contains('a'),
        "the exiting child's buffered output is flushed: got {out:?}"
    );
    assert!(
        !out.contains("after"),
        "the post-parallel statement never runs after os.exit: got {out:?}"
    );
}

/// B3.4: `os.exit` in one child aborts a `recv`-blocked sibling too (same machinery as a fault —
/// it trips the nursery cancel flag), so the join completes with the exit code instead of hanging.
/// `exiter` is spawned first → runs inline on the joining thread (the exit trips cancel without
/// depending on pool scheduling); the recv-blocked `consumer` runs on the pool and aborts.
#[test]
fn parallel_os_exit_aborts_recv_blocked_sibling() {
    let src = "import std.os\nfn exiter(ch: Channel[int]):\n    os.exit(5)\nfn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn exiter(ch)\n        spawn consumer(ch)\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, _res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(5),
        "os.exit code propagates; the recv-blocked consumer aborts, no hang"
    );
    assert!(
        !out.contains("consumed"),
        "the aborted consumer never ran past its blocked recv: got {out:?}"
    );
}

/// B3.5 — run an entry on the `--parallel` engine under a watchdog: a missing/broken deadlock
/// detector would hang the nursery forever, so we run it on a side thread and fail loudly if it
/// doesn't finish, instead of wedging the whole test binary. (On a clean detector none of these
/// ever time out — the leak only happens on the failure path we're guarding against.)
fn run_parallel_watchdog(src: &str) -> RunOutput {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => {
            panic!("hung: --parallel nursery did not terminate (deadlock detection missing/broken)")
        }
    }
}

/// gaps.md B5 — cross-nursery child→parent wake parity: a `send` from a nested (eager) `parallel:`
/// nursery must wake a receiver parked in the OUTER nursery. Uncontended 1-send / 1-recv, so the serial
/// oracle == M:N. Pre-fix M:N false-faulted `deadlock` while serial printed "receiver got 1". Runs the
/// M:N leg under a watchdog (10 rounds) so a lost-wakeup regression fails loud rather than hanging.
#[test]
fn parallel_cross_nursery_nested_send_to_outer_recv_parity() {
    let src = include_str!("../../examples/parallel_cross_nursery_nested_send_to_outer_recv.chz");
    let expected =
        include_str!("../../examples/parallel_cross_nursery_nested_send_to_outer_recv.expected");
    // Serial oracle.
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (so, _se, sr, _) = run_file(&entry);
    assert!(sr.is_ok(), "serial oracle faulted: {sr:?}");
    assert_eq!(so, expected, "serial oracle != expected");
    // M:N leg under a watchdog, several rounds to shake a flaky lost wakeup.
    for _ in 0..10 {
        let (mo, _me, mr, _) = run_parallel_watchdog(src);
        assert!(
            mr.is_ok(),
            "M:N faulted (spurious cross-nursery deadlock / lost wakeup): {mr:?}"
        );
        assert_eq!(mo, expected, "M:N stdout != serial oracle");
    }
}

/// B3.5 — the cooperative `fibers_all_blocked_is_deadlock` golden, ported to `--parallel`: two
/// tasks each block on a distinct empty channel with no producer. The cooperative scheduler
/// already faults this; under threads B3.5's nursery-local detector must fault it too rather
/// than hang on the condvars.
#[test]
fn parallel_all_blocked_deadlock_faults() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
    let (_o, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("an all-blocked --parallel nursery must fault, not hang");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// A blocking `for v in ch:` (recv) with no producer and no runnable sibling deadlocks on BOTH
/// engines with an ENGINE-AGNOSTIC message — the old text hardcoded "sequential executor", which
/// is misleading under the default M:N (real-thread) engine. Same code path serves both engines.
#[test]
fn deadlock_fault_message_is_engine_agnostic() {
    let msg = parity_entry_fault(
        "fn main():\n    ch := Channel[int]()\n    for v in ch:\n        print(v)\nmain()\n",
    );
    assert!(msg.contains("deadlock"), "got: {msg}");
    assert!(!msg.contains("sequential executor"), "got: {msg}");
}

/// The reworded deadlock fault stays catchable by `recover:` on BOTH engines, surfacing the new
/// engine-agnostic text (catchability is text-independent, but pin it so a future reword can't
/// silently make it uncatchable).
#[test]
fn deadlock_fault_is_recoverable_new_message() {
    let src = "fn main():\n\
               \x20   ch := Channel[int]()\n\
               \x20   r := recover:\n\
               \x20       for v in ch:\n\
               \x20           print(v)\n\
               \x20   match r:\n\
               \x20       Ok(_): print(\"ok\")\n\
               \x20       Err(e): print(\"caught: {e.message()}\")\n\
               main()\n";
    let out = assert_parity_file(&[("main.chz", src)], "main.chz");
    assert!(out.contains("caught:"), "got: {out}");
    assert!(out.contains("deadlock"), "got: {out}");
    assert!(!out.contains("sequential executor"), "got: {out}");
}

/// `std.cancel` wakeup regression — a sibling's `cancel()` (which `trip()`s the token's `done()`
/// channel) must wake a fiber **parked** in `wait: tok.done().recv()` under the OS-thread engine.
/// The canceller sleeps first so the waiter reliably reaches `park_wait` and blocks; the `trip()`
/// then routes through `close_wake`, which must reach the parked `WaitPark` token and re-poll it to
/// observe the latch. (The *narrower* park-decision-vs-`trip` race is closed by adding `done_latch`
/// to `MnSched::park`/`park_wait`'s gap re-check — correct by construction, mirroring how `closed`
/// is handled; not deterministically forceable from source.) All rounds must complete.
#[test]
fn cancel_trip_wakes_parked_wait_under_parallel() {
    let src = "import std.cancel\n\
                   import std.time\n\
                   fn waiter(tok: Token, out: Channel[bool]):\n\
                   \x20   wait:\n\
                   \x20       _ := tok.done().recv(): out.send(true)\n\
                   fn canceller(tok: Token):\n\
                   \x20   time.sleep_ms(5)\n\
                   \x20   tok.cancel()\n\
                   fn main():\n\
                   \x20   out := Channel[bool]()\n\
                   \x20   n := 30\n\
                   \x20   for i in 0..n:\n\
                   \x20       tok := cancel.manual()\n\
                   \x20       parallel:\n\
                   \x20           spawn waiter(tok, out)\n\
                   \x20           spawn canceller(tok)\n\
                   \x20   c := 0\n\
                   \x20   for _ in 0..n:\n\
                   \x20       out.recv()\n\
                   \x20       c = c + 1\n\
                   \x20   print(c)\n\
                   main()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "cancel→wait must not fault/strand: {res:?}");
    assert_eq!(
        out, "30\n",
        "every parked waiter must be woken by the sibling's cancel"
    );
}

/// B3.5 — the named anti-false-positive case: one sibling `send`s the very channel the other
/// `recv`s, so the nursery genuinely progresses. The barrier-confirm detector must NOT report a
/// deadlock (a real send aborts any half-built all-blocked confirmation).
#[test]
fn parallel_near_miss_does_not_false_positive() {
    let src = "fn consumer(c: Channel[int]):\n    print(c.recv())\nfn producer(c: Channel[int]):\n    c.send(7)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn consumer(c)\n        spawn producer(c)\n    print(\"done\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "near-miss must not fault: {res:?}");
    assert!(
        out.contains('7'),
        "consumer received the sent value: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
}

/// B3.5 — a three-task relay (consumer ← relay ← producer) where `blocked == live` is reached
/// only momentarily while a message is in flight. A naive blocked-count detector false-positives
/// here; the per-epoch barrier (a worker holding a deliverable message pops it instead of
/// confirming empty) must not.
#[test]
fn parallel_chained_near_miss_no_false_positive() {
    let src = "fn relay(x: Channel[int], z: Channel[int]):\n    v := x.recv()\n    z.send(v)\nfn producer(x: Channel[int]):\n    x.send(1)\nfn consumer(z: Channel[int]):\n    print(z.recv())\nfn main():\n    x := Channel[int]()\n    z := Channel[int]()\n    parallel:\n        spawn consumer(z)\n        spawn relay(x, z)\n        spawn producer(x)\n    print(\"ok\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "chained relay must not false-positive: {res:?}"
    );
    assert!(
        out.contains('1'),
        "the relayed value reached the consumer: {out:?}"
    );
    assert!(out.contains("ok"), "the nursery joined: {out:?}");
}

/// D6 — single-connection loopback over `std.net` on the M:N engine: a `parallel:` runs a server
/// fiber (`listen`/`accept`/`read`/`write`) and a client fiber (`connect`/`write`/`read`) in one
/// program. A would-block `accept`/`read` parks on the netpoller and resumes on readiness, so the
/// round-trip completes without a thread per op. `Listener.addr()` surfaces the OS-assigned port so
/// the client can reach the `:0` bind. Watchdog-guarded.
#[test]
fn net_loopback_round_trip_over_parallel() {
    let src = r#"import std.net

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("hello")?
    reply := sock.read(64)?
    print(reply)
    sock.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("done")
        Err(e): print("net error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "net round-trip must not fault: {res:?}");
    assert!(
        out.contains("echo:hello"),
        "the client received the server's echo: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
    assert!(
        !out.contains("net error"),
        "no I/O error on the happy path: {out:?}"
    );
}

/// D6c — run a `--parallel` net program from a temp file under a 30 s watchdog (net round-trips
/// can legitimately take longer than `run_parallel_watchdog`'s 5 s, and a regressed timeout would
/// HANG rather than fault). Returns the captured stdout, or panics loudly on a hang.
fn run_net_timeout_watchdog(tag: &str, src: &str) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "{tag}: program faulted: {res:?}");
            out
        }
        Err(_) => panic!("{tag}: hung — D6c socket timeout regressed (the op parked forever)"),
    }
}

/// D6c — `conn.read(n, timeout_ms)` returns `Err("timeout")` when the peer accepts but never
/// writes. The server accepts the connection and then sleeps past the client's read timeout; the
/// client's `read(64, 100)` parks on the netpoller with a 100 ms deadline, the deadline fires
/// before any data, and the rewound op returns `Err` with `e.message() == "timeout"`.
#[test]
fn read_timeout_returns_err() {
    let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(400)
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64, 100):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "read(64, 100) must surface Err(\"timeout\"): {out:?}"
    );
    assert!(
        !out.contains("GOT:"),
        "no data should have been read: {out:?}"
    );
}

/// B1 — a `read(n)` whose chunk ends mid-codepoint must NOT corrupt the text. The client reads a
/// multibyte payload ONE BYTE AT A TIME (the ordinary read-in-a-loop idiom): the incomplete tail is
/// carried on the socket core and prepended to the next read, so the reassembly is byte-exact.
/// Before the fix `String::from_utf8_lossy` turned `é` into two U+FFFD.
#[test]
fn net_read_reassembles_split_codepoint_over_parallel() {
    let src = "\
import std.net

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    conn.write(\"héllo\")?
    conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    acc := \"\"
    while true:
        chunk := sock.read(1)?
        if chunk == \"\":
            break
        acc = acc + chunk
    print(\"GOT:\" + acc)
    sock.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("split_codepoint", src);
    assert!(
        out.contains("GOT:héllo"),
        "a byte-at-a-time read must reassemble the multibyte text exactly: {out:?}"
    );
    assert!(
        !out.contains('\u{fffd}'),
        "no replacement chars — the seam must never lossily decode: {out:?}"
    );
}

/// B1 — a genuinely-invalid UTF-8 byte on the wire (a lone `0xFF` ⇒ `Utf8Error::error_len() ==
/// Some(1)`) must surface a clear, recoverable `Err` naming the str-only limitation, NOT a silent
/// U+FFFD. Peer is a raw Rust `TcpListener` so we can put arbitrary bytes on the wire.
///
/// Review #2/#5 — and the `Err` must NOT SHRED the stream. The first cut dropped the WHOLE chunk it
/// had already taken off the fd (`carry.clear()`), so the valid text before the bad byte (`"hi"`) and
/// everything after it vanished — a recoverable `Err` that silently eats up to `MAX_SOCKET_READ` of
/// payload is the same data-loss family B1 exists to kill. Now: the valid prefix is DELIVERED, the
/// undecodable remainder STAYS carried, and the `Err` is STICKY (a str-only seam can never hand those
/// bytes back, so every later read re-decodes them and re-errs identically — no byte is ever silently
/// consumed-and-dropped, and a log-and-continue caller cannot shred the stream).
#[test]
fn net_read_invalid_utf8_errs_not_replacement_chars() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"hi\xFFthere").unwrap();
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    i := 0
    while i < 3:
        match sock.read(64):
            Ok(s): print(\"GOT:[\" + s + \"]\")
            Err(e): print(\"ERR:\" + e.message())
        i = i + 1
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("invalid_utf8", &src);
    server.join().unwrap();
    assert!(
        out.contains("invalid utf-8 on the socket"),
        "an invalid byte sequence must surface the str-only Err: {out:?}"
    );
    assert!(
        out.contains("read_bytes"),
        "the Err must point at the real limitation/future path: {out:?}"
    );
    assert!(
        out.contains("GOT:[hi]"),
        "the valid text BEFORE the bad byte must still be delivered, never swallowed by the Err: \
         {out:?}"
    );
    assert!(
        out.matches("invalid utf-8 on the socket").count() >= 2,
        "the Err is sticky — the undecodable bytes stay carried, so a log-and-continue caller \
         re-errs instead of silently shredding the stream: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #1) — `read(0)` on a CLOSED socket must still be `Err("read on a closed socket")`. The
/// first cut early-returned `Ok("")` for `n == 0` BEFORE taking the stream lock, and the `guard.as_mut()`
/// `None` arm is the ONLY closed-socket detector on the read path — so `read(0)` (or any caller-computed
/// `read(want - have)` that lands on 0) silently converted a recoverable error into a value that is
/// indistinguishable from the EOF sentinel.
#[test]
fn net_read_zero_on_closed_socket_errs() {
    use std::net::TcpListener;

    // Bound-but-never-accepted: the connect completes out of the listen backlog, so no peer thread is
    // needed. `listener` is held alive for the whole test.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    sock.close()
    match sock.read(0):
        Ok(s): print(\"OK:[\" + s + \"]\")
        Err(e): print(\"ERR:\" + e.message())
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("read_zero_closed", &src);
    drop(listener);
    assert!(
        out.contains("ERR:read on a closed socket"),
        "read(0) must not mask the closed-socket Err as a success indistinguishable from EOF: {out:?}"
    );
}

/// B1 (review #3/#8) — a POLL-ONCE read (`read(n, 0)`) that DID take bytes off the fd but landed
/// mid-codepoint must not report `Err("timeout")`. `timeout` is documented as "no data within
/// timeout_ms"; here data arrived, it just did not complete a codepoint — and 1–3 bytes were
/// permanently removed from the wire into the socket's carry. The distinct `Err("incomplete utf-8:
/// …")` names what actually happened and says the bytes are retained, so a retry on the same socket
/// recovers them byte-exactly (asserted: the full `"éllo"` still arrives).
#[test]
fn net_read_poll_once_mid_codepoint_errs_incomplete_not_timeout() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The lead byte of `é` alone, then a stall — the poll-once read takes it and can complete
        // nothing.
        stream.write_all(b"\xC3").unwrap();
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        stream.write_all(b"\xA9llo").unwrap();
        stream.flush().unwrap();
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    acc := \"\"
    seen := 0
    while true:
        match sock.read(8, 0):
            Ok(s):
                if s == \"\":
                    break
                acc = acc + s
            Err(e):
                m := e.message()
                if m != \"timeout\" and seen == 0:
                    print(\"ERR:\" + m)
                    seen = 1
    print(\"ACC:\" + acc)
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("poll_once_mid_codepoint", &src);
    server.join().unwrap();
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "a poll-once read that consumed bytes must not lie about it as a deadline expiry: {out:?}"
    );
    assert!(
        out.contains("ACC:éllo"),
        "the carried lead byte is retained across the Err and recovered by the next read: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #7) — `timeout_ms` must bound the WHOLE `read` call on the DEMOTE path too. A `read`
/// reached inside a native callback (`native_reentry > 0` — here a `list.map`) cannot snapshot-park
/// onto the netpoller, so it demotes to [`Vm::demote_block_socket`] — which took no deadline and
/// looped on `wait_fd_ready` until readiness/cancel/terminate. Once B1 routed a mid-codepoint read
/// back into that wait, `read(8, 200)` against a peer that sends the lead byte and STALLS blocked
/// indefinitely: the demoted op is accounted `inflight`, which VETOES the deadlock predicate, so it
/// hung with no fault and no `Err("timeout")` — while `docs/stdlib.md` promised the deadline bounds
/// the whole call. The deadline is now threaded into the demote loop.
#[test]
fn net_read_timeout_bounds_the_in_callback_demote_path() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"\xC3").unwrap(); // a lone lead byte…
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2)); // …then stall, far past the 200 ms budget
    });

    let src = format!(
        "\
import std.net

fn grab(s: Socket) -> str:
    match s.read(8, 200):
        Ok(v): return \"GOT:\" + v
        Err(e): return \"ERR:\" + e.message()

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    outs := [sock].map(grab)
    print(outs[0])
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("demote_read_timeout", &src);
    server.join().unwrap();
    // N3(a) — the demote loop took the lead byte `\xC3` off the wire before timing out, so its
    // timeout reports `Err("incomplete utf-8: …")` (the poll-once classification), NOT `Err("timeout")`
    // which is documented as "nothing arrived". The partial is retained on the socket for a retry.
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "the in-callback demote loop must honour timeout_ms AND classify a taken-partial timeout as \
         incomplete-utf-8, not 'timeout' (nothing arrived): {out:?}"
    );
}

/// B1 — an incomplete codepoint left over when the peer CLOSES (`b"ok\xC3"` ⇒ `error_len() == None`,
/// then EOF) is a real error, never a silent drop and never U+FFFD. The valid prefix is still
/// delivered; the dangling lead byte errors on the read that sees the close.
#[test]
fn net_read_incomplete_tail_at_eof_errs() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"ok\xC3").unwrap();
        stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown the peer");
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    while true:
        match sock.read(8):
            Ok(s):
                if s == \"\":
                    break
                print(\"GOT:\" + s)
            Err(e):
                print(\"ERR:\" + e.message())
                break
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("eof_tail", &src);
    server.join().unwrap();
    assert!(
        out.contains("GOT:ok"),
        "the valid prefix is delivered: {out:?}"
    );
    assert!(
        out.contains("ERR:invalid utf-8 at eof"),
        "a dangling partial codepoint at EOF must error, not be dropped: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #1/#4/#6) — a `read(0)` while a partial codepoint is CARRIED must return `Ok("")` at
/// once, never spin. `read(2)` over `"héllo"` hands back `"h"` and carries the `0xC3` lead byte; a
/// following `read(0)` (the caller-computed `read(want - have)` that lands on 0 is the real-world
/// shape) reads a ZERO-length buffer — which `Read::read` answers `Ok(0)` for unconditionally, so the
/// decode-retry loop could neither progress nor would-block: it spun at 100% CPU forever, un-cancelable
/// and un-timeout-able. The carry must SURVIVE the no-op read (the next `read` still gets `"éllo"`).
#[test]
fn net_read_zero_with_pending_carry_returns_empty_not_spin() {
    let src = "\
import std.net

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    conn.write(\"héllo\")?
    conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    first := sock.read(2)?          # \"h\" — the 0xC3 lead byte of \"é\" is carried
    print(\"FIRST:\" + first)
    zero := sock.read(0)?           # must NOT spin: no bytes wanted, carry untouched
    print(\"ZERO:[\" + zero + \"]\")
    rest := sock.read(64)?          # the carry is still owed — \"éllo\"
    print(\"REST:\" + rest)
    sock.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_zero_carry", src);
    assert!(out.contains("FIRST:h"), "{out:?}");
    assert!(
        out.contains("ZERO:[]"),
        "read(0) is a no-op Ok(\"\"), not a spin and not a false EOF error: {out:?}"
    );
    assert!(
        out.contains("REST:éllo"),
        "the carried lead byte survives the read(0) and is prepended to the next read: {out:?}"
    );
}

/// B1 (review #2/#5/#7) — TWO fibers reading ONE shared `Socket` (an `Arc`'d core — the `spawn
/// handle(conn)` idiom) must decode in WIRE order. The fd read and the carry update are one critical
/// section (carry lock OUTER); when they were split, fiber B could take the continuation bytes off the
/// fd and decode them BEFORE fiber A stored the lead byte it had taken — a leading continuation byte is
/// `error_len() == Some(1)`, so a perfectly valid UTF-8 text stream errored as `"invalid utf-8 on the
/// socket"` (and A's stale carry then poisoned the next read). Both readers poll-once (`read(n, 0)`) so
/// neither ever parks (a second PARKED op on a shared socket is a separate, deliberate fault).
#[test]
fn net_read_shared_socket_two_fibers_decode_in_wire_order() {
    let src = "\
import std.net
import std.concurrency

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    i := 0
    while i < 300:
        conn.write(\"é\")?
        i = i + 1
    conn.close()
    server.close()
    return Ok(0)

fn drain(sock: Socket, seen: Shared[int], bad: Shared[int]) -> int!:
    while seen.get() < 300 and bad.get() == 0:
        match sock.read(3, 0):
            Ok(s):
                got := s.chars().len()
                if got > 0:
                    seen.update(fn(v: int) -> int: v + got)
            Err(e):
                # \"timeout\" (nothing ready) and \"incomplete utf-8\" (this poll took a partial
                # codepoint — retained, the next read finishes it) are both benign poll-once retry
                # signals. \"invalid utf-8\" is THE bug this test guards: valid text mis-decoded
                # because two fibers took bytes out of wire order.
                if e.message().starts_with(\"invalid utf-8\"):
                    print(\"ERR:\" + e.message())
                    bad.update(fn(v: int) -> int: v + 1)
    return Ok(0)

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    seen := Shared(0)
    bad := Shared(0)
    parallel:
        spawn serve(server)
        spawn:
            sock := net.connect(addr)?
            parallel:
                spawn drain(sock, seen, bad)
                spawn drain(sock, seen, bad)
            sock.close()
            return Ok(0)
    print(\"SEEN:\" + str(seen.get()))
    print(\"BAD:\" + str(bad.get()))
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("shared_socket_carry", src);
    assert!(
        !out.contains("invalid utf-8"),
        "valid multibyte text must never error, whichever fiber takes which bytes: {out:?}"
    );
    assert!(
        out.contains("SEEN:300") && out.contains("BAD:0"),
        "every codepoint is delivered exactly once across the two readers: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #3/#8) — `timeout_ms` bounds the WHOLE `read` call, not one park. A park rewinds `ip` and
/// re-executes the op, so the deadline used to be recomputed (`now + timeout_ms`) on every wake: a peer
/// that dribbles ONE byte of a multibyte codepoint per (timeout - ε) kept re-arming the budget and the
/// `read` never timed out. The deadline is now latched on the fiber (`Vm::poll_deadline`), so it fires.
/// Peer: a raw `TcpListener` sending the 4 bytes of `😀` 300 ms apart; `read(64, 400)` must `Err`.
#[test]
fn net_read_timeout_bounds_whole_call_across_codepoint_parks() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for b in "😀".as_bytes() {
            // The client hangs up on the timeout (that IS the fix), so a later byte can hit EPIPE.
            if stream
                .write_all(&[*b])
                .and_then(|()| stream.flush())
                .is_err()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(64, 400):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("read_timeout_midcodepoint", &src);
    server.join().unwrap();
    // N3(a) — the read parked having taken 1-3 bytes of the emoji off the wire, so its latched-deadline
    // timeout classifies as `Err("incomplete utf-8: …")` (the poll-once classification), not the
    // `Err("timeout")` that means "nothing arrived". Still proves the deadline FIRED (both are only
    // reachable on the timeout path) — i.e. the budget did not re-arm at every park.
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "a 400 ms read must time out against a 300 ms-per-byte dribbler (not re-arm its budget at \
         every park) AND classify the taken-partial timeout as incomplete-utf-8: {out:?}"
    );
}

/// N2 (B1 residual) — `write(s, timeout_ms)` honours the deadline when the send buffer is full. The
/// server accepts then never reads, so the client's writes fill the kernel send buffer and the next
/// `write` finds it full, parks on writability, and its deadline fires → `Err("timeout")`. This also
/// exercises the new `poll_deadline` LATCH + `drop_poll_latch` clear on the write path (N2): the
/// deadline is registered through the same fiber latch as `read`, and cleared on completion so the
/// following op gets a fresh budget. (A `write` is architecturally single-park — it returns `Ok(got)`
/// after the first partial write — so the multi-park re-arm the latch guards against is only reachable
/// on a spurious `EPOLLOUT` wake, not deterministically; this test pins the ordinary timeout path.)
#[test]
fn net_write_timeout_when_buffer_full() {
    let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    _ := listener.accept()?
    time.sleep_ms(3000)
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    payload := \"x\".repeat(4000000)
    total := 0
    i := 0
    while i < 200:
        match sock.write(payload, 300):
            Ok(n): total = total + n
            Err(e):
                print(\"ERR:\" + e.message())
                break
        i = i + 1
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("write_timeout_full", src);
    assert!(
        out.contains("ERR:timeout"),
        "a write(_, 300) against a full send buffer must surface Err(\"timeout\"): {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined (no hang): {out:?}"
    );
}

/// N3(a) stale-latch guard — the taken-partial flag (`Vm::poll_partial`) that makes a mid-codepoint
/// timeout report `incomplete utf-8` MUST be cleared on op completion, or it corrupts the NEXT read.
/// First `read(8, 200)` takes the lone lead byte `\xC3` and times out → `Err("incomplete utf-8")`.
/// The lead byte stays carried; a SECOND `read(8, 200)` takes NOTHING new off the wire (the peer is
/// still stalled) and must report a plain `Err("timeout")` — proving `poll_partial` was cleared, not
/// left `Some` from the first read (which would wrongly say "incomplete" again).
#[test]
fn net_read_partial_timeout_then_clean_timeout_is_not_incomplete() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"\xC3").unwrap(); // one lone lead byte…
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2)); // …then stall past both budgets
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(8, 200):
        Ok(s): print(\"A-OK:\" + s)
        Err(e): print(\"A:\" + e.message())
    match sock.read(8, 200):
        Ok(s): print(\"B-OK:\" + s)
        Err(e): print(\"B:\" + e.message())
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error:\" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("partial_then_clean_timeout", &src);
    server.join().unwrap();
    assert!(
        out.contains("A:incomplete utf-8"),
        "the first read took the lead byte then timed out → incomplete utf-8: {out:?}"
    );
    assert!(
        out.contains("B:timeout"),
        "the second read took nothing new → plain timeout (poll_partial was cleared): {out:?}"
    );
    assert!(
        !out.contains("B:incomplete"),
        "a stale poll_partial must not make the second read lie about a partial: {out:?}"
    );
}

/// D6c — `server.accept(timeout_ms)` returns `Err("timeout")` when NO client ever connects, and the
/// program terminates (no hang). The lone acceptor parks on the netpoller with a deadline; the
/// deadline fires, the rewound `accept` returns `Err("timeout")`, and the nursery joins.
#[test]
fn accept_timeout_returns_err() {
    let src = "\
import std.net

fn server(listener: Listener):
    match listener.accept(100):
        Ok(_): print(\"ACCEPTED\")
        Err(e): print(\"ERR:\" + e.message())
    listener.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    parallel:
        spawn server(listener)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("accept_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "accept(100) with no client must surface Err(\"timeout\"): {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued (no hang): {out:?}"
    );
    assert!(!out.contains("ACCEPTED"), "nothing was accepted: {out:?}");
}

/// D6c regression — a `read(n)` with NO timeout still parks FOREVER (until data arrives): the
/// timeout machinery must not have made the untimed read return early. The server sleeps well past
/// any plausible deadline before writing; the client's untimed `read(64)` must wait for the bytes
/// (not time out) and print them.
#[test]
fn read_without_timeout_still_parks_forever() {
    let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(300)
    conn.write(\"late\")?
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_no_timeout", src);
    assert!(
        out.contains("GOT:late"),
        "an untimed read must block until data, not time out: {out:?}"
    );
    assert!(
        !out.contains("ERR:"),
        "no timeout/error on the untimed read: {out:?}"
    );
}

/// D6c — the bundled `examples/socket_timeout.chz` golden: a `--parallel` program that demonstrates
/// both an `accept(timeout_ms)` and a `read(n, timeout_ms)` timeout branch, run end-to-end against
/// its `.expected` output. Net examples need `--parallel` (no fibers to park on the cooperative
/// engine), so — like `echo_server.chz` — this is exercised here rather than in the cooperative
/// golden harness. Watchdog-guarded so a regression faults instead of hanging the test binary.
#[test]
fn example_socket_timeout_matches_expected() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let entry = manifest.join("examples/socket_timeout.chz");
    let expected = std::fs::read_to_string(manifest.join("examples/socket_timeout.expected"))
        .expect("read examples/socket_timeout.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "socket_timeout.chz faulted: {res:?}");
            assert_eq!(
                out, expected,
                "socket_timeout.chz output diverged from its golden"
            );
        }
        Err(_) => panic!("socket_timeout.chz hung — D6c timeout regressed"),
    }
}

/// D6b — the production-ready gate (regression for the documented HANG): a `parallel:` runs a
/// fiber parked on `accept` (no client ever connects, so it parks on the netpoller forever) beside
/// a sibling that faults. Before D6b's `poller::drain_sched`, the faulting sibling tripped cancel
/// but never reached the poller-parked acceptor — its task stayed `inflight`, the fault never
/// propagated, and the nursery wedged. Now the drain re-injects the acceptor, it unwinds on the
/// cancel flag, and the original fault surfaces. Watchdog-guarded: a regression re-hangs here.
#[test]
fn net_faulting_sibling_aborts_accept_parked_peer() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn acceptor(server: Listener) -> int!:
    conn := server.accept()?
    conn.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    parallel:
        spawn acceptor(server)
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling's error must propagate, not hang the nursery");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b — non-blocking `connect` actually parks (and is drainable): a fiber connects to an
/// unroutable TEST-NET-1 address (RFC 5737 `192.0.2.0/24` — the SYN gets no reply, so the
/// non-blocking connect stays `EINPROGRESS` and the fiber parks on writability *indefinitely*),
/// while a sibling faults. A blocking v1 connect would have pinned a worker on the dead handshake;
/// the parked connect must instead be reached by `poller::drain_sched` so the fault propagates and
/// the nursery joins. Deterministic (the address never completes) and watchdog-guarded.
#[test]
fn net_connect_parks_and_is_drained_on_fault() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn dialer() -> int!:
    sock := net.connect("192.0.2.1:9")?
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn dialer()
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling aborts the connect-parked dialer, no hang");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b — the top-level (no-`--parallel`) blocking connect fallback returns a clean `Err` rather
/// than hanging: `net.connect` to a dead loopback port (bound-then-dropped) settles to a refusal
/// through `block_until_connected`. Guards the bounded-spin fix — a regression to an unbounded spin
/// on a non-completing handshake would surface as a watchdog timeout here.
#[test]
fn net_connect_top_level_dead_port_errors_not_hangs() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let src = format!(
        "import std.net\nfn main():\n    match net.connect(\"{dead}\"):\n        Ok(_): print(\"connected\")\n        Err(e): print(\"refused\")\nmain()\n"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let (out, _e, res, _c) = run_file(&entry); // cooperative (no --parallel) ⇒ the blocking fallback
        let _ = tx.send((out, res));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok((out, res)) => {
            res.expect("top-level connect program runs");
            assert_eq!(
                out, "refused\n",
                "dead port ⇒ Err branch (bounded, no hang)"
            );
        }
        Err(_) => {
            panic!("hung: top-level connect to a dead port did not return (unbounded spin?)")
        }
    }
}

/// D6 — the headline netpoller test: an echo server services **far more connections than there are
/// workers**, without a thread per connection. One acceptor fiber + N=100 client fibers run in a
/// single `parallel:` over a core-sized pool (100 ≫ cores). Every client parks on its `read` and
/// the acceptor parks on each `accept`/`read` — on the netpoller, not a pinned worker — so all 100
/// round-trips complete. Without the poller (thread-per-park) the bounded pool would starve and the
/// watchdog would fire. (N stays under the TCP backlog so the v1 *blocking* connect never pins a
/// worker waiting for backlog room — non-blocking connect is deferred to D6b; the per-connection
/// handler runs inline in the acceptor because M:N's fixed task `total` has no spawn-after-join.)
#[test]
fn net_echo_server_services_more_conns_than_workers() {
    let src = r#"import std.net

fn acceptor(server: Listener, n: int) -> int!:
    for _ in 0..n:
        conn := server.accept()?
        msg := conn.read(64)?
        conn.write("echo:" + msg)?
        conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "100-conn echo server must not fault: {res:?}");
    assert!(
        out.contains("all served"),
        "every connection was serviced + the nursery joined: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — the spec's canonical shape: the acceptor `spawn`s a `handle(conn)`
/// fiber PER connection inside its `parallel:` instead of serving inline, and the inner nursery
/// joins them. `#conns ≫ #workers` still completes (handlers multiplex over the core-sized pool).
/// Exercises the eager-nursery + `MnSched::inject` path end-to-end with the bytecode engine.
#[test]
fn net_echo_server_spawns_handler_per_connection() {
    // Nested socket nurseries (an acceptor's `parallel:` servicing outer-sibling clients) need
    // ≥2 hw threads: the inner join blocks the parent's outer worker (decision B), so on a single
    // core the outer clients can't progress to drain the echoes — a pre-existing M:N limit that
    // per-connection spawn is the first to exercise. Skip on 1 core (CI is ≥2 core) rather than hang.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "per-connection-spawn echo server must not fault: {res:?}"
    );
    assert!(
        out.contains("all served"),
        "every connection was handled by its own fiber: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — proves handlers run CONCURRENTLY with accepting (not queued-to-join).
/// A single client opens N connections SEQUENTIALLY: each reply must arrive before the next
/// connect. The acceptor `spawn`s a handler per connection. Under the old queue-at-join model the
/// handler never ran during the accept loop, so the client's first `read` blocked forever and the
/// acceptor's second `accept` had no incoming connection → hang (watchdog fires). The eager
/// inner nursery runs each handler immediately, unblocking the client so the loop advances.
#[test]
fn net_echo_sequential_client_needs_concurrent_handlers() {
    // Eager per-connection spawn requires ≥2 hardware threads (the inner join blocks the parent's
    // sole outer worker on a single core — see `Op::EnterNursery`). This test's whole point is a
    // handler running mid-loop to unblock the next connect, which a 1-core box cannot do; skip it
    // there rather than hang. CI runners are ≥2 core in practice.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str, n: int) -> int!:
    for i in 0..n:
        sock := net.connect(addr)?
        sock.write("ping")?
        reply := sock.read(64)?
        sock.close()
        if reply != "echo:ping":
            return Err("bad reply: " + reply)
    return Ok(0)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        spawn client(addr, n)
    return Ok(0)

fn main():
    match run(8):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "sequential client must complete once handlers run concurrently: {res:?}"
    );
    assert!(
        out.contains("all served"),
        "all 8 sequential round-trips serviced: {out:?}"
    );
    assert!(
        !out.contains("error"),
        "every reply was a correct echo: {out:?}"
    );
}

/// Per-connection spawn — a per-connection HANDLER fault propagates as the acceptor's fault and
/// tears the run down WITHOUT hanging. One injected handler faults (index-out-of-bounds — a real
/// runtime fault, since a spawned task's `Result` *return value* is discarded); the eager inner
/// nursery trips its own cancel (D6b `cancel_drain` + `drain_sched` reach sibling handlers), the
/// join surfaces the fault as the acceptor's body fault, and the OUTER nursery then cancels the
/// clients (so a client stranded without its echo unwinds instead of blocking on `read` forever).
#[test]
fn net_echo_handler_fault_cancels_acceptor() {
    // Nested socket nursery → needs ≥2 hw threads (see `net_echo_server_spawns_handler_per_connection`).
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket, i: int) -> int!:
    msg := conn.read(64)?
    if i == 0:
        conn.close()
        boom := [1]
        return Ok(boom[10])
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for i in 0..n:
            conn := server.accept()?
            spawn handle(conn, i)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    return Ok(1)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(6):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    // The whole point is "no hang": `run_parallel_watchdog` panics if the nursery never
    // terminates. A faulting handler must drive the run to a clean finish (fault surfaced via the
    // acceptor's `match`, or propagated), not deadlock the netpoller-parked siblings.
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_err() || out.contains("error"),
        "a per-connection handler fault must surface (faulted run or reported error), not be swallowed: res={res:?} out={out:?}"
    );
    assert!(
        !out.contains("all served"),
        "the run must not report success once a handler faulted: {out:?}"
    );
}

/// Per-connection spawn — the DEGENERATE eager nursery: a `parallel:` body (entered eagerly under
/// `--parallel` inside a fiber) that injects NOTHING. `activate_eager_nursery` builds a `total==0`
/// sched with `body_open`; `JoinNursery` must `close_body` and have the inline worker terminate
/// immediately (`done==0==total`) and join the drainer — not hang on the empty sched. Pins the
/// `body_open` → `close_body` → terminate handshake on the empty path.
#[test]
fn eager_nursery_with_zero_spawns_completes() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = "fn worker():\n    parallel:\n        print(\"eager body, no spawn\")\n    print(\"worker done\")\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "an empty eager nursery must join cleanly: {res:?}"
    );
    assert!(
        out.contains("eager body, no spawn"),
        "the body ran: {out:?}"
    );
    assert!(
        out.contains("worker done"),
        "the eager nursery joined and the worker continued: {out:?}"
    );
}

/// Per-connection spawn — CONCURRENT eager nurseries (the pool-exhaustion regression): four
/// independent servers each run their OWN eager per-connection-spawn nursery at once. Because each
/// eager nursery drains on a DEDICATED raw OS thread (not the bounded process pool), they do not
/// starve each other — with the earlier pool-farmed design, four long-running eager drainers would
/// exhaust a core-sized pool and hang (undetectably, since `body_open` vetoes the deadlock predicate).
#[test]
fn net_concurrent_eager_servers_do_not_exhaust_pool() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn server_loop(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn pinger(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn one_server(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn server_loop(server, n)
        for _ in 0..n:
            spawn pinger(addr)
    return Ok(0)

fn run(servers: int, conns: int) -> int!:
    parallel:
        for _ in 0..servers:
            spawn one_server(conns)
    return Ok(0)

fn main():
    match run(4, 12):
        Ok(_): print("all servers done")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "concurrent eager servers must not fault: {res:?}"
    );
    assert!(
        out.contains("all servers done"),
        "every concurrent eager nursery completed: {out:?}"
    );
    assert!(!out.contains("error"), "no pinger saw a bad echo: {out:?}");
}

/// B3.5 — a task that finishes normally but strands a `recv`-blocked sibling (it never sent the
/// channel the sibling waits on) is a deadlock. Exercises the `task_finished` `live--` path:
/// dropping the finished task from the live count makes `blocked == live`, so the survivor faults.
#[test]
fn parallel_finished_task_leaves_sibling_deadlocked() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn quick():\n    print(\"quick\")\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn waiter(c)\n        spawn quick()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("a finished sibling that strands a recv-blocked task is a deadlock");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
    assert!(
        out.contains("quick"),
        "the finished task's output is still flushed: {out:?}"
    );
}

/// Run an entry through both engines with a freshly-built [`crate::native::HostConfig`] each
/// (the config isn't `Clone` — `mk_cfg` produces an identical one per engine). Asserts stdout +
/// ok/err parity; returns the agreed stdout.
fn parity_entry_cfg(src: &str, mk_cfg: impl Fn() -> crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    // RAW BYTES on both legs (W6-9b) — same arguments `run_file_parallel`/`run_file_with` pass,
    // minus their lossy decode. `mk_cfg()` still runs ONCE PER ENGINE (fresh stdin queue).
    let (io, ie_out, ir, _ic) = crate::vm::run_file_bytes(&entry, mk_cfg(), true, None, None);
    let (vo, ve_out, vr, _vc) = crate::vm::run_file_bytes(&entry, mk_cfg(), false, None, None);
    assert_stream_parity(&io, &vo, "stdout", "(interp vs vm)");
    assert_stream_parity(&ie_out, &ve_out, "stderr", "(interp vs vm)");
    assert_eq!(
        ir.is_ok(),
        vr.is_ok(),
        "ok/err divergence: interp={ir:?} vm={vr:?}"
    );
    captured(io)
}

/// Like [`parity_entry_cfg`], but for a program whose stdout is a deterministic MULTISET with a
/// nondeterministic ORDER — a shared, consumable stdin read from several tasks: which task gets
/// which line is nondeterministic BY DESIGN (Go/Python), so a byte-equal stdout assert here would be
/// a flake built on purpose. Asserts ok/err parity + that the two engines agree on the line multiset
/// (the existing [`crate::vm::assert_same_lines`]), and returns both engines' stdout.
///
/// A FRESH cfg per engine (`mk_cfg` is called twice): the two engines must not share one `Arc` stdin
/// queue, or the second engine would find it already drained.
fn parity_entry_cfg_lines(
    src: &str,
    mk_cfg: impl Fn() -> crate::native::HostConfig,
) -> (String, String) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mn, mn_err, mr, _mc) = run_file_parallel(&entry, mk_cfg());
    let (se, se_err, sr, _sc) = run_file_with(&entry, mk_cfg());
    assert_eq!(
        mr.is_ok(),
        sr.is_ok(),
        "ok/err divergence: M:N={mr:?} serial={sr:?}"
    );
    assert_eq!(mn_err, se_err, "stderr divergence");
    crate::vm::assert_same_lines(&se, &mn);
    (mn, se)
}

/// Assert stdout is EXACTLY the `want` lines, in ANY order (sorted compare). Do NOT "fix" a caller
/// of this into an `assert_eq!` on the raw stdout: the order is nondeterministic by design.
fn assert_lines_multiset(out: &str, want: &[&str]) {
    let mut got: Vec<&str> = out.lines().collect();
    got.sort_unstable();
    let mut want: Vec<&str> = want.to_vec();
    want.sort_unstable();
    assert_eq!(got, want, "line multiset differs; got:\n{out}");
}

#[test]
fn parity_std_io_print() {
    assert_eq!(
        parity_entry("import std.io\nfn main():\n    io.print(\"hello\")\nmain()"),
        "hello\n"
    );
}

#[test]
fn parity_std_io_read_write_file() {
    let t = TmpDir::new();
    let data = t.0.join("data.txt").display().to_string();
    let src = format!(
        "import std.io\nfn main():\n    match io.write_file(\"{data}\", \"hello\\nworld\"):\n        Ok(_): io.print(\"wrote\")\n        Err(e): io.print(e)\n    match io.read_file(\"{data}\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(e)\nmain()"
    );
    let entry = t.write("main.chz", &src);
    let (io_out, _ie, ir, _) = run_file_p(&entry);
    let (vo, _ve, vr, _) = run_file(&entry);
    assert!(ir.is_ok() && vr.is_ok(), "interp={ir:?} vm={vr:?}");
    assert_eq!(io_out, vo);
    assert_eq!(io_out, "wrote\nhello\nworld\n");
}

#[test]
fn parity_std_io_read_missing_file_errs() {
    // The error text comes from the same `std::fs` call on both engines, so it matches; we only
    // assert the Err branch is taken (deterministic regardless of OS message).
    let src = "import std.io\nfn main():\n    match io.read_file(\"/no/such/chezzi/path/xyz\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(\"err\")\nmain()";
    assert_eq!(parity_entry(src), "err\n");
}

#[test]
#[cfg(target_os = "linux")]
fn read_file_caps_oversized_input() {
    // /dev/zero is unbounded; read_file must return an Err (the size cap), not OOM.
    let src = "import std.io\nfn main():\n    match io.read_file(\"/dev/zero\"):\n        Ok(s): io.print(\"ok\")\n        Err(e): io.print(\"capped\")\nmain()";
    assert_eq!(parity_entry(src), "capped\n");
}

#[test]
fn parity_std_io_read_line_consumes_injected_stdin() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    match io.read_line():\n        Some(l): io.print(\"got {l}\")\n        None: io.print(\"eof\")\n    match io.read_line():\n        Some(l): io.print(l)\n        None: io.print(\"eof\")\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["alpha".to_string()]),
        ..Default::default()
    });
    assert_eq!(out, "got alpha\neof\n");
}

/// `io.read_all()` drains the WHOLE remaining stdin to EOF as one `str` (Python `sys.stdin.read()`).
/// Over the injected `Lines` source it reconstructs each line + `\n` (the injected queue is newline-
/// stripped), so two lines come back as "line0\nline1\n". Single entry task ⇒ deterministic exact
/// assert. Also pins `read_all` is NOT in `is_blocking` (an offloaded call would hit `OffloadHost`'s
/// stdio `unreachable!`).
#[test]
fn parity_std_io_read_all_drains_injected_stdin() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    io.print(io.read_all())\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["héllo".to_string(), "wörld".to_string()]),
        ..Default::default()
    });
    // read_all = "héllo\nwörld\n"; print adds one more \n.
    assert_eq!(out, "héllo\nwörld\n\n");
}

/// `io.read_char()` yields ONE Unicode scalar (a 1-char `str` — Chezzi has no `char` scalar) per call,
/// `None` at EOF. Over injected `Lines` the virtual stream is line0 chars + a reconstructed `\n` +
/// line1 chars… — so "aé" reads as 'a', 'é' (a 2-byte scalar returned WHOLE), then '\n', then `None`.
/// Single entry task ⇒ deterministic exact assert.
#[test]
fn parity_std_io_read_char_yields_scalars_then_eof() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    while true:\n        match io.read_char():\n            Some(c): io.print(\"[{c}]\")\n            None:\n                io.print(\"done\")\n                break\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["aé".to_string()]),
        ..Default::default()
    });
    // 'a', 'é', then the reconstructed '\n' (prints `[`, newline, `]`), then None → done.
    assert_eq!(out, "[a]\n[é]\n[\n]\ndone\n");
}

/// `io.input(prompt)` = print the prompt (no newline) + flush + `read_line`. Under the BUFFERED sink
/// (every test helper + embedder) `flush` is a no-op and the prompt simply lands in the captured
/// `out` — both engines identical. Also pins that neither fn is in `is_blocking` (an offloaded call
/// would hit `OffloadHost`'s stdio `unreachable!`).
#[test]
fn parity_std_io_input_prompt_then_line_and_flush_is_a_noop() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    io.flush()\n    match io.input(\"p: \"):\n        Some(l): io.print(\"got {l}\")\n        None: io.print(\"eof\")\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["ada".to_string()]),
        ..Default::default()
    });
    assert_eq!(out, "p: got ada\n");
}

/// stdin is ONE shared, consumable source that EVERY task reads (Go's `os.Stdin` / Python's
/// `sys.stdin`), at the `spawn:`/nursery task-entry path: 3 lines, 2 spawned readers + the entry
/// reader ⇒ each line is read exactly ONCE, by SOME reader, and nobody sees a false EOF.
///
/// Every reader prints the same `got {v}` tag, so the line MULTISET is assignment-independent —
/// which is the point: WHICH task gets a given line is nondeterministic BY DESIGN (both engines).
/// Do NOT "fix" this back to an exact-stdout `assert_eq!`; that is a designed flake. A false EOF
/// would add an `eof` line; a duplicated line would repeat a `got` line. Both change the multiset.
#[test]
fn parity_spawned_tasks_share_stdin_exactly_once() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn t():\n    match io.read_line():\n        Some(v): io.print(\"got {v}\")\n        None: io.print(\"eof\")\nfn main():\n    parallel:\n        spawn: t()\n        spawn: t()\n    t()\nmain()";
    let (mn, se) = parity_entry_cfg_lines(src, || HostConfig {
        stdin: Stdin::lines(["a".to_string(), "b".to_string(), "c".to_string()]),
        ..Default::default()
    });
    for out in [&mn, &se] {
        assert_lines_multiset(out, &["got a", "got b", "got c"]);
    }
}

/// Same shared-stdin contract at the OTHER task-entry family: `Executor.submit` (the cooperative
/// inline drain AND the M:N pool drain). An invariant enforced at one seam is not enforced.
#[test]
fn parity_executor_tasks_share_stdin_exactly_once() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nimport std.concurrency\nfn t():\n    match io.read_line():\n        Some(v): io.print(\"got {v}\")\n        None: io.print(\"eof\")\nfn main():\n    e := Executor()\n    e.submit(t)\n    e.submit(t)\n    e.shutdown()\n    t()\nmain()";
    let (mn, se) = parity_entry_cfg_lines(src, || HostConfig {
        stdin: Stdin::lines(["a".to_string(), "b".to_string(), "c".to_string()]),
        ..Default::default()
    });
    for out in [&mn, &se] {
        assert_lines_multiset(out, &["got a", "got b", "got c"]);
    }
}

#[test]
fn parity_std_io_eprint_goes_to_stderr_not_stdout() {
    let src = "import std.io\nfn main():\n    io.eprint(\"to stderr\")\n    io.print(\"to stdout\")\nmain()";
    // Parity (both engines): stdout has only the print line, stderr has only the eprint line.
    assert_eq!(parity_entry(src), "to stdout\n");
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, err, res, _) = run_file(&entry);
    assert!(res.is_ok());
    assert_eq!(out, "to stdout\n");
    assert_eq!(err, "to stderr\n");
}

#[test]
fn parity_std_log_defaults_to_stderr() {
    // std.log: default min level INFO → debug() dropped, info/warn land on STDERR (not stdout),
    // formatted "LEVEL message" in order. Both engines identical.
    let src = "import std.log\nfn main():\n    lg := log.new()\n    lg.info(\"served\")\n    lg.debug(\"noisy\")\n    lg.warn(\"careful\")\nmain()";
    assert_eq!(parity_entry(src), ""); // nothing on stdout
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, err, res, _) = run_file(&entry);
    assert!(res.is_ok());
    assert_eq!(out, "");
    assert_eq!(err, "INFO served\nWARN careful\n");
}

#[test]
fn parity_std_os_args_and_env() {
    use crate::native::HostConfig;
    let src = "import std.io\nimport std.os\nfn main():\n    for a in os.args():\n        io.print(a)\n    match os.env(\"CHEZZI_TEST_VAR\"):\n        Some(v): io.print(v)\n        None: io.print(\"no var\")\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        args: vec!["x".to_string(), "y".to_string()],
        env: std::sync::Arc::new(std::sync::Mutex::new(
            [("CHEZZI_TEST_VAR".to_string(), "hi".to_string())]
                .into_iter()
                .collect(),
        )),
        ..Default::default()
    });
    assert_eq!(out, "x\ny\nhi\n");
}

#[test]
fn parity_std_os_env_missing_is_none() {
    use crate::native::HostConfig;
    let src = "import std.io\nimport std.os\nfn main():\n    match os.env(\"DEFINITELY_UNSET_XYZ\"):\n        Some(v): io.print(v)\n        None: io.print(\"none\")\nmain()";
    let out = parity_entry_cfg(src, HostConfig::default);
    assert_eq!(out, "none\n");
}

#[test]
fn parity_std_os_getcwd_ok() {
    let src = "import std.io\nimport std.os\nfn main():\n    match os.getcwd():\n        Ok(p): io.print(\"ok\")\n        Err(e): io.print(\"err\")\nmain()";
    assert_eq!(parity_entry(src), "ok\n");
}

/// Run a single-file (importing std) program on the VM with GC stress on (collect before every
/// instruction) and the given config — surfaces any native-return value the collector might free
/// while still reachable.
fn vm_run_file_stress(src: &str, cfg: crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.host = cfg;
    vm.run()
        .unwrap_or_else(|e| panic!("unexpected error under GC stress: {e}"));
    captured(vm.out)
}

/// Task 1 — the SERIAL `Executor` PROGRAM-EXIT drain (`drain_live_executors`) runs each queued job
/// via `with_serial_child_modules` against EMPTY frames (it runs AFTER `run()` popped the top-level
/// frame). This exercises that path under `gc_stress` (collect before every instruction): both
/// un-`shutdown()`'d jobs mutate a module-global aggregate (forcing the snapshot path) and allocate,
/// and the drain must complete with no error and full isolation (the parent's `xs` untouched).
///
/// Defense-in-depth note on `pinned_module_roots`: on this empty-frames path the swapped-out shell
/// `module_objs` are otherwise unrooted, so a mid-job collection frees them and the restore reinstalls
/// dangling `GcRef`s. The pin keeps the invariant "`module_objs` is always valid". This test does NOT
/// distinguish the pin (it still passes with the pin's root scan removed) because normal post-exit-
/// drain flow never DEREFERENCES those refs — every downstream read uses the memoized, heap-
/// independent snapshot, not `self.module_objs`. The pin is retained to close the latent hazard
/// (a future caller that touches module globals after an empty-frames drain would UAF), matching the
/// adversarial-review finding; it is not claimed to be caught by an assertion here.
#[test]
fn serial_executor_exit_drain_module_globals_survive_gc_stress() {
    let src = "\
import std.concurrency
xs := [1, 2, 3]
fn worker():
    xs.push(99)
    junk := []
    for i in range(30):
        junk.push([str(i), str(i + 1)])
fn main():
    ex := concurrency.Executor()
    ex.submit(worker)
    ex.submit(worker)
main()";
    // No `shutdown()` → both jobs run at the EMPTY-frames program-exit drain. Each mutates its
    // private copy (isolated); the parent's `xs` is untouched. The point is it does not UAF/abort.
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.host = crate::native::HostConfig::default();
    vm.run().unwrap();
    vm.drain_live_executors(Span { line: 1, col: 1 }).unwrap();
    assert_eq!(vm.out, b"");
}

/// Bug 3 — a module-qualified generic fn (`geo.empty_list[int]()`) type-checks AND runs on both
/// engines (type args are erased, so this exercises the checker fix end-to-end).
#[test]
fn parity_qualified_generic_fn_turbofish() {
    let geo = ("geo.chz", "fn empty_list[T]() -> List[T]:\n    return []\n");
    let main = (
        "main.chz",
        "import geo\nfn main():\n    xs := geo.empty_list[int]()\n    xs.push(1)\n    print(xs)\n    ys: List[str] = geo.empty_list()\n    print(ys)\nmain()",
    );
    assert_eq!(assert_parity_file(&[geo, main], "main.chz"), "[1]\n[]\n");
}

/// A from-imported global RE-DECLARED at module scope (`:=`) is the module's OWN binding — assigning
/// it is legal and runs on both engines (the rebind gate must not fire on a name the import no longer
/// owns).
#[test]
fn parity_from_import_then_module_scope_redeclare() {
    let st = ("st.chz", "COUNT := 0\n");
    let main = (
        "main.chz",
        "import COUNT from st\nCOUNT := COUNT + 1\nCOUNT = COUNT + 1\nfn main():\n    print(COUNT)\nmain()",
    );
    assert_eq!(assert_parity_file(&[st, main], "main.chz"), "2\n");
}

/// Bug 4 acceptance: the std string module is `std.string`, so importing it UN-aliased does not
/// bind the reserved name `str` — the global `str()` ctor and the qualified module fn both work in
/// the SAME module.
#[test]
fn parity_import_std_string_and_str_ctor() {
    let src = "import std.string
fn main():
    print(str(5))
    print(string.pad_left(\"a\", 3, \"-\"))\nmain()";
    assert_eq!(parity_entry(src), "5\n--a\n");
}

#[test]
fn parity_std_str_pure_chezzi_with_mixed_native_import() {
    // std.string is a real Chezzi file (crate/std/string.chz); std.io is native — both in one program.
    let src = "import std.io\nimport std.string as text\nfn main():\n    io.print(text.repeat(\"ab\", 3))\n    io.print(text.reverse(\"hello\"))\n    io.print(text.pad_left(\"7\", 3, \"0\"))\n    if text.is_empty(\"\"):\n        io.print(\"empty\")\n    for line in text.split_lines(\"a\\nb\\nc\"):\n        io.print(line)\nmain()";
    assert_eq!(parity_entry(src), "ababab\nolleh\n007\nempty\na\nb\nc\n");
}

/// The pure-Chezzi `std.string` free fn is a byte-identical alias of the native `pad_left` METHOD:
/// same never-shrinks rule, and the same truncated-cycle rule for a multi-char fill (it used to
/// overshoot `width`). Codepoints, not bytes — a non-ASCII fill char counts as 1.
#[test]
fn parity_std_str_pad_left_matches_native_method() {
    for (args, want) in [
        (r#""a", 4, "xy""#, "xyxa"),
        (r#""ab", 7, "xyz""#, "xyzxyab"),
        (r#""7", 3, "0""#, "007"),
        (r#""12345", 3, "0""#, "12345"),
        (r#""a", -5, "0""#, "a"),
        // `width = i64::MIN`: the free fn must return `s` unchanged like the native method — no
        // `integer overflow in Sub` fault (Chezzi `-` is checked), no divergence from the alias.
        (r#""ab", -9223372036854775808, "x""#, "ab"),
        (r#""é", 3, "ü""#, "üüé"),
        (r#""a", 4, "日本""#, "日本日a"),
    ] {
        let src = format!(
            "import std.string as text\nfn main():\n    print(text.pad_left({args}))\nmain()"
        );
        assert_eq!(
            parity_entry(&src),
            format!("{want}\n"),
            "free fn mismatch for `{args}`"
        );
    }
}

/// An empty `fill` LIVELOCKED the free fn's prepend loop (zero output, no diagnostic). It must now
/// fault — with the same message as the native method — on both engines, EAGERLY: a `width` the
/// receiver already satisfies does not excuse it.
#[test]
fn parity_std_str_pad_left_empty_fill_faults() {
    for args in [r#""a", 5, """#, r#""abc", 1, """#] {
        let src = format!(
            "import std.string as text\nfn main():\n    print(text.pad_left({args}))\nmain()"
        );
        let msg = parity_entry_fault(&src);
        assert!(
            msg.contains("pad_left: fill must not be empty"),
            "unexpected fault for `{args}`: {msg}"
        );
    }
}

#[test]
fn native_returned_heap_values_survive_gc_stress() {
    use crate::native::HostConfig;
    // Each os.args() call allocates a fresh heap list (immediately garbage); under stress the
    // collector runs every instruction. A dangling handle in native lowering would panic here.
    let src = "import std.io\nimport std.os\nfn main():\n    n := 0\n    while n < 300:\n        xs := os.args()\n        n += 1\n    io.print(\"done {n}\")\nmain()";
    let cfg = HostConfig {
        args: vec!["a".to_string()],
        ..Default::default()
    };
    let out = vm_run_file_stress(src, cfg);
    assert_eq!(out, "done 300\n");
}

/// A spread of programs exercising every feature class — run through BOTH engines.
const PROGRAMS: &[&str] = &[
    // arithmetic + promotion + truncation
    "print(7 / 2)\nprint(1 + 2.0)\nprint(2.5 * 2.0)\nprint(10 % 3)",
    // string concat + interpolation + escapes
    "fn main():\n    n := \"x\"\n    print(\"a{n}b {1 + 2} {{lit}}\")\nmain()",
    // comparison + equality + bool logic
    "print(1 < 2)\nprint(2 == 2.0)\nprint(true and false)\nprint(false or true)\nprint(not true)",
    // lists, indexing, len
    "print([1, 2, 3])\nprint([10, 20, 30][2])\nprint([1, 2].len())",
    // structs + methods
    "struct P:\n    x: int\n    y: int\n    fn sum(self) -> int:\n        return self.x + self.y\nfn main():\n    p := P(3, 4)\n    print(p)\n    print(p.sum())\nmain()",
    // enums + match + payload binding
    "enum S:\n    C(int)\n    Sq(int)\nfn a(s: S) -> int:\n    match s:\n        S.C(r): return r * r\n        S.Sq(n): return n * n\nfn main():\n    print(a(S.C(3)))\n    print(a(S.Sq(4)))\nmain()",
    // generic enum (type-erased): same enum at two element types + match payload substitution
    "enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\nfn sum(t: Tree[int]) -> int:\n    match t:\n        Tree.Leaf: return 0\n        Tree.Node(v, l, r): return sum(l) + v + sum(r)\nfn main():\n    t: Tree[int] = Tree.Node(2, Tree.Node(1, Tree.Leaf, Tree.Leaf), Tree.Node(3, Tree.Leaf, Tree.Leaf))\n    print(sum(t))\nmain()",
    // closures
    "fn adder(n: int):\n    return fn(x: int) -> int: x + n\nfn main():\n    f := adder(10)\n    print(f(5))\nmain()",
    // ? operator (Ok + Err propagation)
    "fn d(a: int, b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(a / b)\nfn use() -> Result[int]:\n    r := d(10, 0)?\n    return Ok(r)\nfn main():\n    match use():\n        Ok(v): print(v)\n        Err(e): print(e)\nmain()",
    // for + while loops
    "fn main():\n    t := 0\n    for i in 0..100:\n        t += i\n    print(t)\n    n := 5\n    while n > 0:\n        n -= 1\n    print(n)\nmain()",
    // builtins
    "print(range(4))\nprint(int(\"7\") + 1)\nprint(float(3))\nprint([1, 2, 3].len())\nprint(str(42))",
    // recursion
    "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nfn main():\n    print(fib(15))\nmain()",
    // inferred return type (no `-> T`): runtime is unaffected, both engines agree
    "fn add(a: int, b: int):\n    return a + b\nfn classify(n: int):\n    if n == 0:\n        return Some(0)\n    return None\nfn main():\n    print(add(2, 3))\n    match classify(0):\n        Some(v): print(v)\n        None: print(\"none\")\nmain()",
    // expression-valued match (multiline) + if (inline): both engines must agree on the value
    "fn lookup(k: int) -> int?:\n    if k == 0:\n        return None\n    return Some(k)\nfn main():\n    found := match lookup(7):\n        Some(v): v\n        None: -1\n    print(found)\n    sign := if found > 0: \"pos\" else: \"neg\"\n    print(sign)\n    none := match lookup(0):\n        Some(v): v\n        None: -1\n    print(none)\nmain()",
    // ----- M6: core-type methods (str) -----
    "print(\"abcd\".len())\nprint(\"Hi There\".upper())\nprint(\"Hi There\".lower())\nprint(\"  pad  \".trim())",
    // str conforms to Error: message() returns the string itself
    "print(\"boom\".message())",
    // Go-style Result[T, E]: custom struct error (T!E), match, message() dispatch
    "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db {self.code}\"\nfn q(ok: bool) -> int!DbErr:\n    if ok:\n        return Ok(1)\n    return Err(DbErr(503))\nfn main():\n    match q(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n    match q(true):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    // default-Error path: Err(str) flows as Result[int, Error], consumed via message()
    "fn parse(ok: bool) -> int!:\n    if ok:\n        return Ok(42)\n    return Err(\"bad input\")\nfn main():\n    match parse(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    // ----- M11 Phase B: recover boundary -----
    // recover catches index-OOB; Ok path wraps the trailing value
    "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()",
    // recover catches divide-by-zero
    "fn main():\n    r := recover:\n        10 / 0\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"err: {e.message()}\")\nmain()",
    // recover catches integer overflow
    "fn main():\n    r := recover:\n        9223372036854775807 * 2\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"ovf\")\nmain()",
    // recover ok-path wraps the value
    "fn main():\n    r := recover:\n        2 + 3\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err\")\nmain()",
    // a fault three calls deep is caught at the boundary (no per-call wrapping)
    "fn a() -> int:\n    return b()\nfn b() -> int:\n    return c()\nfn c() -> int:\n    return [1][9]\nfn main():\n    r := recover:\n        a()\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"deep recovered\")\nmain()",
    // `?` inside recover short-circuits to `r` (try-block): the Err lands in `r`, and code
    // AFTER the recover still runs — the enclosing fn returns a plain str, so this only works
    // if `?` did NOT exit the function.
    "fn d(b: int) -> int!:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(10 / b)\nfn use() -> str:\n    r := recover:\n        x := d(0)?\n        x + 1\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return \"caught {e.message()}\"\nfn main():\n    print(use())\nmain()",
    // `?` Ok path inside recover: value unwrapped, trailing expression becomes the Ok result
    "fn d(b: int) -> int!:\n    return Ok(10 / b)\nfn main():\n    r := recover:\n        x := d(2)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(e.message())\nmain()",
    // side effects before a caught fault PERSIST (keep semantics) — both engines must agree
    "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered\")\n    print(\"x={x}\")\nmain()",
    // nested recover: the inner boundary catches, the outer sees a normal value
    "fn main():\n    r := recover:\n        inner := recover:\n            [1][9]\n        match inner:\n            Ok(v): v\n            Err(e): 0\n    match r:\n        Ok(v): print(\"outer ok {v}\")\n        Err(e): print(\"outer err\")\nmain()",
    // recovered value composes with `?` after the boundary\n
    "fn run() -> int!:\n    r := recover:\n        [10, 20][0]\n    v := r?\n    return Ok(v + 1)\nfn main():\n    match run():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    "print(\"a,b,c\".split(\",\"))\nprint(\",\".join([\"a\", \"b\", \"c\"]))",
    "print(\"abc\".starts_with(\"ab\"))\nprint(\"abc\".starts_with(\"z\"))\nprint(\"abc\".contains(\"b\"))\nprint(\"abc\".contains(\"q\"))",
    // chained core-type methods
    "print(\"  Hello,World  \".trim().lower().split(\",\"))",
    // ----- M6: core-type methods (list) -----
    "fn main():\n    xs := [1, 2]\n    xs.push(3)\n    xs.push(4)\n    print(xs)\n    print(xs.len())\nmain()",
    // ----- M6: pipe operator -----
    "fn inc(n: int) -> int: n + 1\nfn dbl(n: int) -> int: n * 2\nfn main():\n    print(5 |> inc() |> dbl())\nmain()",
    "fn shout(s: str) -> str: s.upper()\nfn main():\n    print(\"hi\" |> shout())\nmain()",
    // ----- error parity -----
    "print(1 / 0)",
    "print([1, 2][9])",
    "print(1 + \"x\")",
    "fn loop(n: int) -> int:\n    return loop(n + 1)\nfn main():\n    print(loop(0))\nmain()",
    // M6 method error parity
    "print(\"hi\".upper(\"extra\"))",
    "print(\"hi\".frobnicate())",
    "print(\",\".join([1, 2]))",
    "print((5).upper())",
    // arg-eval order: a bad method/receiver with an erroring arg must report the SAME error on
    // both engines — the VM evaluates args (operands) before the call, so the interp must too.
    "print((5).frob(1 / 0))",
    "print(\"hi\".frob(1 / 0))",
    // ----- entry model: no auto-main; unhandled top-level Err/None exits -----
    "fn main():\n    print(\"hi\")", // main defined but never called → no output
    "Err(\"boom\")",                 // bare top-level Err → unhandled error
    "x := Err(\"oops\")?",           // top-level `?` Err → unhandled error
    "fn g() -> Option[int]:\n    return None\ng()", // bare None → unhandled error
    "fn f() -> Result[int]:\n    return Err(\"x\")\nr := f()\nprint(\"handled\")", // Err bound = handled → no exit
    "fn main():\n    print(\"before\")\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()", // partial output then exit
    // a user enum shadowing `Err` is a normal value: bare one must NOT exit, `?` must reject it
    "enum Signal:\n    Err(int)\n    Quiet\nErr(5)\nprint(\"made it\")",
    "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Err(5)?\n    return x\nf()",
    // unhandled top-level error INSIDE a top-level block (interp: call_depth 0, VM: is_toplevel)
    "if true:\n    Err(\"boom\")\nprint(\"after\")", // bare Err in `if` → exit, no "after"
    "for i in 0..1:\n    Err(\"x\")\nprint(\"after\")", // bare Err in `for` → exit
    "fn d() -> Result[int]:\n    return Err(\"z\")\nif true:\n    x := d()?\n    print(x)", // top-level `?` in block → exit (same span both engines)
];

#[test]
fn parity_full_suite_vm_vs_interp() {
    for src in PROGRAMS {
        assert_parity(src);
    }
}

// A manifest `module:function` entrypoint whose fn returns `Err(..)` must surface it as an unhandled
// runtime error (rc=1), symmetric with the unhandled-top-level-Err rule — not silently discard the
// return value. Covers BOTH engines (they route through `run_file_inner`→`invoke_entrypoint`).
// (2026-07-18 bug-hunt: `?` in a nil entry fn used to swallow the error; the fix lets the entry be
// `-> T!` and use `?`, and this surfaces a returned Err.)
#[test]
fn manifest_entrypoint_err_surfaced_both_engines() {
    let dir = TmpDir::new();
    let entry = dir.write("main.chz", "fn main() -> int!:\n    return Err(\"boom\")\n");
    for parallel in [false, true] {
        let (_out, _err, outcome, _rc) = crate::vm::run_file_with_entry(
            &entry,
            crate::native::HostConfig::default(),
            parallel,
            Some("main"),
            None,
        );
        let e = outcome.expect_err(&format!(
            "entry fn returning Err must surface a runtime error (parallel={parallel})"
        ));
        assert!(
            e.message.contains("unhandled error: boom"),
            "expected 'unhandled error: boom', got {:?} (parallel={parallel})",
            e.message
        );
    }
}

// GUARD: an entry fn returning `Ok(..)` runs clean (rc=0) — the surfacing gate is Err/None only.
#[test]
fn manifest_entrypoint_ok_runs_clean_both_engines() {
    let dir = TmpDir::new();
    let entry = dir.write(
        "main.chz",
        "fn main() -> int!:\n    print(\"ran\")\n    return Ok(0)\n",
    );
    for parallel in [false, true] {
        let (out, _err, outcome, _rc) = crate::vm::run_file_with_entry(
            &entry,
            crate::native::HostConfig::default(),
            parallel,
            Some("main"),
            None,
        );
        assert!(
            outcome.is_ok(),
            "entry fn returning Ok must run clean (parallel={parallel}), got {outcome:?}"
        );
        assert!(
            out.contains("ran"),
            "expected 'ran' in output (parallel={parallel})"
        );
    }
}

#[test]
fn parity_index_assign() {
    assert_parity("xs := [1, 2, 3]\nxs[1] = 9\nxs[0] += 4\nxs[2] -= 1\nprint(xs)\n");
}

#[test]
fn parity_index_assign_out_of_bounds() {
    assert_parity("xs := [1, 2, 3]\nxs[9] = 0\nprint(xs)\n");
}

#[test]
fn parity_compound_index_oob_vs_rhs_error_order() {
    // Compound `xs[i] += rhs` on an out-of-bounds `i` where `rhs` ALSO errors: both engines
    // must agree on which error wins. The VM reads the target (bounds-check) before `rhs`;
    // the interp must do the same.
    assert_parity("xs := [1, 2, 3]\nz := 0\nxs[5] += 1 / z\n");
}

#[test]
fn parity_compound_index_oob_skips_rhs_side_effect() {
    // On an out-of-bounds compound assign, neither engine should run the rhs side effect.
    assert_parity(
        "fn side() -> int:\n    print(\"rhs ran\")\n    return 0\nxs := [1, 2, 3]\nxs[5] += side()\nprint(\"after\")\n",
    );
}

#[test]
fn parity_field_assign() {
    assert_parity(
        "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\np.y += 3\nprint(p.x)\nprint(p.y)\n",
    );
}

// NOTE: method_type_params / param_protocol / method_default_args were parity-only; they are
// now full golden tests (`golden_*_chz_matches_expected_and_interp` above), which assert exact
// output AND cross-engine parity, so the weaker `parity_*` file tests were removed.

#[test]
fn parity_hof_param() {
    let src = "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nprint(apply(inc, 4))\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "5\n");
}

/// Integer `sum()` must use checked arithmetic and raise the SAME recoverable
/// "integer overflow in Add" runtime error as `+`, not silently wrap. Runs both engines.
#[test]
fn parity_list_sum_overflow() {
    let src = "print([9223372036854775807, 1].sum())\n";
    assert_parity(src);
    let err = vm_outcome(src).expect_err("expected a runtime error, not wraparound");
    assert!(
        err.contains("integer overflow in Add"),
        "unexpected error: {err}"
    );
}

/// A list containing any float takes the float path — would-overflow ints must NOT fault and
/// both engines must agree (guards against the int checked_add being hoisted above any_float).
#[test]
fn parity_list_sum_mixed_float() {
    let src = "print([9223372036854775807, 1, 0.0].sum())\n";
    assert_parity(src);
}

// ===== higher-order list methods: map / filter / fold =====
//
// These call a closure per element. On the VM each closure runs nested frames that can GC at
// instruction boundaries, so the source/result lists (and fold's accumulator) must stay rooted.
// Several tests use HEAP elements (strings / nested lists) and run under `gc_stress` so that a
// collection actually happens mid-iteration — if rooting is wrong they crash with a dangling ref.

#[test]
fn parity_list_map_to_str_gc_stress() {
    // Each element maps to a freshly-allocated string (heap), so collection mid-map matters.
    let src =
        "xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n";
    assert_parity(src);
    let expected = "n1\nn2\nn3\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_map_to_nested_list_gc_stress() {
    // Maps each element to a nested list (heap); the result list holds heap children.
    let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> List[int]: [x, x])\nprint(ys[1][0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
    assert_eq!(
        run_capture_stress(src),
        "2\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_filter_gc_stress() {
    // Filter over string elements; kept elements are heap objects pushed into the result.
    let src = "xs := [\"a\",\"bb\",\"ccc\",\"d\"]\nys := xs.filter(fn(x: str) -> bool: x.len() > 1)\nprint(ys.len())\nprint(ys[0])\n";
    assert_parity(src);
    let expected = "2\nbb\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_fold_str_acc_gc_stress() {
    // Fold building a string accumulator (heap) — each step allocates a new acc string, so the
    // rooted accumulator slot must survive the next element's closure call.
    let src = "xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "abc\n");
    assert_eq!(
        run_capture_stress(src),
        "abc\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_sort_by_str_gc_stress() {
    // Sort heap-string elements by length; the comparator re-enters the VM and a collection can
    // fire mid-sort. The source list must stay rooted (we permute indices, not raw Values).
    let src = "xs := [\"ccc\",\"a\",\"dd\",\"b\"]\nxs.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nfor x in xs:\n    print(x)\n";
    assert_parity(src);
    let expected = "a\nb\ndd\nccc\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_sort_by_nested_list_gc_stress() {
    // Elements are nested lists (heap); sort by first element. Exercises rooting of heap children
    // across comparator calls under stress.
    let src = "xs := [[3,0],[1,0],[2,0]]\nxs.sort_by(fn(a: List[int], b: List[int]) -> int: a[0] - b[0])\nprint(xs[0][0])\nprint(xs[2][0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
    assert_eq!(
        run_capture_stress(src),
        "1\n3\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_map_closure_free_generic_call() {
    // Bug D: `xs.map(fn(x): ident(x))` where `ident[T](x: T) -> T` type-checks to List[int] (the
    // closure-return loop-back recovers `map`'s `U` from the nested free generic call). Runtime is
    // generic-erased, so both engines print the same `2`.
    let src = "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): ident(x))\n    print(ys[0] + 1)\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
}

#[test]
fn parity_fold_closure_free_generic_call() {
    // Bug D (adversarial-review fix): `xs.fold(0, fn(acc, x): ident(x))` where `ident[T](x: T) -> T`
    // type-checks (fold's `U` pinned `int` by `init`, closure body re-inferred `int`). The reducer
    // returns the last element via identity → `s = 3`, `print(s + 1)` = `4`. Generic-erased runtime,
    // so both engines print the same.
    let src = "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(x))\n    print(s + 1)\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "4\n");
}

#[test]
fn parity_free_fn_hof_map() {
    // Bug D free-fn analog: `mymap([1,2,3], fn(x): x*2)` where `mymap[U](..., fn(int)->U) -> List[U]`
    // type-checks to List[int] (the closure-return loop-back now runs on the free-fn path too). Runtime
    // is generic-erased, so both engines print the same `3` (ys=[2,4,6], ys[0]+1).
    let src = "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): x * 2)\n    print(ys[0] + 1)\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "3\n");
}

#[test]
fn parity_free_fn_hof_apply_sibling() {
    // `apply[A,B](f: fn(A)->B, a: A) -> B` with `apply(fn(x): x*2, 5)`: A pinned int by the sibling
    // value arg, B (return-only) recovered from the closure body → int, so `y + 1` = 11. Both engines.
    let src = "fn apply[A, B](f: fn(A) -> B, a: A) -> B:\n    return f(a)\nfn main():\n    y := apply(fn(x): x * 2, 5)\n    print(y + 1)\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "11\n");
}

#[test]
fn parity_free_fn_hof_sibling_closure_param() {
    // adversarial-review bug 1: `pair[T](f: fn()->T, g: fn(T)->int) -> int` with
    // `pair(fn(): 5, fn(x): x + 1)`. `T` is recovered from the FIRST closure's concrete return (int)
    // before the un-inferable-param probe runs, so the SECOND closure's `x: T` param is not rejected.
    // Runtime is generic-erased → both engines print `6`.
    let src = "fn pair[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f())\nfn main():\n    print(pair(fn(): 5, fn(x): x + 1))\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "6\n");
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Run a file through both engines and assert identical (stdout, error).
fn assert_file_parity(rel: &str) {
    let path = fixture(rel);
    // RAW BYTES on both legs (see `RunOutputRaw`): a `String` compare would fold a genuine
    // non-UTF-8 divergence (`ff` vs `fe`) into equal U+FFFDs and pass a byte-divergent run.
    let raw = |parallel| {
        crate::vm::run_file_bytes(
            &path,
            crate::native::HostConfig::default(),
            parallel,
            None,
            None,
        )
    };
    let (vm_out, vm_err, vm_res, _) = raw(false);
    let (ip_out, ip_err, ip_res, _) = raw(true);
    // Text compare first (readable failure), then the byte compare that catches a divergence a
    // lossy decode would erase.
    let label = format!("for {rel}");
    assert_stream_parity(&vm_out, &ip_out, "stdout", &label);
    assert_stream_parity(&vm_err, &ip_err, "stderr", &label);
    assert_eq!(
        vm_res.err().map(|e| e.to_string()),
        ip_res.err().map(|e| e.to_string()),
        "error divergence for {rel}"
    );
}

#[test]
fn golden_hello_via_run_file() {
    let path = fixture("examples/hello.chz");
    let expected = std::fs::read_to_string(fixture("examples/hello.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok());
    assert_eq!(out, expected);
}

/// `examples/concurrent_jobs.chz` — the self-verifying concurrency stress test that deliberately
/// touches EVERY concurrency primitive (Channel incl. bounded-cap/try_send backpressure, wait:,
/// parallel: nursery, spawn, Executor incl. submit_result, Shared/RwShared/Atomic, concurrent
/// collections, cancel, timer, and the airlock). It is deterministic by construction, so it is two
/// tests at once: the 49 self-checks must all PASS (golden), and serial-VM must byte-match M:N.
#[test]
fn golden_concurrent_jobs_both_engines() {
    let path = fixture("examples/concurrent_jobs.chz");
    let expected = std::fs::read_to_string(fixture("examples/concurrent_jobs.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert!(
        out.contains("ALL PASS (49 checks)"),
        "self-test must fully pass:\n{out}"
    );
    assert_file_parity("examples/concurrent_jobs.chz");
}

/// M6 golden: core-type methods + pipe run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_methods_via_run_file() {
    let path = fixture("examples/methods.chz");
    let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/methods.chz");
}

/// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_mutate_via_run_file() {
    let path = fixture("examples/mutate.chz");
    let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/mutate.chz");
}

/// `examples/collection_ops.chz` — golden coverage of every collection operator (gap #3): list
/// `+`/`*` and set `| & - ^` (multi-element, empty, type-correct mixes). Uses `std.io`, so it
/// runs through `run_file` (module resolution) and asserts two-engine parity via
/// `assert_file_parity` (VM==interp==expected).
#[test]
fn golden_collection_ops_via_run_file() {
    let path = fixture("examples/collection_ops.chz");
    let expected = std::fs::read_to_string(fixture("examples/collection_ops.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/collection_ops.chz");
}

/// Golden: `Self` usable in inherent struct/enum/newtype method signatures + bodies (not
/// protocols-only). Runs end-to-end on the VM and byte-matches both the `.expected` file and the
/// M:N engine (parity via `assert_file_parity`).
#[test]
fn golden_self_method_via_run_file() {
    let path = fixture("examples/self_method.chz");
    let expected = std::fs::read_to_string(fixture("examples/self_method.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/self_method.chz");
}

/// Golden: the `Contains` operator protocol (L5) — `x in obj` dispatches to a user
/// `contains(self, item) -> bool` on structs (incl. generic `Box[T]`) and enums. Byte-matches the
/// `.expected` file and the M:N engine (parity via `assert_file_parity`).
#[test]
fn golden_contains_protocol_via_run_file() {
    let path = fixture("examples/contains_protocol.chz");
    let expected = std::fs::read_to_string(fixture("examples/contains_protocol.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/contains_protocol.chz");
}

/// Golden: compound assignment (`+=`/`-=`/…) honors struct/enum/newtype operator overloading —
/// `a += V(10)` produces the same value as `a = a + V(10)`. Byte-matches the `.expected` file and
/// the M:N engine (parity via `assert_file_parity`).
#[test]
fn golden_compound_overload_via_run_file() {
    let path = fixture("examples/compound_overload.chz");
    let expected = std::fs::read_to_string(fixture("examples/compound_overload.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/compound_overload.chz");
}

/// M6c golden: the std-library demo (native std.io/math/os + Chezzi std.string) runs end-to-end on
/// the VM and byte-matches both the `.expected` file and the interpreter.
#[test]
fn golden_std_demo_via_run_file() {
    let path = fixture("examples/std_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/std_demo.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/std_demo.chz");
}

// ----- std.io TTY detection (gaps §6) — separated block near golden_std_demo to shrink the
// hand-resolved conflict with the concurrent List-methods task also editing this file. -----
/// `io.isatty()`/`isatty_stdin()`/`isatty_stderr()` each return a plain `bool` without faulting, and
/// serial==M:N (an env fd query is engine-agnostic). The value depends on how the harness's real fds
/// are wired (a terminal → true, a pipe/redirect → false — libtest's capture doesn't touch the fd),
/// so we assert bool-SHAPE + engine agreement, NOT a fixed value.
#[test]
fn golden_isatty_via_run_file() {
    let out = parity_entry(
        "import std.io\nio.print(str(io.isatty()))\nio.print(str(io.isatty_stdin()))\nio.print(str(io.isatty_stderr()))\n",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 isatty lines, got {out:?}");
    for line in lines {
        assert!(
            line == "true" || line == "false",
            "isatty must return a bool, got {line:?}"
        );
    }
}

// ----- std.os system query + mutation fns (gaps §6) — separated block to shrink the hand-resolved
// conflict with the concurrent std.csv task also editing this file. -----
/// setenv writes and environ/env READ the SAME per-VM HostConfig env map (gaps §6 drift-fix): after
/// `os.setenv("K","V")`, BOTH `os.env("K")` and `os.environ()["K"]` observe "V", and a seeded var
/// survives — proving one consistent env source. serial==M:N (per-VM HostConfig, deterministic).
#[test]
fn golden_os_setenv_environ_consistency() {
    let src = "import std.os\nos.setenv(\"K\", \"V\")\nmatch os.env(\"K\"):\n    Some(v): print(v)\n    None: print(\"NONE\")\nprint(os.environ()[\"K\"])\nprint(os.environ()[\"SEED\"])\n";
    let out = parity_entry_cfg(src, || {
        let mut env = std::collections::HashMap::new();
        env.insert("SEED".to_string(), "1".to_string());
        crate::native::HostConfig {
            env: std::sync::Arc::new(std::sync::Mutex::new(env)),
            ..Default::default()
        }
    });
    assert_eq!(out, "V\nV\n1\n");
}

/// getpid/platform/temp_dir/home_dir are engine-agnostic queries: assert SHAPE + serial==M:N
/// agreement (values are machine-dependent — no fixed literal). pid>0, platform nonempty, temp_dir
/// nonempty, home_dir is Some/None-shaped.
#[test]
fn golden_os_queries() {
    let out = parity_entry(
        "import std.os\nprint(str(os.getpid() > 0))\nprint(os.platform())\nprint(str(os.temp_dir() != \"\"))\nmatch os.home_dir():\n    Some(_): print(\"H\")\n    None: print(\"NH\")\n",
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "expected 4 os-query lines, got {out:?}");
    assert_eq!(lines[0], "true", "getpid() must be > 0");
    assert!(!lines[1].is_empty(), "platform() must be nonempty");
    assert_eq!(lines[2], "true", "temp_dir() must be nonempty");
    assert!(
        lines[3] == "H" || lines[3] == "NH",
        "home_dir() must be Some/None-shaped, got {:?}",
        lines[3]
    );
}

/// chdir(abs) -> Ok on a real dir, Err on a missing one; serial==M:N (both run the same
/// absolute `set_current_dir` in the same process, so the second sequential engine run is
/// idempotent). Takes the fs scratch lock + restores cwd (chdir mutates PROCESS-GLOBAL cwd).
#[test]
fn golden_os_chdir() {
    let _g = crate::native::fs::FS_SCRATCH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::current_dir().expect("cwd");
    let out = parity_entry(
        "import std.os\nmatch os.chdir(os.temp_dir()):\n    Ok(_): print(\"OK\")\n    Err(_): print(\"ERR\")\nmatch os.chdir(\"/no/such/chezzi/dir\"):\n    Ok(_): print(\"OK\")\n    Err(_): print(\"ERR\")\n",
    );
    std::env::set_current_dir(&saved).expect("restore cwd");
    assert_eq!(out, "OK\nERR\n");
}

/// environ() must yield keys in a DETERMINISTIC (sorted) order. The source is a std HashMap whose
/// iteration order is randomized per-instance, so `os_environ` sorts before lowering — otherwise the
/// two engines (each a separate HashMap) diverge in key order. This ITERATES the map (not key-index),
/// exercising the ordering the three key-lookup tests never touch.
#[test]
fn golden_os_environ_deterministic_order() {
    let src = "import std.os\nfor k in os.environ().keys():\n    print(k)\n";
    let out = parity_entry_cfg(src, || {
        let mut env = std::collections::HashMap::new();
        for (k, v) in [
            ("zed", "1"),
            ("alpha", "2"),
            ("mid", "3"),
            ("beta", "4"),
            ("qux", "5"),
            ("cee", "6"),
            ("dee", "7"),
            ("eff", "8"),
        ] {
            env.insert(k.to_string(), v.to_string());
        }
        crate::native::HostConfig {
            env: std::sync::Arc::new(std::sync::Mutex::new(env)),
            ..Default::default()
        }
    });
    assert_eq!(out, "alpha\nbeta\ncee\ndee\neff\nmid\nqux\nzed\n");
}

/// setenv inside a spawned task must be visible to the parent after the nursery joins — on BOTH
/// engines. The serial engine runs tasks on the shared parent Vm, so the write lands directly; the
/// M:N engine gives each worker a SHARED (Arc) handle to the one env map, not a per-worker clone, so
/// the write survives back to the parent. Process-global env, like Python `os.environ` / Go
/// `os.Setenv` (visible across threads). Without sharing, M:N prints "unset" and diverges from serial.
#[test]
fn golden_os_setenv_visible_across_tasks() {
    let src = "import std.io\nimport std.os\nfn t():\n    os.setenv(\"TASKVAR\", \"fromtask\")\nfn main():\n    parallel:\n        spawn: t()\n    match os.env(\"TASKVAR\"):\n        Some(v): io.print(v)\n        None: io.print(\"unset\")\nmain()";
    assert_eq!(
        parity_entry_cfg(src, crate::native::HostConfig::default),
        "fromtask\n"
    );
}

/// Additive std.math trig/exp/log intrinsics run end-to-end on the VM and byte-match both the
/// `.expected` file and the interpreter (parity via `assert_file_parity`).
#[test]
fn golden_math_more_via_run_file() {
    let path = fixture("examples/math_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/math_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/math_more.chz");
}

/// `std.bisect` — binary search + sorted-insert over the full boundary matrix (empty, all-equal,
/// both ends, duplicate left/right boundary, in-place insort). Runs end-to-end on the VM, byte-matches
/// the `.expected` file and stays identical on the M:N engine (parity via `assert_file_parity`).
#[test]
fn golden_bisect_via_run_file() {
    let path = fixture("examples/bisect.chz");
    let expected = std::fs::read_to_string(fixture("examples/bisect.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/bisect.chz");
}

/// `std.memoize` — `memoize1` caches per distinct argument (single-eval proven by a captured
/// call-counter: two distinct args → counter == 2, cache hits do NOT re-run `f`). Runs end-to-end on
/// the VM, byte-matches the `.expected` file and stays identical on the M:N engine.
#[test]
fn golden_memoize_via_run_file() {
    let path = fixture("examples/memoize.chz");
    let expected = std::fs::read_to_string(fixture("examples/memoize.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/memoize.chz");
}

/// `std.duration` — Go-like time spans (pure-Chezzi). `to_string`/`parse` across the shape space,
/// parse round-trips, malformed → Err, accessors + arithmetic. Deterministic (no clock) → runs
/// end-to-end on the VM, byte-matches the `.expected` file and stays identical on the M:N engine.
#[test]
fn golden_duration_via_run_file() {
    let path = fixture("examples/duration.chz");
    let expected = std::fs::read_to_string(fixture("examples/duration.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/duration.chz");
}

// ----- std.csv (gaps §7) — separate labeled block to shrink the hand-resolved conflict with the
// concurrent std.os task also editing this file. -----
/// `std.csv` — RFC 4180 parse/format round-trip over every hard case (embedded comma, `""` escaped
/// quote, embedded newline in a quoted field, empty field, sandwiched empty record, unicode) plus
/// direct parse asserts (empty input -> [], trailing separator -> no spurious record). Runs
/// end-to-end on the VM, byte-matches the `.expected` file and stays identical on the M:N engine.
#[test]
fn golden_csv_via_run_file() {
    let path = fixture("examples/csv.chz");
    let expected = std::fs::read_to_string(fixture("examples/csv.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/csv.chz");
}

/// `Channel.trip()` — the manual level-trigger latch (behind `std.cancel`'s `done()`). Tripping
/// makes the channel permanently ready (`recv`/`try_recv` yield `true`, fanning out). Deterministic
/// on every engine → golden + parity.
#[test]
fn golden_channel_trip_via_run_file() {
    let path = fixture("examples/channel_trip.chz");
    let expected = std::fs::read_to_string(fixture("examples/channel_trip.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/channel_trip.chz");
}

/// `std.cancel` — a manual token: `cancelled()`/`reason()` before/after `cancel()`, `done()`
/// readiness, idempotent re-cancel. Pure `Shared`/latch, no timing → golden + parity on every engine.
#[test]
fn golden_cancel_manual_via_run_file() {
    let path = fixture("examples/cancel_manual.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_manual.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_manual.chz");
}

/// `std.cancel` — timeouts + `wait:` integration (`done()` as a wait arm). All cases deterministic
/// (pre-loaded channels / already-elapsed deadline) → golden + parity.
#[test]
fn golden_cancel_timeout_wait_via_run_file() {
    let path = fixture("examples/cancel_timeout_wait.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/cancel_timeout_wait.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_timeout_wait.chz");
}

/// `std.cancel` — cooperative CPU-loop cancellation DIVERGES by engine, so it has no `.expected`
/// (cf. `examples/parallel_cancel.chz`). A sibling's manual `cancel()` aborts the polling worker
/// early on the default OS-thread engine (preemption); the single-threaded cooperative oracle runs
/// the worker to completion first (the canceller only runs at the sequential join). Asserts both.
#[test]
fn cancel_cpu_diverges_by_engine() {
    let path = fixture("examples/cancel_cpu.chz");
    let (par_out, _e, par_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, "worker aborted early\n",
        "default OS-thread engine should preempt + abort"
    );
    let (coop_out, _e, coop_res, _) = run_file(&path);
    assert!(coop_res.is_ok(), "{coop_res:?}");
    assert_eq!(
        coop_out, "worker ran to completion\n",
        "cooperative oracle should run to completion"
    );
}

// ===== std.cancel TREE PROPAGATION (parent/child derivation) =====
//
// A child token derived from a parent observes the parent's cancellation/timeout transitively
// (root-to-leaves, one-directional). The link is LIVE (Shared flag + Shared kids registry cross
// the airlock as live cores), so a parent flip is seen by already-derived children — including
// children that crossed `spawn`/`parallel:`. Cancelling a child never touches the parent.
//
// Each test runs an inline Chezzi snippet through the real module graph (`import std.cancel`
// needs `run_file`, not the standalone compile path). The `print`s are the assertion surface.

/// Wrap a `main()` body in `import std.cancel` + run it through the module graph, return stdout.
fn run_cancel_snippet(tag: &str, body: &str) -> String {
    let src = format!("import std.cancel\n\nfn main():\n{body}\nmain()\n");
    run_cancel_src(tag, &src)
}

/// Run a full `std.cancel`-importing source through the module graph, return stdout.
fn run_cancel_src(tag: &str, src: &str) -> String {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let entry = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
    std::fs::write(&entry, src).expect("write temp .chz");
    let (out, _err, res, _) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "snippet faulted: {res:?}");
    out
}

/// A derived child observes a manual parent cancel (transitive, live link). Child starts live.
#[test]
fn cancel_child_polls_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_polls_parent",
        "    p := cancel.manual()\n    c := p.derive()\n    print(\"c before: {c.cancelled()}\")\n    p.cancel()\n    print(\"c after: {c.cancelled()}\")\n    match c.reason():\n        Some(r): print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n",
    );
    assert_eq!(out, "c before: false\nc after: true\nc reason: cancelled\n");
}

/// Cancelling a child is one-directional: the parent is untouched (flag + reason stay clear).
#[test]
fn cancel_child_cancel_does_not_touch_parent() {
    let out = run_cancel_snippet(
        "cancel_child_one_directional",
        "    p := cancel.manual()\n    c := p.derive()\n    c.cancel()\n    print(\"c: {c.cancelled()}\")\n    print(\"p: {p.cancelled()}\")\n    match p.reason():\n        Some(r): print(\"p reason: {r}\")\n        None:    print(\"p reason: none\")\n",
    );
    assert_eq!(out, "c: true\np: false\np reason: none\n");
}

/// Cascade is transitive: cancelling the root cancels a grandchild with the inherited reason.
#[test]
fn cancel_transitive_grandchild() {
    let out = run_cancel_snippet(
        "cancel_transitive_grandchild",
        "    p := cancel.manual()\n    c := p.derive()\n    g := c.derive()\n    p.cancel()\n    print(\"g: {g.cancelled()}\")\n    match g.reason():\n        Some(r): print(\"g reason: {r}\")\n        None:    print(\"g reason: none\")\n",
    );
    assert_eq!(out, "g: true\ng reason: cancelled\n");
}

/// A manual parent cancel makes a derived child's `done()` channel ready (registry fan-out).
#[test]
fn cancel_child_done_ready_after_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_done_fanout",
        "    p := cancel.manual()\n    c := p.derive()\n    p.cancel()\n    match c.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascades TRANSITIVELY: a manual GRANDPARENT cancel makes a grandchild's `done()`
/// channel ready, not just `cancelled()`. The grandchild's done-channel is registered into every
/// ancestor's `kids`, so the grandparent's fan-out reaches it directly (a `wait:` waiter wakes).
#[test]
fn cancel_grandchild_done_ready_after_grandparent_cancel() {
    let out = run_cancel_snippet(
        "cancel_grandchild_done_fanout",
        "    gp := cancel.manual()\n    mid := gp.derive()\n    leaf := mid.derive()\n    gp.cancel()\n    match leaf.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascade reaches any depth: a manual ROOT cancel makes a great-grandchild's (depth 3)
/// `done()` channel ready, proving the per-ancestor registration walks the whole chain to the root.
#[test]
fn cancel_great_grandchild_done_ready_after_root_cancel() {
    let out = run_cancel_snippet(
        "cancel_ggchild_done_fanout",
        "    root := cancel.manual()\n    a := root.derive()\n    b := a.derive()\n    leaf := b.derive()\n    root.cancel()\n    match leaf.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// A child of an already-elapsed timeout parent inherits the tightest deadline: it is cancelled
/// at once, its reason is "timeout", its done() is ready, and its deadline equals the parent's.
#[test]
fn cancel_child_inherits_tightest_deadline() {
    let out = run_cancel_snippet(
        "cancel_child_tightest_deadline",
        "    p := cancel.timeout(0)\n    c := p.derive()\n    print(\"c cancelled: {c.cancelled()}\")\n    match c.reason():\n        Some(r): print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n    match c.done().try_recv():\n        Some(v): print(\"c done: {v}\")\n        None:    print(\"c not done\")\n    print(\"deadline match: {c.deadline_at() == p.deadline_at()}\")\n",
    );
    assert_eq!(
        out,
        "c cancelled: true\nc reason: timeout\nc done: true\ndeadline match: true\n"
    );
}

/// The live link survives the concurrency airlock: a derived child crosses `spawn`/`parallel:`,
/// and a parent cancel done in the parent task BEFORE the nursery is observed by the spawned task
/// (Shared cores cross as LIVE handles, not snapshots). Deterministic on all engines: the parent
/// already cancelled before the spawn, so the worker only polls the already-flipped flag.
#[test]
fn cancel_token_sendable_with_parent() {
    let src = "import std.cancel\n\nfn watch(c: Token, seen: Shared[bool]):\n    if c.cancelled():\n        seen.set(true)\n\nfn main():\n    p := cancel.manual()\n    c := p.derive()\n    seen := Shared(false)\n    p.cancel()\n    parallel:\n        spawn watch(c, seen)\n    print(\"observed cancel: {seen.get()}\")\n\nmain()\n";
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

/// `std.cancel` tree-propagation golden (VM): `examples/cancel_tree.chz` byte-matches `.expected`
/// and stays identical to the interpreter (`assert_file_parity`). Deterministic (manual cancel +
/// pre-ready/zero timers, same task) → golden on every engine.
#[test]
fn golden_cancel_tree_via_run_file() {
    let path = fixture("examples/cancel_tree.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_tree.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_tree.chz");
}

/// C-ABI FFI golden (VM twin of `interp::golden_ffi_chz`): the `extern "lib":` block calls
/// `cos`/`sqrt` (libm) and `strlen` (libc) via dlopen+libffi on the VM, byte-matches `.expected`,
/// and stays identical to the interpreter (`assert_file_parity`). Linux-only (needs libm/libc).
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_chz_via_run_file() {
    let path = fixture("examples/ffi.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi.chz");
}

/// C-ABI opaque `ptr` handle golden (VM twin of `interp::golden_ffi_ptr_chz`): the `extern "lib":`
/// block opens a libc `FILE*` (`fopen -> ptr`), hands the handle back to `fclose`, and detects a
/// NULL handle via `std.ffi.is_null` / `== std.ffi.null()`. Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity`). Linux-only (needs libc).
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_ptr_chz_via_run_file() {
    let path = fixture("examples/ffi_ptr.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_ptr.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_ptr.chz");
}

/// C struct BY VALUE golden (VM twin of `interp::golden_ffi_struct_chz`): the `extern "lib":`
/// block binds `div_t div(int, int)` — a libc fn taking two scalars and returning a small POD
/// struct BY VALUE — to a Chezzi `struct DivT{quot, rem}`. `div(17, 5) == {3, 2}` (pure, always
/// present). Byte-matches `.expected` and stays identical to the interpreter (`assert_file_parity`
/// runs the serial VM + interp). The `--parallel`/M:N engine is NOT driven here; the returned
/// `NativeRet::Struct` already crosses the M:N airlock by the same deep-copy std.regex uses, so the
/// path is exercised, just not by this golden. Linux-only.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_struct_chz_via_run_file() {
    let path = fixture("examples/ffi_struct.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_struct.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_struct.chz");
}

/// CAPSTONE C-buffer FFI golden (VM twin of `interp::golden_ffi_qsort_chz`): sort a Chezzi list
/// with libc `qsort`, composing `ffi.alloc` + `store_int64_at` + a Chezzi `qsort` comparator
/// callback (`load_int64` both `const void*` sides) + `load_int64_at` read-back + `defer ffi.free`.
/// The marquee proof that callbacks + deref + alloc all compose. Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity` runs the serial VM + interp). Linux-only
/// (needs libc); the fixed 8-byte int64 stride matches the comparator on every LP64 unix.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_qsort_chz_via_run_file() {
    let path = fixture("examples/ffi_qsort.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_qsort.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_qsort.chz");
    // Also exercise the M:N `--parallel` engine: the qsort comparator re-enters the VM as a
    // libffi callback under worker pinning — the highest-risk callback+alloc composition, and
    // the one path the cooperative-VM + interp parity above does not cover. Keeps the engine
    // matrix honest (the sibling deref test runs `--parallel` too).
    let (par_out, _par_err, par_res, _) =
        run_file_parallel(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, expected,
        "M:N --parallel diverged for ffi_qsort.chz"
    );
}

/// C-ABI deeper `str` returns golden (VM twin of `interp::golden_ffi_str_chz`): `strdup -> owned_str`
/// (owned `char*` copied into a Chezzi `str` AND freed) and `getenv -> str?` (NULL → `None`). Byte-
/// matches `.expected` and stays identical to the interpreter (`assert_file_parity`). Linux-only.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_str_chz_via_run_file() {
    let path = fixture("examples/ffi_str.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_str.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_str.chz");
}

/// C-ABI fixed-width integer marshalling golden (VM twin of `interp::golden_ffi_int_chz`): `atoi
/// -> int32` (sign-extend), `htonl(uint32) -> uint32` (zero-extend, high-bit positive), `abs(int8)
/// -> int8` (signed round-trip + param truncation per a C cast). Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity`). Linux-only (needs libc).
// The example's `htonl` lines encode little-endian oracles, so the golden is LE-gated.
#[test]
#[cfg(all(target_os = "linux", target_endian = "little"))]
fn golden_ffi_int_chz_via_run_file() {
    let path = fixture("examples/ffi_int.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_int.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_int.chz");
}

/// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
/// the VM, byte-matches `.expected`, and stays identical to the interpreter.
#[test]
fn golden_overflow_via_run_file() {
    // The integer-overflow policy, end-to-end: every overflow (arith, neg, div, math.abs) is a
    // recoverable fault, identical on both engines.
    let path = fixture("examples/overflow.chz");
    let expected = std::fs::read_to_string(fixture("examples/overflow.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/overflow.chz");
}

#[test]
fn golden_stats_app_via_run_file() {
    let path = fixture("examples/stats.chz");
    let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/stats.chz");
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
    assert_file_parity("examples/stdlib_cmp.chz");
}

/// std.flag: `--name value` + `--count=3` `=`-form + a trailing positional, both engines.
#[test]
fn flag_parse_value_and_eq_form_parity() {
    let out = parity_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.str_flag(\"name\", \"def\", \"the name\")\n\
         fs.int_flag(\"count\", 0, \"how many\")\n\
         match fs.parse([\"--name\", \"alice\", \"--count=3\", \"pos1\"]):\n\
         \x20   Ok(p):\n\
         \x20       print(fs.get_str(\"name\"))\n\
         \x20       print(fs.get_int(\"count\"))\n\
         \x20       print(p[0])\n\
         \x20   Err(e):\n\
         \x20       print(e.message())\n",
    );
    assert_eq!(out, "alice\n3\npos1\n");
}

/// std.flag: bool-as-presence + the `--` terminator makes later dash tokens positional.
#[test]
fn flag_bool_presence_and_terminator_parity() {
    let out = parity_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.bool_flag(\"verbose\", false, \"v\")\n\
         match fs.parse([\"--verbose\", \"--\", \"--notaflag\", \"x\"]):\n\
         \x20   Ok(p):\n\
         \x20       print(fs.get_bool(\"verbose\"))\n\
         \x20       print(\" \".join(fs.positionals()))\n\
         \x20   Err(e):\n\
         \x20       print(e.message())\n",
    );
    assert_eq!(out, "true\n--notaflag x\n");
}

/// std.flag: unknown flag / missing value / non-int are clean `Err`, never a fault, both engines.
#[test]
fn flag_error_paths_parity() {
    let unknown = parity_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         match fs.parse([\"--nope\"]):\n\
         \x20   Ok(p): print(\"ok\")\n\
         \x20   Err(e): print(e.message())\n",
    );
    assert_eq!(unknown, "unknown flag --nope\n");

    let missing = parity_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.str_flag(\"name\", \"\", \"n\")\n\
         match fs.parse([\"--name\"]):\n\
         \x20   Ok(p): print(\"ok\")\n\
         \x20   Err(e): print(e.message())\n",
    );
    assert_eq!(missing, "flag --name: missing value\n");

    let badint = parity_entry(
        "import std.flag\n\
         fs := flag.new()\n\
         fs.int_flag(\"count\", 0, \"c\")\n\
         match fs.parse([\"--count\", \"abc\"]):\n\
         \x20   Ok(p): print(\"ok\")\n\
         \x20   Err(e): print(\"errored\")\n",
    );
    assert_eq!(badint, "errored\n");
}

/// std.flag: `usage()` is a byte-exact multi-line string in REGISTRATION order (parity-safe).
#[test]
fn flag_usage_deterministic_parity() {
    let out = parity_entry(
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
    assert_file_parity("examples/flag_demo.chz");
}

/// std.log golden: `examples/log_demo.chz` exercises gating, all 4 level formats, set_level, an
/// injectable prefix, the pure `format_line`, and stderr-default + stdout target in one program.
/// Log lines land on STDERR, so `.expected` pins the STDERR stream; discrimination asserts prove the
/// stream routing; and it stays identical on the serial + M:N engines (parity compares both streams).
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
    assert_file_parity("examples/log_demo.chz");
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
    assert_file_parity("examples/str_more.chz");
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
    assert_file_parity("examples/str_methods.chz");
}

/// `iter.reduce` has no seed, so an empty list has no accumulator to start from. It used to leak the
/// std module's own internal index error (`index 0 out of bounds (len 0)` at a std/iter.chz line
/// number) — an implementation detail, and a confusing one, since the user never wrote that index.
/// It faults with a named, user-facing message instead (Python: `TypeError: reduce() of empty
/// iterable with no initial value`). Still an ordinary recoverable fault, never a host panic.
#[test]
fn reduce_empty_list_faults_with_named_message() {
    let src = "import std.iter as iter\ne: List[int] = []\nprint(iter.reduce(e, fn(a: int, b: int) -> int: a + b))\n";
    // Faults identically on both engines (`parity_entry_fault` asserts serial == M:N).
    let e = parity_entry_fault(src);
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
        Ok(v): print("no fault {v}")
        Err(err): print("caught: {err.message()}")
    print(iter.reduce([1, 2, 3], fn(a: int, b: int) -> int: a + b))
    print(iter.reduce([7], fn(a: int, b: int) -> int: a + b))
main()
"#;
    // `parity_entry` runs the graph path on BOTH engines and asserts byte-identical stdout.
    let out = parity_entry(src);
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
    assert_file_parity("examples/iter_more.chz");
}

// --- lazy iterator adapters (gaps §3) — count/repeat/cycle/chain/islice/imap/ifilter ---
// Each RUNS inline source through the graph path on BOTH engines (serial + M:N) via parity_entry
// and asserts byte-identical stdout. The count+islice test is the laziness canary: an infinite
// source pulled through a finite prefix MUST terminate — if it hangs, laziness broke.

#[test]
fn test_lazy_count_islice_terminates() {
    // Infinite count() → islice prefix of 5. Must terminate.
    let out = parity_entry(
        "import std.iter\nfn main():\n  out:=[]\n  for x in iter.islice(iter.count(), 5): out.push(x)\n  print(out)\nmain()\n",
    );
    assert_eq!(out, "[0, 1, 2, 3, 4]\n");
    // count(start, step) arithmetic + stop<=0 = empty.
    let out2 = parity_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.count(10, 3), 4): a.push(x)\n  b:=[]\n  for x in iter.islice(iter.count(), 0): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out2, "[10, 13, 16, 19]\n[]\n");
}

#[test]
fn test_lazy_islice_consumes_exactly_stop() {
    // islice pulls EXACTLY `stop` from a shared live source (CPython itertools.islice contract), not
    // stop+1. Slice 3 off a live count(), then slice 2 more off the SAME generator: exact ⇒ [0,1,2]
    // then [3,4]; over-consuming by one would leave the source at 4 ⇒ [4,5].
    let out = parity_entry(
        "import std.iter\nfn main():\n  g:=iter.count()\n  a:=[]\n  for x in iter.islice(g, 3): a.push(x)\n  b:=[]\n  for x in iter.islice(g, 2): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[0, 1, 2]\n[3, 4]\n");
}

// std.string.count with an EMPTY substring matches Python/Go (`len(s)+1`), not 0 (the old guard drifted
// from both ancestors + from its own sibling `index_of(s,"")==0`). Both engines agreed on the wrong value
// before the fix, so the parity oracle was blind — this pins the corrected value on both engines.
#[test]
fn string_count_empty_substring_matches_python() {
    let out = parity_entry(
        "import std.string\nfn main():\n  print(string.count(\"abc\", \"\"))\n  print(string.count(\"hello\", \"\"))\n  print(string.count(\"\", \"\"))\n  print(string.count(\"abc\", \"b\"))\nmain()\n",
    );
    // Python: "abc".count("")==4, "hello".count("")==6, "".count("")==1, "abc".count("b")==1.
    assert_eq!(out, "4\n6\n1\n1\n");
}

#[test]
fn test_lazy_repeat() {
    let out = parity_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.repeat(7), 3): a.push(x)\n  b:=[]\n  for x in iter.repeat(9, 2): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[7, 7, 7]\n[9, 9]\n");
}

#[test]
fn test_lazy_cycle_and_empty() {
    let out = parity_entry(
        "import std.iter\nfn main():\n  a:=[]\n  for x in iter.islice(iter.cycle([1,2,3]), 7): a.push(x)\n  b:=[]\n  for x in iter.cycle([]): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    // cycle of empty list terminates to [] (not an infinite spin).
    assert_eq!(out, "[1, 2, 3, 1, 2, 3, 1]\n[]\n");
}

#[test]
fn test_lazy_chain_order() {
    let out = parity_entry(
        "import std.iter\nfn main():\n  out:=[]\n  for x in iter.chain([1,2],[3,4]): out.push(x)\n  print(out)\nmain()\n",
    );
    assert_eq!(out, "[1, 2, 3, 4]\n");
}

#[test]
fn test_lazy_imap_ifilter_compose() {
    // Lazy map/filter over an infinite count(), sliced — must compose and terminate.
    let out = parity_entry(
        "import std.iter\nfn double(x:int)->int: return x*2\nfn is_even(x:int)->bool: return x%2==0\nfn main():\n  a:=[]\n  for x in iter.islice(iter.imap(iter.count(1), double), 4): a.push(x)\n  b:=[]\n  for x in iter.islice(iter.ifilter(iter.count(), is_even), 3): b.push(x)\n  print(a)\n  print(b)\nmain()\n",
    );
    assert_eq!(out, "[2, 4, 6, 8]\n[0, 2, 4]\n");
}

/// std.rand goldens — run as ONE test so they execute sequentially. The PRNG is a process-global
/// (shared by VM + interp + every test in the run); two *separate* `#[test]`s seeding-then-drawing
/// would interleave on that global under the test harness's parallel runner and diverge. Drawn
/// sequentially within one test body, each program's `rand.seed()`-then-draw stream is
/// deterministic and byte-identical across engines (`assert_file_parity` checks VM == interp).
/// (This is the same concurrent-draw limit documented in docs/stdlib.md §std.rand — the goldens
/// avoid it by serializing.)
///
/// - `rand_seeded`: all four scalar fns (seed/int/float/bool), seeded → deterministic values.
/// - `rand_shape`: UNSEEDED (OS-entropy path); asserts only range/shape, prints fixed "ok" lines
///   (value-independent, so byte-identical across engines despite the nondeterministic seed).
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
        assert_file_parity(rel);
    }
}

/// std.path golden: `examples/path.chz` — the pure-Chezzi unix path-STRING ops
/// (is_abs/is_rel/basename/dirname/split/ext/stem/with_ext/normalize/join), end-to-end on the
/// VM, byte-identical to `.expected` and the interpreter (assert_file_parity == VM==interp).
#[test]
fn golden_path_via_run_file() {
    let path = fixture("examples/path.chz");
    let expected = std::fs::read_to_string(fixture("examples/path.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/path.chz");
}

/// std.datetime golden: `examples/datetime.chz` — the pure-Chezzi civil-calendar module
/// (from_epoch/to_epoch/formatters/duration/leap/weekday on FIXED epochs only, no clock),
/// end-to-end on the VM, byte-identical to `.expected` and the interpreter (assert_file_parity).
#[test]
fn golden_datetime_via_run_file() {
    let path = fixture("examples/datetime.chz");
    let expected = std::fs::read_to_string(fixture("examples/datetime.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/datetime.chz");
}

/// std.concurrency.collection golden: `examples/concurrent_collection.chz` — the pure-Chezzi
/// thread-safe `ConcurrentMap`/`ConcurrentCounter` wrappers over `RwShared[Map[...]]`. The program
/// is DETERMINISTIC by construction (single-write-lock RMW for the counter, each-own-key for the
/// map), so its output is identical on the VM, the interpreter (assert_file_parity), and (verified
/// manually) `--parallel`.
#[test]
fn golden_concurrent_collection_via_run_file() {
    let path = fixture("examples/concurrent_collection.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/concurrent_collection.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/concurrent_collection.chz");
}

/// std.collections golden: `examples/collections.chz` — the pure-Chezzi Heap/Deque/Counter
/// module (min/max heap + from_list, two-stack deque both ends, Counter most_common ties),
/// end-to-end on the VM, byte-identical to `.expected` and the interpreter (assert_file_parity).
#[test]
fn golden_collections_via_run_file() {
    let path = fixture("examples/collections.chz");
    let expected = std::fs::read_to_string(fixture("examples/collections.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/collections.chz");
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
    assert_file_parity("examples/json_decode.chz");
}

/// M8-M3 golden: `examples/sys.chz` — the native trio std.process/std.fs/std.time, end-to-end
/// on the VM, byte-identical to `.expected` and the interpreter (deterministic ops only).
#[test]
fn golden_sys_via_run_file() {
    let path = fixture("examples/sys.chz");
    let expected = std::fs::read_to_string(fixture("examples/sys.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/sys.chz");
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
    assert_file_parity("examples/process_polish.chz");
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
    assert_file_parity("examples/fs_mutations.chz");
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
        assert_file_parity(rel);
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
    assert_file_parity("examples/uuid_shape.chz");
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
    assert_file_parity("examples/comprehensions.chz");
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
    assert_file_parity("examples/comprehensions_nested.chz");
}

/// Stateful-iterator comprehension golden: `examples/comprehension_iter_state.chz` — a
/// comprehension whose element/guard read a struct iterator's LIVE, per-`next()` state. The
/// iterator must be driven lazily (interleaved with the body), so this byte-matches `.expected`
/// and stays identical on interp + VM (regression for the eager-drain parity bug).
#[test]
fn golden_comprehension_iter_state_via_run_file() {
    let path = fixture("examples/comprehension_iter_state.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/comprehension_iter_state.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/comprehension_iter_state.chz");
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
    assert_file_parity("examples/hex.chz");
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
    assert_file_parity("examples/concat_merge.chz");
}

/// Tuple-destructuring `for` + `std.iter` golden: `examples/for_tuple.chz` — destructure a list
/// of tuples, one-var whole-tuple, triples, `enumerate`/`zip`, comprehension combo. Byte-matches
/// `.expected`, identical on interp + VM (the `IsMap` runtime split is exercised alongside maps).
#[test]
fn golden_for_tuple_via_run_file() {
    let path = fixture("examples/for_tuple.chz");
    let expected = std::fs::read_to_string(fixture("examples/for_tuple.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/for_tuple.chz");
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
    assert_file_parity("examples/optchain.chz");
}

/// Runtime stack trace: a faulting nested call reports the error line + the call chain (innermost
/// first) with each call's line. Asserted on the VM, and the interp must produce the IDENTICAL
/// formatted trace (frames carry the same call-site spans on both engines).
#[test]
fn stack_trace_reports_call_chain_on_both_engines() {
    let path = fixture("examples/stack_trace.chz");
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "division by zero");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["divide", "compute", "main"]);
    // Call-site lines, innermost first.
    let lines: Vec<usize> = e.trace.iter().map(|f| f.span.line).collect();
    assert_eq!(lines, vec![15, 18, 20]);
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    assert!(
        vm_fmt.contains("at divide (called at line 15"),
        "got: {vm_fmt}"
    );

    // Interp parity: identical formatted trace.
    let (_o, _er, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("program should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(vm_fmt, ip_fmt, "engines must produce the same stack trace");
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
    let fmt = format_trace(&e.message, e.span, &e.trace);
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

/// Gap #8 parity: the bounded/collapsed trace must be byte-identical across both engines.
#[test]
fn recursion_trace_parity_vm_vs_interp() {
    let path = deep_recursion_fixture("parity");
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("deep recursion should fault");
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    let (_o2, _e2, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("deep recursion should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must produce the same bounded/collapsed trace"
    );
}

/// Gap #8 cap path: a deep chain of DISTINCT-named frames (no collapse possible) is still bounded
/// by the head/tail cap with a `frames elided` marker, and stays byte-identical across engines.
#[test]
fn format_trace_caps_distinct_name_chain() {
    let span = Span { line: 1, col: 1 };
    let trace: Vec<TraceFrame> = (0..50)
        .map(|n| TraceFrame {
            function: format!("f{n}"),
            span,
        })
        .collect();
    let vm_fmt = format_trace("boom", span, &trace);
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
    let ip_fmt = format_trace("boom", span, &ip_trace);
    assert_eq!(vm_fmt, ip_fmt, "engines must agree on the capped trace");
}

/// Gap #8 cap boundary: with many short same-name runs the collapse emits `× N` markers; the cap
/// must never split a marker from its `at` line (markers stay inside their entry). Every `× N`
/// line must be immediately preceded by its `at` line, head and tail alike, and engines agree.
#[test]
fn format_trace_cap_never_orphans_collapse_marker() {
    let span = Span { line: 1, col: 1 };
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
    let vm_fmt = format_trace("boom", span, &trace);
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
    let ip_fmt = format_trace("boom", span, &ip_trace);
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
/// must reflect the DEFERRED fault's chain (deeper, includes the deferred fn), identically on
/// both engines — not the original body fault's chain.
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
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    let (_o2, _e2, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must agree on a deferred-fault trace"
    );
}

/// gaps.md B4: an uncaught fault thrown from an `Executor.submit(...)` closure must print the SAME
/// backtrace frames on both engines. Previously `--serial` drained the submitted task INLINE on the
/// entry `Vm`, so the task's callee frames survived into `fault_trace` and printed `at boom` /
/// `at <closure>` / `at main`, while M:N ran each task on an isolated worker `Vm` and discarded that
/// worker's trace, printing only `at main`. Both engines now converge on `[main]` — matching a plain
/// nursery-task panic (already `at main` on both engines, asserted by the neighbor guard below).
#[test]
fn executor_task_fault_trace_matches_on_both_engines() {
    let dir = std::env::temp_dir().join("chezzi_b4_executor_trace");
    std::fs::create_dir_all(&dir).unwrap();

    // Executor.submit path.
    let ex_src = "import std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): boom())\n    ex.shutdown()\nmain()\n";
    let ex_path = dir.join("ex.chz");
    std::fs::write(&ex_path, ex_src).unwrap();
    let (_o, _e, se, _) = run_file(&ex_path);
    let (_o, _e, mn, _) = run_file_p(&ex_path);
    let se = se.expect_err("serial should fault");
    let mn = mn.expect_err("M:N should fault");
    let se_names: Vec<&str> = se.trace.iter().map(|f| f.function.as_str()).collect();
    let mn_names: Vec<&str> = mn.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(se_names, vec!["main"], "serial Executor trace == [main]");
    assert_eq!(mn_names, vec!["main"], "M:N Executor trace == [main]");
    // Soundness invariants (message + location) stay identical across engines.
    assert_eq!(se.message, mn.message, "same fault message");
    assert_eq!(se.span, mn.span, "same fault location");

    // Neighbor guard: a plain nursery-task panic (non-Executor) was already `at main` on both
    // engines — the fix must not regress it.
    let nu_src = "fn boom():\n    panic(\"kaboom\")\nfn main():\n    parallel:\n        spawn: boom()\nmain()\n";
    let nu_path = dir.join("nu.chz");
    std::fs::write(&nu_path, nu_src).unwrap();
    let (_o, _e, nse, _) = run_file(&nu_path);
    let (_o, _e, nmn, _) = run_file_p(&nu_path);
    let nse = nse.expect_err("serial nursery should fault");
    let nmn = nmn.expect_err("M:N nursery should fault");
    let nse_names: Vec<&str> = nse.trace.iter().map(|f| f.function.as_str()).collect();
    let nmn_names: Vec<&str> = nmn.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(nse_names, vec!["main"], "serial nursery trace unchanged");
    assert_eq!(nmn_names, vec!["main"], "M:N nursery trace unchanged");

    // B4 review edge 1: IMPLICIT end-of-program drain (no `ex.shutdown()`) — the executor is
    // reaped by `drain_live_executors` AFTER `main` returned, so there is no enclosing `run_until`
    // to re-capture at: both engines print the fault with an EMPTY trace. Parity holds (both []).
    let im_src = "import std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): boom())\nmain()\n";
    let im_path = dir.join("implicit.chz");
    std::fs::write(&im_path, im_src).unwrap();
    let (_o, _e, ise, _) = run_file(&im_path);
    let (_o, _e, imn, _) = run_file_p(&im_path);
    let ise = ise.expect_err("serial implicit drain should fault");
    let imn = imn.expect_err("M:N implicit drain should fault");
    let ise_names: Vec<&str> = ise.trace.iter().map(|f| f.function.as_str()).collect();
    let imn_names: Vec<&str> = imn.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        ise_names,
        Vec::<&str>::new(),
        "serial implicit-drain trace empty"
    );
    assert_eq!(
        imn_names,
        Vec::<&str>::new(),
        "M:N implicit-drain trace empty"
    );

    // B4 review edge 2 (the medium charge): `defer ex.shutdown()` while `main` is unwinding from
    // an outer panic. The drain must drop ONLY the inline task's frames, NOT the superseding outer
    // fault already captured — else serial nukes it to [] while M:N keeps [main] (re-introducing the
    // serial != M:N divergence this fix exists to kill). The submitted task's fault supersedes; both
    // engines report `kaboom` at `[main]`.
    let df_src = "import std.concurrency\nfn boom():\n    panic(\"kaboom\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): boom())\n    defer ex.shutdown()\n    panic(\"outer\")\nmain()\n";
    let df_path = dir.join("defer_unwind.chz");
    std::fs::write(&df_path, df_src).unwrap();
    let (_o, _e, dse, _) = run_file(&df_path);
    let (_o, _e, dmn, _) = run_file_p(&df_path);
    let dse = dse.expect_err("serial defer-unwind should fault");
    let dmn = dmn.expect_err("M:N defer-unwind should fault");
    let dse_names: Vec<&str> = dse.trace.iter().map(|f| f.function.as_str()).collect();
    let dmn_names: Vec<&str> = dmn.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        dse_names,
        vec!["main"],
        "serial defer-unwind trace == [main]"
    );
    assert_eq!(dmn_names, vec!["main"], "M:N defer-unwind trace == [main]");
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
    assert_file_parity("examples/default_expr.chz");
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
    assert_file_parity("examples/fn_field.chz");
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
    assert_file_parity("examples/sort_by_key.chz");
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
    assert_file_parity("examples/const_binding.chz");
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
    assert_file_parity("examples/tuple_match.chz");
}

/// `std.os.exit(code)` golden: `examples/exit.chz` halts at the negative branch with status 2.
/// Byte-matches `.expected` on both engines and both report the same exit code.
#[test]
fn golden_exit_via_run_file() {
    let path = fixture("examples/exit.chz");
    let expected = std::fs::read_to_string(fixture("examples/exit.expected")).unwrap();
    let (vo, _ve, vr, vc) = run_file(&path);
    let (io, _ie, ir, ic) = run_file_p(&path);
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
    assert_file_parity("examples/json_dynamic.chz");
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
    assert_file_parity("examples/regex_demo.chz");
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
    assert_file_parity("examples/knapsack.chz");
}

/// `Iterator[T]` golden: a generic fn bounded `[S: Iterator[T], T]` over list/str/set/struct,
/// with the element type flowing into returns. Parity-checked across both engines.
#[test]
fn golden_iterator_bound_via_run_file() {
    let path = fixture("examples/iterator_bound.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterator_bound.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iterator_bound.chz");
}

/// Lazy iterator adapters (Take/Mapped over an infinite Count) — the no-`yield` story. The inner
/// `self.inner.next()` recovers the element type through the `I: Iterator[T]` bound on both engines.
#[test]
fn golden_iter_adapters_via_run_file() {
    let path = fixture("examples/iter_adapters.chz");
    let expected = std::fs::read_to_string(fixture("examples/iter_adapters.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iter_adapters.chz");
}

/// `Iterable[T]` + `.iter()` — a list flows into the Take/Mapped adapter pipeline, `.iter()`+manual
/// `.next()` on every collection, a pure-`Iterable` struct drives `for`, an `[S: Iterable[T]]` fn
/// over a list AND a struct iterator, empty/idempotent cursors, `List(xs.iter())` round-trip. Byte-
/// identical on VM + interp (the `--serial` third engine is asserted in the regression script).
#[test]
fn golden_iterable_via_run_file() {
    let path = fixture("examples/iterable.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterable.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iterable.chz");
}

#[test]
fn golden_multi_file_project_via_vm() {
    let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/proj/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, expected);
    assert_file_parity("tests/fixtures/proj/main.chz");
}

/// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
/// constant must resolve against *its own* module, not the caller — even when the caller
/// defines a same-named global with a different value.
#[test]
fn imported_fn_uses_home_globals() {
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, "from-lib\nfrom-main\n");
    assert_file_parity("tests/fixtures/homeglobals/main.chz");
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
    assert_parity(src);
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
    assert_parity(src);
    assert!(
        vm_outcome(src).unwrap_err().contains("key not found"),
        "{:?}",
        vm_outcome(src)
    );
}

// ----- Hashable struct keys (hash-table map/set) -----

/// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
/// the checker, so the runtime must error consistently (not panic) on both engines.
#[test]
fn parity_map_struct_key_missing_hash_errors() {
    let src = "\
struct P:
    x: int
fn main():
    m: Map[P, int] = {}
    m[P(1)] = 5
main()";
    assert_parity(src);
}

/// REGRESSION (AsInt relocation): a non-int LIST index now errors at runtime in `GetIndex`,
/// with the SAME message the removed `AsInt` produced. The checker is bypassed by `run_capture`,
/// so this exercises the relocated runtime validation on both engines.
#[test]
fn parity_list_non_int_index_still_errors() {
    let src = "xs := [1, 2, 3]\nprint(xs[\"a\"])\n";
    assert_parity(src);
    assert!(
        vm_outcome(src)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src)
    );
    // And on assignment (SetIndex relocation).
    let src2 = "xs := [1, 2, 3]\nxs[\"a\"] = 9\n";
    assert_parity(src2);
    assert!(
        vm_outcome(src2)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src2)
    );
}

#[test]
fn parity_map_gc_stress_heap_keys_and_values() {
    // Keys AND values are heap strings; build many maps so collection runs mid-stream and the
    // `Heap::children` tracing of BOTH keys and values is exercised (a use-after-free if either
    // is untraced). The keys()/values() lists also hold heap children.
    let src = "fn main():\n    i := 0\n    while i < 200:\n        m := {\"k{i}\": \"v{i}\"}\n        m[\"extra\"] = \"x{i}\"\n        if i == 199:\n            print(m[\"k{i}\"])\n            print(m.values())\n        i += 1\nmain()\n";
    assert_parity(src);
    let expected = "v199\n[v199, x199]\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (untraced map key/value?)"
    );
}

// (Removed `bench_vm_faster_than_interp`: it timed the bytecode VM against the tree-walk
// interpreter, which no longer exists. Perf tracking lives in `benches/run.chz` vs CPython and
// `docs/benchmarks.md`; serial-vs-M:N output parity on loop-heavy code is covered by other tests.)

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn parity_tuple_literal_display() {
    assert_parity_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
}

#[test]
fn parity_tuple_element_access() {
    assert_parity_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
}

#[test]
fn parity_tuple_element_out_of_range_errors() {
    // The checker would catch `.2` statically, but `t` here is built so both engines hit the
    // runtime bounds check with the identical message — parity on the error path.
    assert_parity("t := (1, 2)\nprint(t.0)\nprint(t.1)\n");
}

#[test]
fn parity_destructure_local() {
    assert_parity_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
}

#[test]
fn parity_tuple_equality() {
    assert_parity_out(
        "print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n",
        "true\nfalse\n",
    );
}

#[test]
fn parity_multi_return_destructured_at_call_site() {
    let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
    assert_parity_out(src, "7\n");
}

#[test]
fn parity_tuple_heap_elements_gc_stress() {
    // A tuple of heap values (a string + a list). Under GC stress a collection happens between
    // building the tuple and reading it back — proving `Heap::children` traces tuple elements.
    let src = "t := (\"hi\", [1, 2, 3])\nprint(t.0)\nprint(t.1)\n";
    assert_parity(src);
    assert_eq!(
        run_capture_stress(src),
        "hi\n[1, 2, 3]\n",
        "tuple elements not GC-traced?"
    );
}

// ----- slicing + Index/IndexSet/Slice protocol dispatch (VM ↔ interp parity) -----

#[test]
fn slice_list_and_str_parity() {
    assert_parity_out("print([1, 2, 3, 4, 5][1:3])\n", "[2, 3]\n");
    assert_parity_out("print(\"hello\"[0:2])\n", "he\n");
    // Open bounds + step + reverse — both engines byte-identical.
    assert_parity_out("print([1, 2, 3, 4, 5][2:])\n", "[3, 4, 5]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][:2])\n", "[1, 2]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][::2])\n", "[1, 3, 5]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][::-1])\n", "[5, 4, 3, 2, 1]\n");
    assert_parity_out("print(\"hello\"[::-1])\n", "olleh\n");
    // Multibyte UTF-8 must round-trip through char-stepping (SSO/heap boundary) on both engines.
    assert_parity_out("print(\"héllo\"[::-1])\n", "olléh\n");
    assert_parity_out("print(\"héllo\"[1:3])\n", "él\n");
}

#[test]
fn slice_clamped_parity() {
    assert_parity_out("print([1, 2, 3][1:99])\n", "[2, 3]\n");
    assert_parity_out("print([1, 2, 3][2:1])\n", "[]\n");
    // Negative bound counts from the end (Python): [-1:2] -> start=2,end=2 -> [].
    assert_parity_out("print([1, 2, 3][-1:2])\n", "[]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][-2:])\n", "[4, 5]\n");
    // Slice bounds CLAMP (no fault) even far out of range...
    assert_parity_out("print([1, 2, 3][-100:])\n", "[1, 2, 3]\n");
    // ...but a plain out-of-range negative index FAULTS, byte-identically.
    assert_parity("print([1, 2, 3][0 - 100])\n");
    // Zero step faults with the same message in both engines.
    assert_parity("print([1, 2, 3][::0])\n");
}

#[test]
fn negative_index_parity() {
    assert_parity_out("print([10, 20, 30][-1])\n", "30\n");
    assert_parity_out("print(\"hello\"[-2])\n", "l\n");
    assert_parity_out("xs := [1, 2, 3]\nxs[-1] = 99\nprint(xs[2])\n", "99\n");
}

const BUF_PROG: &str = "\
struct Buf:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> List[int]:
        match (start, end, step):
            (Some(s), Some(e), Some(c)): return self.xs[s:e:c]
            (Some(s), Some(e), None): return self.xs[s:e]
            (Some(s), None, Some(c)): return self.xs[s::c]
            (Some(s), None, None): return self.xs[s:]
            (None, Some(e), Some(c)): return self.xs[:e:c]
            (None, Some(e), None): return self.xs[:e]
            (None, None, Some(c)): return self.xs[::c]
            (None, None, None): return self.xs[:]
            _: return self.xs[:]
fn main():
    b := Buf([10, 20, 30])
    print(b[0])
    b[1] = 99
    print(b[1])
    b[0] += 5
    print(b[0])
    print(b[0:2])
    print(b[:])
    print(b[::-1])
main()";

#[test]
fn struct_index_slice_dispatch_parity() {
    assert_parity_out(
        BUF_PROG,
        "10\n99\n15\n[15, 99]\n[15, 99, 30]\n[30, 99, 15]\n",
    );
}

#[test]
fn slice_survives_gc_stress() {
    // The sliced list shares the source's element handles; a GC during the slice alloc must not
    // collect them. (Source is an inline temporary, unrooted except by the slice path.)
    let src = "print([1, 2, 3, 4, 5][1:4])\n";
    assert_parity(src);
    assert_eq!(
        run_capture_stress(src),
        "[2, 3, 4]\n",
        "slice elements not GC-rooted?"
    );
}

// ----- M19 lever #3: positional closure captures (HashMap → Vec<Value> by compile-time slot).
// Characterization net: these pin observable behavior across BOTH engines so the HashMap→Vec
// refactor cannot silently diverge. They pass on the pre-refactor (HashMap) code too. -----

/// Closure-capture golden: `examples/closure_capture.chz` (one var, several vars, nested
/// capture, a shared mutable box, HOF callbacks, and a hot-loop capture read) byte-identical on
/// the VM, the interpreter, the `--parallel` engine, and its `.expected`. The two-engine parity
/// is the oracle for M19 lever #3 (positional captures, HashMap → Vec<Value> by compile-time slot).
#[test]
fn golden_closure_capture_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/closure_capture.chz");
    let expected = include_str!("../../examples/closure_capture.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// Closure-capture-across-scopes golden: `examples/closure_capture_scopes.chz` test-locks the
/// uniform by-reference capture rule — a plain local is shared and sees later writes (`20`), a
/// nested fn writes a captured local visible to the caller (`20`), and a global is referenced live
/// (`20`). Byte-identical on the VM, the interpreter, the `--parallel` engine, and its `.expected`.
/// It runs through the real module graph via `run_file` (a temp entry), not
/// `run_capture`/`compile_module_standalone`.
#[test]
fn golden_closure_capture_scopes_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/closure_capture_scopes.chz");
    let expected = include_str!("../../examples/closure_capture_scopes.expected");
    let dir = std::env::temp_dir().join(format!("chezzi_vm_cap_scopes_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let (po, _pe, pr, _pc) = run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    assert!(pr.is_ok(), "parallel faulted: {pr:?}");
    assert_eq!(vo, expected, "VM output vs .expected");
    assert_eq!(vo, io, "VM vs interp divergence");
    assert_eq!(vo, po, "VM vs parallel divergence");
}

// ----- Uniform by-reference closure capture (the acceptance matrix, §4 of the design). Each row is
// a runnable `examples/capture_*.chz` + `.expected`, asserted `run_capture (serial) ==
// run_capture_parallel (M:N) == .expected`. Capture is now by REFERENCE: a closure shares the
// closest binding of a captured name and sees/makes writes to it. The one deliberate divergence from
// Go is F1 (a plain capture into a `spawn` is an isolated per-task copy, not shared+raced). -----

/// A1 — a closure reads a captured local by reference, seeing a write made after creation → `20`.
#[test]
fn golden_capture_outer_write() {
    let src = include_str!("../../examples/capture_outer_write.chz");
    let expected = include_str!("../../examples/capture_outer_write.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// A2 — a capturing (`defer:`) block WRITES a captured local; the write hits the shared cell (not a
/// phantom global), so the two increments are observed → `2`. (Fixes the old `emit_store` bug.)
#[test]
fn golden_capture_closure_write() {
    let src = include_str!("../../examples/capture_closure_write.chz");
    let expected = include_str!("../../examples/capture_closure_write.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// A3 — sibling capturing blocks share one captured local: three write it, one reads it, all through
/// the same cell → `3`.
#[test]
fn golden_capture_shared_counter() {
    let src = include_str!("../../examples/capture_shared_counter.chz");
    let expected = include_str!("../../examples/capture_shared_counter.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// B1 — a two-level nested closure shares a grandparent local (transitive capture via
/// `CapSrc::Captured`); reading it sees a later grandparent write through the shared cell → `11`.
#[test]
fn golden_capture_nested_grandparent() {
    let src = include_str!("../../examples/capture_nested_grandparent.chz");
    let expected = include_str!("../../examples/capture_nested_grandparent.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// B1 coverage — a tuple-destructured local is boxed when captured; a closure over it sees a later
/// write through the shared cell → `1` then `20`.
#[test]
fn golden_capture_destructure() {
    let src = include_str!("../../examples/capture_destructure.chz");
    let expected = include_str!("../../examples/capture_destructure.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// B1 coverage — a `match`-bound payload local captured by a closure in the arm boxes and escapes the
/// arm through the cell → `5`.
#[test]
fn golden_capture_match_bind() {
    let src = include_str!("../../examples/capture_match_bind.chz");
    let expected = include_str!("../../examples/capture_match_bind.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// C1 — a captured loop variable rebinds into a FRESH cell each iteration (Go >=1.22), so the three
/// pushed closures capture distinct values → `0`, `1`, `2` (not `2`, `2`, `2`).
#[test]
fn golden_capture_loop_var_fresh() {
    let src = include_str!("../../examples/capture_loop_var_fresh.chz");
    let expected = include_str!("../../examples/capture_loop_var_fresh.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// C2 — an accumulator declared OUTSIDE the loop is ONE shared cell (unlike the fresh-per-iteration
/// loop var), so per-iteration `defer:` writes accumulate → `6`.
#[test]
fn golden_capture_loop_accumulator() {
    let src = include_str!("../../examples/capture_loop_accumulator.chz");
    let expected = include_str!("../../examples/capture_loop_accumulator.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// E1 — a `defer:` block shares the enclosing binding by reference and runs at frame exit, so it
/// observes the LATEST value of a captured local, not its value at the defer point → `99`.
#[test]
fn golden_capture_defer_latest() {
    let src = include_str!("../../examples/capture_defer_latest.chz");
    let expected = include_str!("../../examples/capture_defer_latest.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// F3 / B2 (parity guard) — a closure capturing a by-reference local (a cell), invoked in a `spawn`
/// task via `spawn f()`, produces the same result on both engines → `7` (post-join, exact-match).
/// A capture-bearing closure crosses the task boundary by DEEP value on BOTH engines
/// (`do_spawn` → `cross_spawn_callee` → `wire_callable`/`from_wire`), matching the M:N
/// `prepare_worker`/`to_snap` deep-copy — so the task's `f` and its cells are isolated from the
/// parent's. See `golden_capture_spawn_closure_mutates_isolated` /
/// `golden_capture_spawn_closure_owner_write_isolated` for the mutation forms that make this
/// deep-cross load-bearing.
#[test]
fn golden_capture_cell_closure_into_spawn() {
    let src = include_str!("../../examples/capture_cell_closure_into_spawn.chz");
    let expected = include_str!("../../examples/capture_cell_closure_into_spawn.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N (cell-bearing closure crosses consistently)"
    );
}

/// F3 (mutation form) — the MEDIUM parity bug the first draft's F3 docstring wrongly declared
/// impossible. A closure `f := fn(): xs.push(2)` mutates its captured cell's INNER heap value (the
/// list) via a method call — the cell is never rebound, so "closures are expression-only" does not
/// save you. When `f` crosses `spawn f()` it must be a deep isolated copy on BOTH engines → the
/// parent's `xs` stays `[1]`. Before the `cross_spawn_callee` deep-cross, serial shared `f` by handle
/// and printed `[1, 2]` while M:N isolated and printed `[1]` (a real serial-vs-M:N divergence). The
/// print is post-join → exact-match.
#[test]
fn golden_capture_spawn_closure_mutates_isolated() {
    let src = include_str!("../../examples/capture_spawn_closure_mutates_isolated.chz");
    let expected = include_str!("../../examples/capture_spawn_closure_mutates_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected (isolated push → [1])");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N (capture-bearing closure crosses by deep value on both engines)"
    );
}

/// F3 (owner-write form) — the CRITICAL parity bug: a closure `f` READS a captured cell that the owner
/// MUTATES after `spawn f()`. Nesting `work()` under `main`'s `parallel:` forces the M:N eager
/// nested-nursery path (wires the task at spawn time). Before the fix, serial shared `f` and read the
/// post-write value (`5`) at join while M:N read the spawn-time snapshot (`0`); now `cross_spawn_callee`
/// snapshots the cell at spawn time on BOTH engines → `0`. `result` is a `Shared[int]` (crosses by
/// reference) so the task reports what it observed. Print is post-join → exact-match.
#[test]
fn golden_capture_spawn_closure_owner_write_isolated() {
    let src = include_str!("../../examples/capture_spawn_closure_owner_write_isolated.chz");
    let expected =
        include_str!("../../examples/capture_spawn_closure_owner_write_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(
        out, expected,
        "serial vs .expected (spawn-time snapshot → 0)"
    );
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N (owner write after spawn is invisible to the task's isolated cell)"
    );
}

/// F1 — THE ONE DELIBERATE DIVERGENCE FROM GO: a plain captured local sent into a `spawn` is
/// snapshot-copied into an independent per-task cell (the airlock deep-copies it), so the task's
/// `x = x + 1` mutates only its OWN copy — the parent's `x` stays `0` (Go shares+races → `1`). The
/// print is post-join, so serial == M:N == `0` exactly (not order-sensitive). This is the
/// memory-safety line; do NOT "fix" toward Go.
#[test]
fn golden_capture_spawn_isolated() {
    let src = include_str!("../../examples/capture_spawn_isolated.chz");
    let expected = include_str!("../../examples/capture_spawn_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(
        out, expected,
        "serial vs .expected (isolated → 0, NOT Go's 1)"
    );
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N (both isolate → 0)"
    );
}

/// B8 (defensive tripwire) — a boxed (captured) local must never surface as a raw cell in a
/// user-visible operation: `==`, string interpolation, and use as a `Map` key all `CellLoad` first,
/// yielding the VALUE. A missed `CellLoad` anywhere would show a cell handle here (or crash).
#[test]
fn capture_boxed_var_never_surfaces_as_cell() {
    let src = "\
fn main():
    x := 1
    f := fn() -> int: x
    print(x == 1)
    print(\"{x}\")
    m := Map()
    m[x] = \"a\"
    print(m[1])
    print(f())
main()";
    assert_parity_out(src, "true\n1\na\n1\n");
}

/// G1 — rebinding a captured HEAP variable (`xs = [9]`) routes through the shared cell, not a phantom
/// global, so the rebind is visible in the owner → `[9]`.
#[test]
fn golden_capture_rebind_heap() {
    let src = include_str!("../../examples/capture_rebind_heap.chz");
    let expected = include_str!("../../examples/capture_rebind_heap.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

/// F2 — cross-task shared mutation uses `Shared[T]` (crosses by reference); N tasks over one `Shared`
/// all accumulate. Uses `std.concurrency`, so it runs through the module graph.
#[test]
fn golden_capture_shared_across_tasks() {
    let src = include_str!("../../examples/capture_shared_across_tasks.chz");
    let expected = include_str!("../../examples/capture_shared_across_tasks.expected");
    let out = assert_parity_file(&[("main.chz", src)], "main.chz");
    assert_eq!(out, expected, "serial == M:N == .expected");
}

/// F4 — two tasks capture the SAME `Shared` counter and each bump it; confirms N-tasks-1-Shared
/// genuinely shares (unlike a plain captured local, which isolates — F1).
#[test]
fn golden_capture_shared_across_tasks_two() {
    let src = include_str!("../../examples/capture_shared_across_tasks_two.chz");
    let expected = include_str!("../../examples/capture_shared_across_tasks_two.expected");
    let out = assert_parity_file(&[("main.chz", src)], "main.chz");
    assert_eq!(out, expected, "serial == M:N == .expected");
}

/// D2 — recursion: each frame captures its OWN local into a fresh cell; the closure per frame reads
/// that frame's value, printed deepest-first as the recursion unwinds → `0\n1\n2\n3`. Matches Go.
#[test]
fn golden_capture_recursion_percall() {
    let src = include_str!("../../examples/capture_recursion_percall.chz");
    let expected = include_str!("../../examples/capture_recursion_percall.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

// ----- Nested `fn` decls are first-class local closures-with-a-name: lexical nearest-scope,
// recursive (letrec via cell), uniform by-reference capture — identical cell model to closures. -----

/// NF#4 — recursion: a nested fn calls ITSELF (letrec: its name is bound into its own cell before
/// the body captures it). `fact(5)` == `120` on both engines.
#[test]
fn nested_fn_recursion_parity() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1:
            return 1
        return n * fact(n - 1)
    print(fact(5))
main()";
    assert_parity_out(src, "120\n");
}

/// Identity-preserving airlock (`WireValue::Backref`): a self-recursive nested `fn` crossing the
/// airlock has a `Closure -> Cell -> Closure` self-cycle; the wire form assigns the Closure/Cell ids
/// and back-references the self-edge, and `from_wire` ties the knot back (placeholder-alloc → patch).
/// `fact(5) == 120` on both engines — previously this rejected ("recursive local fn cannot be sent").
#[test]
fn airlock_recursive_local_fn_round_trips_parity() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1:
            return 1
        return n * fact(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(ch.recv())
main()";
    assert_parity_out(src, "120\n");
}

/// Identity-preserving airlock — a MUTUALLY-recursive closure PAIR (`Cell_even -> Closure_even ->
/// Cell_odd -> Closure_odd -> Cell_even`, a 4-node cycle threaded through two Cells and two Closures)
/// crosses the airlock inside a spawned task's captured environment and ties BOTH directions of the
/// knot. `even(10) == true` on both engines. (Nested `fn`s can't forward-reference each other, so the
/// pair is built from reassigned fn-typed cells — the same closure/cell cycle a mutual `fn` would form.)
#[test]
fn airlock_mutually_recursive_pair_round_trips() {
    let src = "\
fn main():
    even := fn(n: int) -> bool: n == 0
    odd := fn(n: int) -> bool: n == 0
    even = fn(n: int) -> bool: n == 0 or odd(n - 1)
    odd = fn(n: int) -> bool: n != 0 and even(n - 1)
    ch := Channel[bool]()
    parallel:
        spawn: ch.send(even(10))
    print(ch.recv())
main()";
    assert_parity_out(src, "true\n");
}

/// Identity-preserving airlock — a recursive nested `fn` that ALSO reads an outer local (`base`). Its
/// capture graph has the self-cell (on the DFS stack → `Backref`) AND the `base`-cell (visited once,
/// OFF the stack → inline deep copy). The visited-set distinguishes them: the self-edge round-trips as
/// a cycle, the outer local as an independent copy. `f(3)` reads `base == 100` on both engines.
#[test]
fn airlock_recursive_closure_captures_outer_local_round_trips() {
    let src = "\
fn main():
    base := 100
    fn f(n: int) -> int:
        if n <= 0:
            return base
        return f(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(f(3))
    print(ch.recv())
main()";
    assert_parity_out(src, "100\n");
}

/// W7-4 (was `airlock_aliased_closure_stays_independent`, which asserted `1`): a list holding the SAME
/// closure twice (`[f, f]`, f closing over the mutable outer local `count`) crosses the airlock. The
/// closure VALUES are still two independent deep copies (the `Closure` arm keeps the back-edge-only,
/// pop-on-DFS-exit `path` discipline) — but the ONE BINDING they close over is now ONE cell on the far
/// side, because a cell is a binding's identity, not a value (`WireMemo::cells` is never popped). So
/// `pair[0]()` then `pair[1]()` reads `2`, matching the language's own sibling-closure sharing rule
/// (docs/syntax.md) and Go. Contrast `airlock_struct_dag_alias_stays_independent` below, which pins the
/// unchanged DATA rule: an acyclic DAG alias is still two independent copies.
#[test]
fn airlock_aliased_closure_shares_its_binding() {
    let src = "\
fn main():
    count := 0
    fn bump() -> int:
        count = count + 1
        return count
    pair := [bump, bump]
    r := Channel[int]()
    parallel:
        spawn:
            a := pair[0]()
            b := pair[1]()
            r.send(b)
    print(r.recv())
main()";
    assert_parity_out(src, "2\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `struct` (`a.next = [b]; b.next = [a]`,
/// a cycle threaded through struct fields + lists) crosses the airlock as a `spawn` argument and round-
/// trips (was `maximum structural depth exceeded`). The container arms now earn a `WireValue` id +
/// `Backref` exactly like `Cell`/`Closure`, so the cycle is preserved instead of overflowing the cap.
/// `use_it(a)` reads `a.val == 1` on both engines.
#[test]
fn airlock_self_ref_struct_round_trips_both() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
fn use_it(n: Node):
    print(\"got {n.val}\")
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    parallel:
        spawn use_it(a)
main()";
    assert_parity_out(src, "got 1\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `list` (`xs.push(xs)`, a list holding
/// itself) crosses a `Channel[List[List[int]]].send` and round-trips. The receiving task reads the
/// first element (`10`) and that the self-slot is the list itself (`ys[1].len() == 2`) on both engines.
#[test]
fn airlock_self_ref_list_round_trips_both() {
    let src = "\
fn main():
    xs: List[List[int]] = [[10]]
    xs.push(xs)
    ch := Channel[List[List[int]]]()
    ch.send(xs)
    parallel:
        spawn:
            ys := ch.recv()
            print(\"{ys[0][0]} {ys[1].len()}\")
main()";
    assert_parity_out(src, "10 2\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `map` (a map whose value refers back to
/// the map) crosses a `Channel` and round-trips using the CARRIED hash on reconstruction (never re-hashes
/// a cyclic key/value). The task reads the leaf entry's self-length on both engines.
#[test]
fn airlock_self_ref_map_round_trips_both() {
    let src = "\
fn main():
    m: Map[str, List[Map[str, int]]] = {\"leaf\": []}
    m[\"leaf\"].push(m)
    ch := Channel[Map[str, List[Map[str, int]]]]()
    ch.send(m)
    parallel:
        spawn:
            n := ch.recv()
            self_ref := n[\"leaf\"][0]
            depth := self_ref[\"leaf\"].len()
            print(\"{depth}\")
main()";
    assert_parity_out(src, "1\n");
}

/// FLIPPED (was `airlock_mixed_struct_closure_cycle_rejects_both`): a cycle passing through BOTH a
/// container (`struct` field) AND a `Closure` now ROUND-TRIPS. Once every container is identity-
/// preserved (its own id + `Backref`), the mixed cycle is no different from a pure `Cell`/`Closure`
/// cycle — the old `nonpreserved_depth` mixed-cycle reject (commit e8dcad7) is deleted as dead. The
/// closure `n.f = fn: n.x` closes `Closure -> Struct(n) -> Closure`; the task calls `n.f()` and reads
/// `n.x == 5` on both engines.
#[test]
fn airlock_mixed_struct_closure_cycle_round_trips_both() {
    let src = "struct Node:\n    f: fn() -> int\n    x: int\nfn main():\n    n := Node(fn() -> int: 0, 5)\n    n.f = fn() -> int: n.x\n    parallel:\n        spawn:\n            print(n.f())\nmain()\n";
    assert_parity_out(src, "5\n");
}

/// ADVERSARIAL, parity-blind (the item-2 lesson): a mutable `struct` appearing TWICE as an ACYCLIC
/// alias in a spawned payload must stay TWO INDEPENDENT deep copies — the back-edge-only (pop-on-DFS-
/// exit) memo discipline re-serializes an off-stack alias as an independent copy, never a shared node.
/// A shared-vs-duplicated node is stdout-identical on both engines, so this asserts INDEPENDENCE
/// explicitly: mutate one alias in the task, observe the other is UNAFFECTED (`9 1`, not `9 9`). Guards
/// against a future visited-set regression collapsing DAG aliases into one shared node (mirror
/// `airlock_aliased_closure_shares_its_binding` for the closure/BINDING path, which deliberately does
/// NOT share this rule). UNCHANGED by W7-4 — only `Obj::Cell` became persistent-memo.
#[test]
fn airlock_struct_dag_alias_stays_independent() {
    let src = "\
struct Box:
    n: int
fn main():
    box := Box(1)
    pair := [box, box]
    r := Channel[str]()
    parallel:
        spawn:
            p := pair
            p[0].n = 9
            r.send(\"{p[0].n} {p[1].n}\")
    print(r.recv())
main()";
    assert_parity_out(src, "9 1\n");
}

/// W7-4 fence for the SEAM the fix creates: `do_spawn`/`lower_task` now serialize the callee, ALL args
/// and the receiver under ONE `WireMemo` (so sibling closures keep their one binding). The same list
/// passed as TWO SEPARATE args must nonetheless stay TWO INDEPENDENT deep copies — the data-DAG rule
/// is per-serialization, not per-root, and only `Obj::Cell` is exempt. Mutating arg `a` in the task
/// must leave arg `b` untouched (`2 1`, not `2 2`). This is the exact case a careless
/// "share the whole memo for everything" widening would collapse.
#[test]
fn airlock_cross_arg_data_alias_stays_independent() {
    let src = "\
fn work(a: List[int], b: List[int], r: Channel[str]):
    a.push(2)
    r.send(\"{a.len()} {b.len()}\")
fn main():
    xs := [1]
    r := Channel[str]()
    parallel:
        spawn work(xs, xs, r)
    print(r.recv())
main()";
    assert_parity_out(src, "2 1\n");
}

/// NF#5 — capture READ (same task): a nested fn reads an outer local by reference; a write to that
/// local AFTER the fn is defined is visible when the fn is later called (shared cell) — matches
/// closure semantics → `42`.
#[test]
fn nested_fn_capture_read_sees_later_write_parity() {
    let src = "\
fn main():
    x := 10
    fn show():
        print(x)
    x = 42
    show()
main()";
    assert_parity_out(src, "42\n");
}

/// NF#6 — capture WRITE (same task): a nested fn body reassigns a captured outer local (`x = x + 1`);
/// the write is visible in the DEFINING scope (shared cell). Statement bodies make this expressible
/// (unlike expression-only closures) → `2`.
#[test]
fn nested_fn_capture_write_visible_in_owner_parity() {
    let src = "\
fn main():
    x := 0
    fn bump():
        x = x + 1
    bump()
    bump()
    print(x)
main()";
    assert_parity_out(src, "2\n");
}

/// NF#7 — loop-variable: a nested fn defined inside a `for` loop capturing the loop var gets a FRESH
/// cell per iteration (Go ≥1.22), exactly like a closure → `0\n1\n2`.
#[test]
fn nested_fn_loopvar_fresh_cell_parity() {
    let src = "\
fn main():
    fns := []
    for i in [0, 1, 2]:
        fn geti() -> int:
            return i
        fns.push(geti)
    for f in fns:
        print(f())
main()";
    assert_parity_out(src, "0\n1\n2\n");
}

/// NF#8 — spawn airlock (F1 shape): a nested fn capturing an outer local, invoked via `spawn bump()`
/// inside `parallel:`, is a capture-bearing `Obj::Closure` that crosses the airlock by DEEP value
/// (`cross_spawn_callee` on serial, `to_snap` on M:N) — its captured `x`-cell is snapshot-copied, so
/// the task's `x = x + 1` mutates its OWN isolated copy and the parent's `x` stays `0`. Identical to
/// the closure airlock isolation (F1). Print is post-join → exact-match, serial == M:N.
#[test]
fn nested_fn_spawn_airlock_isolated_parity() {
    let src = "\
fn main():
    x := 0
    fn bump():
        x = x + 1
    parallel:
        spawn bump()
    print(x)
main()";
    let out = run_capture(src).expect("serial run");
    assert_eq!(out, "0\n", "serial: nested fn capture isolated at airlock");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N (nested-fn closure crosses the airlock by deep value)"
    );
}

/// NF#7b — loop-variable capture at MODULE TOP LEVEL (not inside a `fn`): a nested fn defined in a
/// top-level `for` loop capturing the loop var gets a FRESH cell per iteration, exactly like the
/// in-function NF#7 case. The synthetic `<toplevel>` proto must compute its own boxed-name set (base
/// branch left it empty → the loop var stayed a raw int, and the captured `CellLoad` hit
/// `unreachable!("CellLoad on a non-handle value")`, panicking BOTH engines / check-OK-run-crash).
#[test]
fn nested_fn_toplevel_loopvar_fresh_cell_parity() {
    let src = "\
fns := []
for i in [0, 1, 2]:
    fn geti() -> int:
        return i
    fns.push(geti)
for f in fns:
    print(f())";
    assert_parity_out(src, "0\n1\n2\n");
}

/// NF#7c — the immediate-call top-level form (the minimal base-branch panic repro): a nested fn that
/// reads the top-level loop var and is CALLED in the same iteration. Must run, not crash.
#[test]
fn nested_fn_toplevel_loopvar_immediate_parity() {
    let src = "\
for i in [0, 1, 2]:
    fn geti() -> int:
        return i
    print(geti())";
    assert_parity_out(src, "0\n1\n2\n");
}

/// D1 — a captured local ESCAPES its defining frame: the heap cell outlives the frame, so a returned
/// closure still reads it; each factory call gets a fresh cell → `42\n7`. Matches Go's escaping upvalue.
#[test]
fn golden_capture_escape_reader() {
    let src = include_str!("../../examples/capture_escape_reader.chz");
    let expected = include_str!("../../examples/capture_escape_reader.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
    assert_eq!(
        out,
        run_capture_parallel(src).expect("parallel run"),
        "serial vs M:N"
    );
}

#[test]
fn capture_single_var_parity() {
    // 1-var capture: the single slot read via GetCaptured.
    let src = "\
fn make():
    n := 7
    return fn(x: int) -> int: x + n
fn main():
    f := make()
    print(f(3))
main()";
    assert_parity_out(src, "10\n");
}

#[test]
fn capture_multi_var_parity() {
    // Multiple captured vars get distinct slots (snapshot order); all read positionally.
    let src = "\
fn make():
    a := 1
    b := 2
    c := 3
    return fn(x: int) -> int: x + a + b + c
fn main():
    f := make()
    print(f(10))
main()";
    assert_parity_out(src, "16\n");
}

#[test]
fn capture_nested_closure_parity() {
    // Inner closure captures the ENCLOSING closure's captured var (CapSrc::Captured) —
    // the nested-slot-mapping path. `n` must reach the innermost body positionally.
    let src = "\
fn make():
    n := 100
    return fn(a: int): fn(b: int) -> int: a + b + n
fn main():
    outer := make()
    inner := outer(20)
    print(inner(3))
main()";
    assert_parity_out(src, "123\n");
}

#[test]
fn capture_deep_nested_three_levels_parity() {
    // Three levels of capture chaining — each level forwards an enclosing capture by slot.
    let src = "\
fn make():
    base := 1000
    return fn(a: int): fn(b: int): fn(c: int) -> int: base + a + b + c
fn main():
    f := make()(200)(30)
    print(f(4))
main()";
    assert_parity_out(src, "1234\n");
}

#[test]
fn capture_mutable_box_mutation_parity() {
    // A captured mutable heap box (a list), mutated after capture — the handle is captured by
    // value, mutation flows through the shared object. Two closures capture the SAME list at
    // distinct slots; the writer's append is visible through the reader's slot. (A shared mutable
    // box pattern without needing a file-path module import.)
    let src = "\
fn main():
    box := [0]
    bump := fn(): box.push(box.len())
    rd := fn() -> int: box.len()
    bump()
    bump()
    bump()
    print(rd())
    print(box)
main()";
    assert_parity_out(src, "4\n[0, 1, 2, 3]\n");
}

#[test]
fn capture_hof_callbacks_parity() {
    // Closures-with-captures as map/filter/fold callbacks (the HOF hot path for GetCaptured).
    let src = "\
fn main():
    base := 10
    xs := [1, 2, 3, 4]
    doubled := xs.map(fn(x: int) -> int: x * base)
    print(doubled)
    threshold := 2
    kept := xs.filter(fn(x: int) -> bool: x > threshold)
    print(kept)
    seed := 100
    total := xs.fold(seed, fn(acc: int, x: int) -> int: acc + x)
    print(total)
main()";
    assert_parity_out(src, "[10, 20, 30, 40]\n[3, 4]\n110\n");
}

#[test]
fn capture_hot_read_loop_parity() {
    // GetCaptured executed many times in a loop (the hot path post-refactor: pure index).
    let src = "\
fn make():
    step := 2
    return fn(x: int) -> int: x + step
fn main():
    f := make()
    total := 0
    i := 0
    while i < 1000:
        total = total + f(i)
        i = i + 1
    print(total)
main()";
    assert_parity_out(src, "501500\n");
}

#[test]
fn capture_closure_across_spawn_parity() {
    // A capturing closure sent across spawn/channel — exercises the --parallel deep-clone +
    // wire/snap rebuild of positional captures. VM (serial), interp, and --parallel must agree.
    let src = "\
fn main():
    base := 41
    ch := Channel[int]()
    parallel:
        spawn:
            ch.send(base + 1)
    print(ch.recv())
main()";
    assert_parity_out(src, "42\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "42\n",
        "--parallel capture wire"
    );
}

// ===== bytes type =====

#[test]
fn vm_bytes_ops() {
    // Index → int, negative index, reverse slice → bytes (repr), for-loop sum, len, ==/!=.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\\x03\"\n",
        "    print(b[0])\n",    // 1
        "    print(b[-1])\n",   // 3
        "    print(b[::-1])\n", // b'\x03\x02\x01'
        "    s := 0\n",
        "    for x in b:\n",
        "        s = s + x\n",
        "    print(s)\n",                  // 6
        "    print(b.len())\n",            // 3
        "    print(b\"ab\" == b\"ab\")\n", // true
        "    print(b\"ab\" == b\"ac\")\n", // false
        "    print(b\"ab\" != b\"ac\")\n", // true
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "1\n3\nb'\\x03\\x02\\x01'\n6\n3\ntrue\nfalse\ntrue\n"
    );
}

#[test]
fn vm_bytes_index_out_of_range_recoverable() {
    // An out-of-range index faults recoverably (catchable by `recover:`), like list/str.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\"\n",
        "    x := recover:\n",
        "        b[9]\n",
        "    match x:\n",
        "        Ok(v): print(v)\n",
        "        Err(e): print(\"caught\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "caught\n");
}

#[test]
fn vm_bytes_repr_and_map_key() {
    // Display/str repr is Python b'...'; bytes works as a map key (Hashable).
    let src = concat!(
        "fn main():\n",
        "    print(b\"hi\\n\")\n",      // b'hi\n'
        "    print(str(b\"\\xFF\"))\n", // b'\xff'
        "    m := {b\"a\": 1, b\"b\": 2}\n",
        "    print(m[b\"a\"])\n", // 1
        "    print(m[b\"b\"])\n", // 2
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "b'hi\\n'\nb'\\xff'\n1\n2\n");
}

#[test]
fn vm_bytes_slice_step_parity() {
    let src = "print(b\"\\x00\\x01\\x02\\x03\\x04\"[1:4:2])\nprint(b\"abc\"[1:])\n";
    assert_parity(src);
}

#[test]
fn bytes_crosses_channel() {
    // `bytes` is fully value-typed and sendable — it crosses the --parallel airlock via
    // WireValue::Bytes and reconstructs as a fresh heap `bytes` (Python b'...' repr preserved).
    // Buffer-then-drain (send before recv) so all THREE engines agree — the sequential interp
    // cannot block a consumer mid-flight on a live producer (a C5 limitation, not bytes-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytes]()\n",
        "    parallel:\n",
        "        spawn ch.send(b\"\\x01\\x02\")\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    // Three-engine parity: cooperative VM, --parallel M:N, and interp all agree.
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "b'\\x01\\x02'\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "b'\\x01\\x02'\n"
    );
}

// ===== bytearray type (mutable sibling of bytes) =====

#[test]
fn vm_bytearray_ops() {
    // Constructors (all 4 forms), index read, slice -> bytearray (incl. reverse), for-loop sum,
    // len, push, pop, extend, ==/!=, and Display bytearray(b'...').
    let src = concat!(
        "fn main():\n",
        "    print(bytearray())\n",                // bytearray(b'')
        "    print(bytearray(3))\n",               // bytearray(b'\x00\x00\x00')
        "    print(bytearray(b\"\\x01\\x02\"))\n", // bytearray(b'\x01\x02')
        "    ba := bytearray([1, 2, 3])\n",
        "    print(ba)\n",       // bytearray(b'\x01\x02\x03')
        "    print(ba[0])\n",    // 1
        "    print(ba[-1])\n",   // 3
        "    print(ba[::-1])\n", // bytearray(b'\x03\x02\x01')
        "    s := 0\n",
        "    for x in ba:\n",
        "        s = s + x\n",
        "    print(s)\n",        // 6
        "    print(ba.len())\n", // 3
        "    ba.push(4)\n",
        "    print(ba)\n",       // bytearray(b'\x01\x02\x03\x04')
        "    print(ba.pop())\n", // Some(4)
        "    ba.extend(b\"\\xFF\")\n",
        "    ba.extend([7, 8])\n",
        "    print(ba)\n", // bytearray(b'\x01\x02\x03\xff\x07\x08')
        "    print(bytearray([1]) == bytearray([1]))\n", // true
        "    print(bytearray([1]) != bytearray([2]))\n", // true
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "bytearray(b'')\nbytearray(b'\\x00\\x00\\x00')\nbytearray(b'\\x01\\x02')\nbytearray(b'\\x01\\x02\\x03')\n1\n3\nbytearray(b'\\x03\\x02\\x01')\n6\n3\nbytearray(b'\\x01\\x02\\x03\\x04')\nSome(4)\nbytearray(b'\\x01\\x02\\x03\\xff\\x07\\x08')\ntrue\ntrue\n"
    );
}

#[test]
fn vm_bytearray_index_write_and_shared_mutation() {
    // ba[i] = x mutates in place; a second binding observes it (proves in-place, not copy).
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    ba2 := ba\n",
        "    ba[0] = 65\n",
        "    print(ba2[0])\n", // 65 — same buffer
        "    ba2[1] = 66\n",
        "    print(ba[1])\n", // 66 — observed through the other binding
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "65\n66\n");
}

#[test]
fn vm_bytearray_oob_and_value_range_recoverable() {
    // An out-of-range index OR an out-of-range value (not 0..=255) is a recoverable fault.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2])\n",
        "    r1 := recover:\n",
        "        ba[9] = 1\n",
        "    match r1:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught oob index\")\n",
        "    r2 := recover:\n",
        "        ba[0] = 999\n",
        "    match r2:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught bad value\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "caught oob index\ncaught bad value\n"
    );
}

#[test]
fn vm_bytearray_huge_size_is_recoverable_not_abort() {
    // `bytearray(N)` for an absurd N must fault recoverably (try_reserve), NOT abort the process
    // (SIGABRT) uncatchably — the language's recoverable-fault invariant (cf. range()'s cap).
    let src = concat!(
        "fn main():\n",
        "    r := recover:\n",
        "        bytearray(9999999999999)\n",
        "    match r:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught huge\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "caught huge\n");
}

#[test]
fn vm_bytearray_conversion_bridge() {
    // bytes(ba) -> immutable snapshot; bytearray(b) -> mutable copy; round-trip + independence.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    b := bytes(ba)\n",
        "    print(b)\n", // b'\x01\x02\x03'
        "    ba[0] = 99\n",
        "    print(b)\n", // b'\x01\x02\x03' — snapshot unaffected
        "    ba2 := bytearray(b\"\\x07\\x08\")\n",
        "    ba2[0] = 10\n",
        "    print(ba2)\n",          // bytearray(b'\n\x08') — 0x0a renders as \n
        "    print(bytearray(b))\n", // bytearray(b'\x01\x02\x03')
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "b'\\x01\\x02\\x03'\nb'\\x01\\x02\\x03'\nbytearray(b'\\n\\x08')\nbytearray(b'\\x01\\x02\\x03')\n"
    );
}

#[test]
fn bytearray_crosses_channel_deep_copy() {
    // `bytearray` crosses the --parallel airlock by VALUE (deep copy): the other side gets a
    // fresh independent buffer. Buffer-then-drain (send before recv) so the sequential interp
    // agrees, exactly like bytes_crosses_channel (a C5 limitation, not bytearray-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytearray]()\n",
        "    parallel:\n",
        "        spawn ch.send(bytearray([1, 2]))\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "bytearray(b'\\x01\\x02')\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "bytearray(b'\\x01\\x02')\n"
    );
}

// ===== Iterable[T] / `.iter()` cursor =====

#[test]
fn iter_next_idempotent_both_engines() {
    // next() yields Some(10), Some(20), then None forever (idempotent past exhaustion).
    let src = "fn main():\n    it := [10, 20].iter()\n    print(it.next())\n    print(it.next())\n    print(it.next())\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "Some(10)\nSome(20)\nNone\nNone\n");
}

#[test]
fn iter_empty_collection_none_immediately() {
    let src =
        "fn main():\n    xs: List[int] = []\n    it := xs.iter()\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "None\n");
}

#[test]
fn iter_snapshot_order_matches_for() {
    // For each collection, the cursor's element sequence must equal `for x in X`.
    for coll in [
        "[1, 2, 3]",
        "{1, 2, 3}",
        "{1: \"a\", 2: \"b\"}", // map → keys
        "\"abc\"",
        "b\"hi\"",
        "bytearray([7, 8])",
    ] {
        let via_for = format!("fn main():\n    for x in {coll}:\n        print(x)\nmain()\n");
        let via_iter = format!(
            "fn main():\n    it := ({coll}).iter()\n    while true:\n        match it.next():\n            Some(x):\n                print(x)\n            None:\n                break\nmain()\n"
        );
        assert_parity(&via_for);
        assert_parity(&via_iter);
        let for_out = vm_outcome(&via_for);
        let iter_out = vm_outcome(&via_iter);
        assert_eq!(
            for_out, iter_out,
            "cursor order must match `for` for {coll}"
        );
    }
}

#[test]
fn iter_self_on_iterator_value() {
    // iter() on a cursor returns self (idempotent); driving the result still works.
    let src = "fn main():\n    it := [1, 2].iter().iter()\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "Some(1)\n");
}

#[test]
fn list_of_cursor_roundtrip_both_engines() {
    assert_parity_out(
        "fn main():\n    print(List([5, 6, 7].iter()))\nmain()\n",
        "[5, 6, 7]\n",
    );
    assert_parity_out(
        "fn main():\n    print(Set({1, 2}.iter()).len())\nmain()\n",
        "2\n",
    );
}

#[test]
fn cursor_composes_into_adapter() {
    // The headline win: a list cursor flows into a `[I: Iterator[T]]` adapter.
    let src = concat!(
        "struct Take[I: Iterator[T], T]:\n",
        "    inner: I\n",
        "    left: int\n",
        "    fn next(self) -> Option[T]:\n",
        "        if self.left <= 0:\n",
        "            return None\n",
        "        self.left = self.left - 1\n",
        "        return self.inner.next()\n",
        "fn main():\n",
        "    for v in Take([10, 20, 30, 40].iter(), 2):\n",
        "        print(v)\n",
        "main()\n"
    );
    assert_parity_out(src, "10\n20\n");
}

#[test]
fn for_over_pure_iterable_struct() {
    // A struct with ONLY iter() drives `for`; one with BOTH still uses next() (sentinel).
    let only_iter = concat!(
        "struct Wrap:\n",
        "    xs: List[int]\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return self.xs.iter()\n",
        "fn main():\n",
        "    for x in Wrap([1, 2, 3]):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_parity_out(only_iter, "1\n2\n3\n");
    // Both iter() and next(): next() wins (iter() would print a different sentinel).
    let both = concat!(
        "struct Two:\n",
        "    n: int\n",
        "    fn next(self) -> Option[int]:\n",
        "        if self.n >= 2:\n",
        "            return None\n",
        "        v := self.n\n",
        "        self.n = self.n + 1\n",
        "        return Some(v + 100)\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return [999].iter()\n",
        "fn main():\n",
        "    for x in Two(0):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_parity_out(both, "100\n101\n");
}

// ---- Bug C: a `for`/`List()`/`Set()` over a NAMED builtin cursor consumes it in place ----
// (must match `.next()` and struct iterators and docs/syntax.md:709-713).

#[test]
fn for_named_cursor_partial_consume_then_drain() {
    // The repro: partial-consume via a broken `for`, then `List()` drains the REMAINDER.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3, 4].iter()\n",
        "    seen := 0\n",
        "    for x in it:\n",
        "        seen = seen + 1\n",
        "        if x == 2:\n",
        "            break\n",
        "    rest := List(it)\n",
        "    print(\"seen={seen} rest={rest}\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap().trim(), "seen=2 rest=[3, 4]");
    assert_parity(src);
}

#[test]
fn for_named_cursor_two_pass_second_yields_nothing() {
    // A named cursor is consumed by the first `for`; the second pass yields nothing.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    for x in it:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n");
    assert_parity(src);
}

#[test]
fn next_after_for_over_named_cursor_is_none() {
    // `for` advances the shared cursor; a trailing `.next()` sees it exhausted.
    let src = concat!(
        "fn main():\n",
        "    it := [10, 20].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "10\n20\nNone\n");
    assert_parity(src);
}

#[test]
fn for_over_collection_reiterates_fully_twice() {
    // Invariant 1: a NON-cursor collection (list) keeps fresh-snapshot semantics — both passes full.
    let src = concat!(
        "fn main():\n",
        "    xs := [1, 2, 3]\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n1\n2\n3\n");
    assert_parity(src);
}

#[test]
fn iter_of_iter_fresh_cursor() {
    // Invariant 2: `xs.iter().iter()` is one fresh cursor (no double-advance).
    let src = concat!(
        "fn main():\n",
        "    a := [5, 6].iter().iter()\n",
        "    print(a.next())\n",
        "    for x in a:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "Some(5)\n6\n");
    assert_parity(src);
}

#[test]
fn for_over_fresh_temp_cursor_full() {
    // Invariant 3: a fresh unnamed temp cursor still fully iterates.
    let src = concat!(
        "fn main():\n",
        "    for x in [7, 8, 9].iter():\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "7\n8\n9\n");
    assert_parity(src);
}

#[test]
fn cursor_crosses_spawn_airlock_three_engine_parity() {
    // A cursor IS sendable: it crosses the spawn/channel airlock as a DEEP COPY (independent
    // snapshot + pos on the receiver), like a `list`. The receiver drives it and gets `Some(1)`.
    // This must hold byte-identically on ALL THREE engines — the cooperative VM, the M:N
    // `--parallel` engine, and the interpreter (whose `deep_clone` already deep-copies a cursor).
    // (Regression guard: an earlier cut gated the cursor non-sendable like a generator, which
    // panicked the spawned worker on the VM while the interp succeeded — a parity divergence.)
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    parallel:\n",
        "        spawn ch.send([1, 2].iter())\n",
        "    print(ch.recv().next())\n",
        "main()\n"
    );
    assert_parity_out(src, "Some(1)\n");
    assert_eq!(
        run_capture_parallel(src).expect("--parallel"),
        "Some(1)\n",
        "cursor crosses the M:N airlock"
    );
}

#[test]
fn generator_iter_returns_self_vm() {
    // VM-only (interp has no generators): a generator's iter() returns self and drives.
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "    yield 2\n",
        "fn main():\n",
        "    it := gen().iter()\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("vm"), "Some(1)\nSome(2)\nNone\n");
}

// ===== one-way int→float implicit widening (Architecture C: real runtime coercion) =====

/// Assert BOTH engines (serial VM + M:N VM) produce `want`. (The tree-walk interp is gone; this used
/// to call the M:N engine twice under a stale "interp" label.)
fn widen_three_engines(src: &str, want: &str) {
    assert_eq!(run_capture(src).expect("serial"), want, "serial engine");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        want,
        "M:N engine"
    );
}

/// A `float`-annotated let binding stores a genuine `f64` (display `3.0`), and `x / 2` is FLOAT
/// division (`1.5`), NOT int division (`1`). The division is the load-bearing semantic proof.
#[test]
fn widen_let_display_and_division() {
    widen_three_engines("x: float = 3\nprint(x)\nprint(x / 2)\n", "3.0\n1.5\n");
}

/// Passing an untyped int CONSTANT EXPRESSION into a `float` param coerces at the callee PROLOGUE
/// (nothing folds `1 + 2`, so an `Int` reaches the callee and `Op::CoerceFloat` converts it): `z / 2`
/// is float division. Proves the coercion is at the callee boundary, not the call site — which is why
/// it must stay for fn-values/closures/methods. An explicit `float(a)` of a TYPED int (the only way
/// to pass one now) lands as an f64 too.
#[test]
fn widen_param_int_variable_division() {
    widen_three_engines("fn f(z: float):\n    print(z / 2)\nf(1 + 2)\n", "1.5\n");
    widen_three_engines(
        "fn f(z: float):\n    print(z / 2)\na := 3\nf(float(a))\n",
        "1.5\n",
    );
}

/// An untyped int CONSTANT EXPRESSION returned from a `-> float` function is coerced before `Return`.
/// (A TYPED int expression — `n + 1` with `n: int` — is now a CHECK ERROR; see
/// checker::tests::widen_int_return_into_float_ret_accepted.)
#[test]
fn widen_return_nonliteral_int_expr() {
    widen_three_engines(
        "fn g() -> float:\n    return 1 + 2\nprint(g() / 2)\n",
        "1.5\n",
    );
    widen_three_engines(
        "fn g(n: int) -> float:\n    return float(n + 1)\nprint(g(2) / 2)\n",
        "1.5\n",
    );
}

/// An int field value widens into a `float` struct field (per-field coercion at `NewStruct`).
#[test]
fn widen_struct_float_field_division() {
    widen_three_engines(
        "struct P:\n    v: float\np := P(3)\nprint(p.v / 2)\n",
        "1.5\n",
    );
}

/// An int DEFAULT value widens into a `float` param: omitted (`g()` → spliced default coerced at
/// the prologue) AND explicit int (`g(5)`) both store a genuine f64.
#[test]
fn widen_default_param_division() {
    widen_three_engines(
        "fn g(a: float = 3) -> float:\n    return a / 2\nprint(g())\nprint(g(5))\n",
        "1.5\n2.5\n",
    );
}

/// An inline-expr fn body (`fn g() -> float: 1 + 2`) coerces its implicit return too.
#[test]
fn widen_inline_expr_body_return() {
    widen_three_engines("fn g() -> float: 1 + 2\nprint(g() / 2)\n", "1.5\n");
}

/// A `float`-param closure coerces at its prologue.
#[test]
fn widen_closure_float_param_division() {
    widen_three_engines("f := fn(z: float): z / 2\nprint(f(3))\n", "1.5\n");
}

/// (A) Annotated `List[float] = [1, 2.3]` — `xs[0]` is a genuine float (`1 / 2 == 0.5`).
/// (B) Un-annotated all-literal mix `[1, 2.3]` widens its int LITERAL via the peephole.
/// (C) A map VALUE float position likewise widens.
#[test]
fn widen_collection_annotated_and_literal() {
    widen_three_engines("xs: List[float] = [1, 2.3]\nprint(xs[0] / 2)\n", "0.5\n");
    widen_three_engines(
        "ys := [1, 2.3]\nprint(ys[0] / 2)\nprint(ys[1])\n",
        "0.5\n2.3\n",
    );
    widen_three_engines(
        "m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\nprint(m[\"a\"] / 2)\n",
        "0.5\n",
    );
}

/// An all-int literal collection must NOT widen (the peephole only fires when ≥1 float literal is
/// present): `[1, 2, 3]` stays `List[int]`, so `xs[0] / 2` is int division (`0`).
#[test]
fn widen_all_int_literal_collection_stays_int() {
    widen_three_engines("xs := [1, 2, 3]\nprint(xs[0] / 2)\n", "0\n");
}

/// Regression-pin: mixed int/float COMPARISONS already widen at runtime; ensure no double-coerce
/// or divergence after the new coercion ops.
#[test]
fn widen_mixed_comparisons_pinned() {
    widen_three_engines("print(1 < 2.3)\nprint(1 == 2.3)\n", "true\nfalse\n");
}

/// An ANNOTATED `List[float]` licenses an untyped int CONSTANT element even when the float sibling is
/// a VARIABLE (the literal peephole cannot see it — only the annotation hint can). The element must
/// land as a genuine f64: `xs[0] / 2 == 0.5`, and `.sort()` sorts as floats.
/// (A TYPED int element — `xs: List[float] = [a, 2.3]` — is now a CHECK ERROR: an annotation is a
/// type CONTEXT for a constant, not a conversion for a typed value. See
/// checker::tests::widen_let_hint_does_not_leak_into_nested_literal / the V1 tests.)
#[test]
fn widen_annotated_list_const_int_float_var_runs() {
    widen_three_engines(
        "f := 2.5\nxs: List[float] = [1, f]\nprint(xs[0] / 2)\nxs.sort()\nprint(xs)\n",
        "0.5\n[1.0, 2.5]\n",
    );
}

/// An untyped int CONSTANT EXPRESSION element (`1 + 1`, not a bare literal) is coerced by the literal
/// peephole — the checker accepts it, so the compiler MUST widen it (else a fresh Int-under-float).
#[test]
fn widen_const_int_expr_element_coerced() {
    widen_three_engines("xs := [1 + 1, 2.5]\nprint(xs[0] / 2)\n", "1.0\n");
}

/// A UNARY / BINARY untyped FLOAT-constant sibling licenses the peephole too (`-2.5`, `2.0 + 0.5` are
/// not `ExprKind::Float` literals). Both were pre-existing Int-under-float leaks (printed `0`).
#[test]
fn widen_unary_float_sibling_coerced() {
    widen_three_engines("xs := [1, -2.5]\nprint(xs[0] / 2)\n", "0.5\n");
    widen_three_engines("xs := [1, 2.0 + 0.5]\nprint(xs[0] / 2)\n", "0.5\n");
    widen_three_engines(
        "m := {\"a\": 1, \"b\": -2.5}\nprint(m[\"a\"] / 2)\n",
        "0.5\n",
    );
}

// ===== Generic fn as a VALUE (scope A + B) — runtime is generic-ERASED, so serial == M:N is
// automatic; a RUN test on BOTH engines is still required per accepted case (the bind-import trap:
// a checker-accept that faults at runtime).

#[test]
fn generic_fn_value_turbofish_parity() {
    // B — turbofish on a fn value.
    assert_parity_out(
        "fn ident[T](x: T) -> T:\n    return x\n\ng := ident[int]\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_annot_parity() {
    // A1 — annotated binding.
    assert_parity_out(
        "fn ident[T](x: T) -> T:\n    return x\n\ng: fn(int) -> int = ident\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_hofarg_parity() {
    // A2 — HOF argument.
    assert_parity_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn applyit(f: fn(int) -> int, x: int) -> int:\n    return f(x)\n\nprint(applyit(ident, 5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_return_parity() {
    // A3 — return position.
    assert_parity_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn getf() -> fn(int) -> int:\n    return ident\n\ng := getf()\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_name_shadowed_local_index_parity() {
    // Compiler-erase shadow-safety: a fn-local binding that shadows a top-level generic fn name is a
    // REAL index (not an erased turbofish) on BOTH engines. (The shadow must be a fn-LOCAL — a
    // top-level `ident := …` would collide with `fn ident`, which the checker rejects.)
    assert_parity_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn h():\n    ident := [10, 20, 30]\n    print(ident[1])\n\nh()\n",
        "20\n",
    );
}

// ===== Finding D — free-variable capture (closures/nested-fns capture only referenced names) =====
// Pre-fix bug: every MakeClosure site captured ALL visible locals via snapshot_entries(), dragging an
// unused non-sendable sibling (a closure value / live generator) across the spawn airlock → check-OK
// but run-fault ("spawn: this task value can't cross a worker boundary yet"). Fix: capture only the
// body's free-variable set. These parity tests prove the fix AND no under-capture (both engines).

#[test]
fn d_repro_unused_sibling_nested_fn_parity() {
    // The exact #D repro: unused non-sendable sibling closure value; task is a nested fn using only g.
    assert_parity_out(
        "g := 7\nfn main():\n    sibling := fn(x: int): x + 1\n    fn task():\n        print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_repro_unused_sibling_closure_value_parity() {
    // Same, but the spawned task is a CLOSURE VALUE (site 1: compile_closure).
    assert_parity_out(
        "g := 7\nfn main():\n    sibling := fn(x: int): x + 1\n    task := fn(): print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_repro_unused_generator_sibling_parity() {
    // The unused non-sendable sibling is a live GENERATOR (frame-holding, non-sendable); task uses only g.
    assert_parity_out(
        "g := 7\nfn gen() -> Iterator[int]:\n    yield 1\nfn main():\n    it := gen()\n    fn task():\n        print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_used_sibling_closure_crosses_by_value_parity() {
    // B3.3: the task ACTUALLY captures + uses a sibling CLOSURE (`inc`). This used to fault at the
    // airlock (a closure lowered to a by-reference `Handle`); as of B3.3 closures cross the airlock BY
    // VALUE, so the captured `inc` is deep-copied into the task and `inc(7)` runs → prints 8 on both
    // engines. (The Finding-D free-var filter is still exercised: only `inc` is captured.) A residual
    // non-sendable value (a live generator) still faults — see
    // `generator_captured_into_spawn_callee_still_faults`.
    assert_parity_out(
        "g := 7\nfn main():\n    inc := fn(x: int): x + 1\n    fn task():\n        print(inc(g))\n    parallel:\n        spawn task()\nmain()\n",
        "8\n",
    );
}

#[test]
fn d_spawn_block_free_capture_parity() {
    // Site 4: `spawn:` block references one outer local while an unused non-sendable sibling is in scope.
    assert_parity_out(
        "fn main():\n    sibling := fn(x: int): x + 1\n    n := 5\n    parallel:\n        spawn:\n            print(n)\nmain()\n",
        "5\n",
    );
}

#[test]
fn d_defer_block_free_capture_parity() {
    // Site 5: `defer:` block references one outer local while an unused non-sendable sibling is in scope.
    assert_parity_out(
        "fn main():\n    sibling := fn(x: int): x + 1\n    n := 9\n    defer:\n        print(n)\n    print(\"body\")\nmain()\n",
        "body\n9\n",
    );
}

#[test]
fn defer_block_q_discards_fired_err_parity() {
    // The `?`-in-defer contract is DISCARD (the block is its own closure — `syntax.md`). A FIRED
    // Err short-circuits the block and is dropped: the tail `print` never runs, the enclosing
    // nil-returning fn returns normally, and both engines agree. (The checker fix that made this
    // program compile under a nil-returning fn — F1 — must not change the runtime discard.)
    assert_parity_out(
        "fn g() -> int!:\n    return Err(\"x\")\nfn f():\n    defer:\n        v := g()?\n        print(\"never {v}\")\n    print(\"body\")\nf()\nprint(\"done\")\n",
        "body\ndone\n",
    );
    // An Ok value flows past `?` into the rest of the cleanup body:
    assert_parity_out(
        "fn g() -> int!:\n    return Ok(7)\nfn f():\n    defer:\n        v := g()?\n        print(\"got {v}\")\n    print(\"body\")\nf()\nprint(\"done\")\n",
        "body\ngot 7\ndone\n",
    );
}

#[test]
fn d_no_undercapture_method_call_on_captured_receiver_parity() {
    // A method call on a captured receiver (outer local `xs`) inside a nested fn.
    assert_parity_out(
        "fn main():\n    xs := [1, 2]\n    fn add():\n        xs.push(3)\n    add()\n    print(xs)\nmain()\n",
        "[1, 2, 3]\n",
    );
}

#[test]
fn d_no_undercapture_interpolation_ref_parity() {
    // A local referenced ONLY inside string interpolation must still be captured.
    assert_parity_out(
        "fn main():\n    n := 42\n    f := fn(): print(\"n={n}\")\n    f()\nmain()\n",
        "n=42\n",
    );
}

#[test]
fn d_no_undercapture_grandparent_through_two_closures_parity() {
    // Transitive capture: a grandparent local referenced only by an inner nested fn (two levels
    // deep) must surface through the middle frame's free set.
    assert_parity_out(
        "fn main():\n    g := 100\n    fn outer():\n        fn inner():\n            print(g)\n        inner()\n    outer()\nmain()\n",
        "100\n",
    );
}

#[test]
fn d_no_undercapture_recursion_self_ref_parity() {
    // LETREC: the recursive self-name is free in its own body → stays captured → self-call resolves.
    assert_parity_out(
        "fn main():\n    fn fact(n: int) -> int:\n        if n <= 1:\n            return 1\n        return n * fact(n - 1)\n    print(fact(5))\nmain()\n",
        "120\n",
    );
}

#[test]
fn d_no_undercapture_ref_capture_mutate_parity() {
    // A nested fn that reads AND mutates a captured local by reference.
    assert_parity_out(
        "fn main():\n    c := 0\n    fn bump():\n        c = c + 1\n    bump()\n    bump()\n    print(c)\nmain()\n",
        "2\n",
    );
}

#[test]
fn d_no_undercapture_match_and_comprehension_parity() {
    // A match-bind and a comprehension over a captured collection inside a nested fn.
    assert_parity_out(
        "fn main():\n    xs := [1, 2, 3]\n    fn work():\n        doubled := [x * 2 for x in xs]\n        print(doubled)\n    work()\nmain()\n",
        "[2, 4, 6]\n",
    );
}

#[test]
fn d_no_undercapture_nonrecursive_nested_fn_parity() {
    // A NON-recursive nested fn: its own name is NOT free in its body → dropped from captures
    // harmlessly; must still bind and run.
    assert_parity_out(
        "fn main():\n    fn greet():\n        print(\"hi\")\n    greet()\nmain()\n",
        "hi\n",
    );
}

// ===== B3.3 (runtime) — closures / bare funcs cross the spawn/Channel airlock BY VALUE =====
// The generic `to_wire`/`to_snap` lowering crosses a closure/nested-fn/bare-func as data (its proto
// + wired captures + home index), never a by-reference `Handle`. A `spawn f()` callee whose captured
// environment contains a NESTED closure (or is a bare fn) now runs on both engines identically,
// instead of the old "task value can't cross a worker boundary" airlock fault. The checker's
// Func-non-sendable gate on Channel ELEMENT types is a separate follow-up and still stands.

#[test]
fn closure_as_data_into_spawn_callee_parity() {
    // `work` is a closure whose captured env holds another closure (`double`). Crossing the spawn
    // callee airlock used to fault (the nested closure lowered to a `Handle`); it now crosses by value.
    let src = "\
fn main():
    double := fn(x: int) -> int: x * 2
    ch := Channel[int]()
    work := fn(): ch.send(double(21))
    parallel:
        spawn work()
    print(ch.recv())
main()";
    assert_parity_out(src, "42\n");
}

#[test]
fn nestedfn_as_data_into_spawn_callee_parity() {
    // Same shape, but with nested `fn` declarations rather than closure values.
    let src = "\
fn main():
    fn double(x: int) -> int:
        return x * 2
    ch := Channel[int]()
    fn work():
        ch.send(double(21))
    parallel:
        spawn work()
    print(ch.recv())
main()";
    assert_parity_out(src, "42\n");
}

#[test]
fn generator_captured_into_spawn_callee_crosses_by_value() {
    // F3 path C: a spawn callee capturing a LOCAL generator (`it`, Pending) crosses it BY VALUE —
    // the task drives its own copy. The module global `g := 7` is a non-generator, unrelated to the
    // crossing. serial == M:N, byte-identical.
    let src = "\
g := 7
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    fn task():
        for x in it:
            print(x)
    parallel:
        spawn task()
main()";
    assert_eq!(vm_outcome(src).expect("serial"), "1\n");
    assert_eq!(parallel_outcome(src).expect("M:N"), "1\n");
}

// ----- F3 path C: a frame-holding generator crosses the airlock BY VALUE (deep copy) -----
// A generator (whether held in a frame LOCAL or a MODULE GLOBAL — see the item-B tests below) is
// serialized by `to_wire`/`from_wire` and rebuilt on the receiving heap as an INDEPENDENT copy. Every
// parked slot is checked sendable at serialize time, so a non-sendable slot still rejects at the
// crossing. All tests assert serial (`--serial` oracle) == M:N.

/// A PENDING generator (never driven) captured into a `spawn:` block crosses by value; the receiving
/// task drives it fully, and the sender's copy is INDEPENDENT (deep-copy: sender also drives the full
/// sequence). Byte-identical on serial and M:N.
#[test]
fn generator_pending_sent_into_spawn_deep_copy_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
fn main():
    it := gen()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "1\n2\n3\n0\n1\n2\n3\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A SUSPENDED generator (driven once → one parked frame) captured into a `spawn:` block crosses by
/// value; the task drives the REMAINDER, and the sender's copy independently drives its own remainder
/// (deep-copy). Byte-identical on serial and M:N.
#[test]
fn generator_suspended_sent_keeps_deep_copy_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "2\n3\n0\n2\n3\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A CLOSURE-backed generator (a nested `fn` capturing a local) crosses by value: `to_wire` wires the
/// generator's backing closure (carrying the captured `base`) and `from_wire` rebuilds it, so the
/// received copy yields the captured-relative values independently of the sender. Covers the
/// `closure = Some(..)` serialize/rebuild arm. serial == M:N.
#[test]
fn generator_closure_backed_sent_deep_copy_both() {
    let src = "\
fn main():
    base := 100
    fn gen() -> Iterator[int]:
        yield base + 1
        yield base + 2
    it := gen()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "101\n102\n0\n101\n102\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A generator crossing over an explicit `Channel[Iterator[int]]` into a task: the parent SENDS it
/// (deep-copied into the channel), the task RECVs its own copy and drives it, and the sender's `it`
/// is untouched (deep-copy). Byte-identical on serial and M:N.
#[test]
fn generator_sent_over_channel_into_task_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 10
    yield 20
fn main():
    gench := Channel[Iterator[int]]()
    out := Channel[int]()
    it := gen()
    gench.send(it)
    parallel:
        spawn:
            g := gench.recv()
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "10\n20\n10\n20\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A suspended generator whose PARKED SLOT holds a non-sendable value rejects cleanly at the crossing —
/// the per-slot sendable-at-serialize-time check walks the parked stack. The witness is a >10000-DEEP
/// ACYCLIC nested list (`keep = [keep]` in a loop), which trips the shared `MAX_STRUCTURAL_DEPTH` guard
/// in `to_wire_depth` — a to_wire-TIME fault that fires byte-identically on BOTH engines. (A CYCLIC
/// struct is NO LONGER a valid witness — self-referential data now round-trips via `WireValue::Backref`;
/// see `airlock_self_ref_struct_round_trips_both`. A genuinely-unbounded ACYCLIC nest is the remaining
/// both-engines-identical serialize-time reject: the depth cap stays as that backstop. A recursive local
/// `fn` is also sendable now — see `generator_carrying_recursive_closure_round_trips_both` below.)
#[test]
fn generator_parked_slot_nonsendable_rejects_both() {
    let src = "\
fn gen() -> Iterator[int]:
    keep: List[int] = []
    deep: List[List[int]] = [keep]
    for i in 0..10001:
        deep = [deep]
    yield 1
    yield deep.len()
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
main()";
    let ve = vm_outcome(src).expect_err("serial: non-sendable parked slot must reject");
    let pe = parallel_outcome(src).expect_err("M:N: non-sendable parked slot must reject");
    assert!(ve.contains("maximum structural depth"), "serial: {ve}");
    assert!(pe.contains("maximum structural depth"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// REGRESSION (silent generator DUPLICATION, the e8dcad7 wrong-result class): a value CYCLE that passes
/// through BOTH an identity-preserved container AND a generator must REJECT, not round-trip. A generator
/// carries no `WireValue` id (its parked frame can't be a `Backref` target), so once the containers
/// back-reference, the depth cap no longer trips on such a cycle — re-serializing the generator would
/// silently deep-copy it TWICE (two independent copies sharing one container). Here a PENDING generator
/// `gen(box)` parks `box` in its call args and `box` holds the generator (`box.push(g)`), so
/// `box -> g -> box` is a cycle through the (non-preservable) generator. The `gens_on_stack` guard
/// rejects it cleanly on BOTH engines (byte-identical fault). Without the guard the program prints
/// `got 1` (the duplicated generator round-trips) — parity-blind, both engines agree on the wrong result.
#[test]
fn generator_in_data_cycle_rejects_both() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    yield box.len()
fn main():
    box: List[Iterator[int]] = []
    g := gen(box)
    box.push(g)
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    let ve = vm_outcome(src).expect_err("serial: generator in a data cycle must reject");
    let pe = parallel_outcome(src).expect_err("M:N: generator in a data cycle must reject");
    assert!(ve.contains("reference cycle"), "serial: {ve}");
    assert!(pe.contains("reference cycle"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// REGRESSION (bug #2, the SUSPENDED shape): a SUSPENDED single-frame generator whose PARKED STACK SLOT
/// holds a container that transitively references the generator is the same cycle-through-a-generator
/// wrong-result as `generator_in_data_cycle_rejects_both`, reached via the `Suspended` arm (parked
/// operand stack) instead of `Pending` (call args). `keep := box` parks `box` across the first `yield`;
/// `box.push(g)` closes `box -> g -> box`. Must reject cleanly on both engines (never duplicate the
/// suspended generator's live drive-state).
#[test]
fn suspended_generator_in_data_cycle_rejects_both() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    keep := box
    yield 0
    yield keep.len()
fn main():
    box: List[Iterator[int]] = []
    g := gen(box)
    started := g.next()
    box.push(g)
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    let ve = vm_outcome(src).expect_err("serial: suspended generator in a data cycle must reject");
    let pe =
        parallel_outcome(src).expect_err("M:N: suspended generator in a data cycle must reject");
    assert!(ve.contains("reference cycle"), "serial: {ve}");
    assert!(pe.contains("reference cycle"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// Identity-preserving airlock, generator path — a suspended generator whose PARKED SLOT holds a
/// recursive local `fn` (`keep := rec`, a self-cycle closure) now ROUND-TRIPS: the generator serializes
/// its parked stack via `to_wire`, which back-references the self-cell, and `from_wire` ties the knot.
/// The generator crosses into the spawned task, resumes, and `keep(3) == 3` on both engines. Pairs with
/// `generator_parked_slot_nonsendable_rejects_both` (which now uses a cyclic struct, not a recursive fn).
#[test]
fn generator_carrying_recursive_closure_round_trips_both() {
    let src = "\
fn gen() -> Iterator[int]:
    fn rec(n: int) -> int:
        if n <= 0:
            return 0
        return rec(n - 1) + 1
    keep := rec
    yield 1
    yield keep(3)
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
main()";
    assert_parity_out(src, "3\n");
}

// ----- Backlog item B: a MODULE-GLOBAL live generator crosses the airlock BY VALUE (deep copy) -----
// The reach-gate + Option-B poison→Nil model is RETIRED. A module-global generator reached by a spawned
// task now crosses BY VALUE exactly like a frame-local one (F3 path C): each task gets its own frozen
// per-task snapshot (F1) whose generator arm deep-copies via `to_wire`/`from_wire` into the worker heap.
// A genuinely non-sendable parked slot (deep acyclic nest) or a reference cycle still REJECTS at
// serialize time — the direct `to_wire` reject, NOT a silent poison→Nil. serial (`--serial` oracle) == M:N.

/// A module-global PENDING generator reached by a spawned task crosses by value and drives fully; the
/// parent's own `g` is an INDEPENDENT copy (drives the full sequence after the join). Was the reach-gate
/// fault (`a generator cannot be sent across tasks`); now crosses. Byte-identical serial == M:N.
#[test]
fn generator_module_global_reached_crosses_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
fn main():
    out := Channel[int]()
    parallel:
        spawn:
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    print(\"parent:\")
    for x in g:
        print(x)
main()";
    let expect = "1\n2\n3\nparent:\n1\n2\n3\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A module-global generator driven ONCE at top level (suspended, one parked frame) then reached by a
/// spawned task crosses by value and RESUMES from its parked position. serial == M:N.
#[test]
fn generator_module_global_suspended_reached_resumes_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
started := g.next()
fn main():
    out := Channel[int]()
    parallel:
        spawn:
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
main()";
    let expect = "2\n3\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// ISOLATION (adversarial, parity-blind, the memory-safety witness): TWO tasks each reach the SAME
/// module-global generator and each gets an INDEPENDENT copy — both see the full `1,2,3`, and the
/// parent's own `g` is likewise independent (full `1,2,3` after the join). A shared `GcRef` would show
/// split/interleaved consumption instead. serial == M:N byte-identical.
#[test]
fn generator_module_global_two_tasks_independent_copies_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:
        spawn:
            for x in g:
                a.send(x)
        spawn:
            for x in g:
                b.send(x)
    print(\"a:\")
    print(a.recv())
    print(a.recv())
    print(a.recv())
    print(\"b:\")
    print(b.recv())
    print(b.recv())
    print(b.recv())
    print(\"parent:\")
    for x in g:
        print(x)
main()";
    let expect = "a:\n1\n2\n3\nb:\n1\n2\n3\nparent:\n1\n2\n3\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// A module-global generator carrying a genuinely NON-SENDABLE parked slot (a >10000-deep acyclic nest,
/// tripping `MAX_STRUCTURAL_DEPTH`) is snapshotted as an inert `Nil` placeholder (the `to_snap` slow arm
/// no longer eager-faults the whole snapshot — that regressed any module merely *holding* such a
/// generator). A task that REACHES it therefore faults recoverably AT THE USE SITE ("cannot iterate over
/// nil"), NOT a crash, NOT a silent skip. Byte-identical serial == M:N. (The unreached case runs clean —
/// see `generator_module_global_unreached_nonsendable_runs_clean_both`.)
#[test]
fn generator_module_global_parked_slot_nonsendable_rejects_both() {
    let src = "\
fn gen() -> Iterator[int]:
    keep: List[int] = []
    deep: List[List[int]] = [keep]
    for i in 0..10001:
        deep = [deep]
    yield 1
    yield deep.len()
g := gen()
started := g.next()
fn main():
    out := Channel[int]()
    parallel:
        spawn:
            for x in g:
                out.send(x)
main()";
    let ve = vm_outcome(src).expect_err("serial: reached non-sendable generator must fault");
    let pe = parallel_outcome(src).expect_err("M:N: reached non-sendable generator must fault");
    assert!(ve.contains("cannot iterate over nil"), "serial: {ve}");
    assert!(pe.contains("cannot iterate over nil"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// A module-global generator in a reference CYCLE is snapshotted as an inert `Nil` placeholder (its
/// `to_wire` trips item A's `gens_on_stack` cycle guard → the `to_snap` slow arm falls back to `Nil`
/// rather than eager-faulting the whole snapshot). `box` (module global) holds `g` and `g`'s Pending arg
/// holds `box`, so `box -> g -> box`. A task that REACHES it faults recoverably at the use site ("cannot
/// iterate over nil"), byte-identical serial == M:N.
#[test]
fn generator_module_global_in_data_cycle_rejects_both() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    yield box.len()
box: List[Iterator[int]] = []
g := gen(box)
pushed := box.push(g)
fn main():
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    let ve =
        vm_outcome(src).expect_err("serial: reached module-global generator in a cycle must fault");
    let pe = parallel_outcome(src)
        .expect_err("M:N: reached module-global generator in a cycle must fault");
    assert!(ve.contains("cannot iterate over nil"), "serial: {ve}");
    assert!(pe.contains("cannot iterate over nil"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// REGRESSION LOCK (backlog item B remediation): a module module that merely HOLDS a non-sendable
/// module-global generator (here in a reference cycle) but whose spawned task NEVER reaches it must run
/// CLEAN — the `to_snap` slow arm snapshots the untouched generator as an inert `Nil`, so `snapshot_modules`
/// (which walks EVERY global once at the first spawn) no longer eager-faults the whole program. Before the
/// remediation this faulted "as part of a reference cycle" at the first spawn on BOTH engines. Byte-identical
/// serial == M:N. (Reached-behaviour is covered by `generator_module_global_in_data_cycle_rejects_both`.)
#[test]
fn generator_module_global_unreached_nonsendable_runs_clean_both() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    yield box.len()
box: List[Iterator[int]] = []
g := gen(box)
pushed := box.push(g)
fn hello():
    print(\"hello\")
fn main():
    parallel:
        spawn hello()
    print(\"done\")
main()";
    let expect = "hello\ndone\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// The deleted `gate_executor_queue` path: a module-global generator reached inside an `Executor.submit`
/// job now crosses by value and drives (was the executor reach-gate fault). serial == M:N.
#[test]
fn generator_module_global_via_executor_crosses_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
g := gen()
out := Channel[int]()
fn job():
    for x in g:
        out.send(x)
fn main():
    ex := Executor()
    ex.submit(job)
    ex.shutdown()
    print(out.recv())
    print(out.recv())
main()";
    let expect = "1\n2\n";
    assert_eq!(vm_outcome(src).expect("serial"), expect);
    assert_eq!(parallel_outcome(src).expect("M:N"), expect);
}

/// CHECKER-UNREACHABLE defensive guard (backlog arm c): `defer` is banned inside a generator body
/// (`checker::sig` — "`defer` is not supported inside a generator"), so a suspended generator's
/// parked frame can NEVER carry a pending `defer` from checker-valid source. This test reaches the
/// guard only because the parity harness (`run_program_inner`) compiles WITHOUT the checker (the
/// compiler is type-blind); the `to_wire` reject in `sched.rs` is a belt-and-braces guard against
/// that type-blind path. Kept as a reject (nothing built): the state is unreachable, so there is
/// nothing coherent to serialize.
#[test]
fn generator_parked_defer_rejects_clean_both() {
    let src = "\
fn gen() -> Iterator[int]:
    defer print(\"cleanup\")
    yield 1
    yield 2
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
main()";
    let ve = vm_outcome(src).expect_err("serial: parked defer must reject");
    let pe = parallel_outcome(src).expect_err("M:N: parked defer must reject");
    assert!(ve.contains("pending `defer`"), "serial: {ve}");
    assert!(pe.contains("pending `defer`"), "M:N: {pe}");
    assert_eq!(ve, pe, "serial == M:N fault");
}

/// Backlog arm (b) — a generator suspended INSIDE a `recover:` block (a LIVE handler in its parked
/// context) now CROSSES the airlock and RESUMES with its recover boundary intact. The generator is
/// advanced once at top level (suspending at `yield 1`, inside the recover), sent into a spawned
/// task, and driven to exhaustion there. The recover block completes NORMALLY after crossing (the
/// trailing `99` becomes `Ok(99)`), and the bound `r` flows through the post-crossing `match` — so
/// the resumed values are `2, 3, 99`, proving the serialized `Handler` (stack_len/frame_len/ip)
/// reconstructs a coherent boundary. Was a clean HARD-ARM reject; now a resume-semantics test.
#[test]
fn generator_recover_suspended_resumes_both() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 1
        yield 2
        99
    yield 3
    match r:
        Ok(v): yield v
        Err(e): yield -1
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
    print(out.recv())
    print(out.recv())
main()";
    assert_parity_out(src, "2\n3\n99\n");
}

/// Backlog arm (b), the item-#2 SEMANTIC guard (not just a non-faulting round-trip): after crossing
/// the airlock, the generator's `recover:` must actually CATCH a post-crossing fault and produce the
/// correct recovered value — not silently drop the handler and let the fault abort the task. The
/// generator suspends at `yield 10` inside the recover; when resumed in the spawned task the `1 / 0`
/// fires, the (serialized) recover catches it (`r = Err`), and the post-recover `match` yields `99`
/// down the Err arm. Asserted EQUAL to a same-heap, NO-airlock control that drives the identical
/// generator inline (`generator_recover_fault_control_inline`): if the handler were NOT serialized,
/// the crossed fault would be UNCAUGHT and abort the task, diverging from the control.
#[test]
fn generator_crossed_recover_catches_fault_matches_control_both() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 10
        boom := 1 / 0
        boom
    yield 20
    match r:
        Ok(v): yield v
        Err(e): yield 99
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
    print(out.recv())
main()";
    // Expected value is pinned by the same-heap control below (plain, well-tested generator
    // semantics with no airlock): catch → r=Err → yield 20, then match Err → yield 99.
    assert_parity_out(src, "20\n99\n");
}

/// Same-heap NO-airlock control for `generator_crossed_recover_catches_fault_matches_control_both`:
/// the identical generator driven INLINE (never crossing a task boundary). This pins the correct
/// recovered semantics with the plain, well-exercised generator+recover path; the crossed test must
/// match this exact output or the airlock changed the computation.
#[test]
fn generator_recover_fault_control_inline() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 10
        boom := 1 / 0
        boom
    yield 20
    match r:
        Ok(v): yield v
        Err(e): yield 99
fn main():
    it := gen()
    started := it.next()
    for x in it:
        print(x)
main()";
    assert_parity_out(src, "20\n99\n");
}

/// Backlog arm (b), the nursery-floor rebase (load-bearing, SERIAL oracle). A generator's `recover:`
/// handler captures an ABSOLUTE `nursery_len` at `PushHandler` — here `0` (the recover is entered by
/// the top-level `it.next()`, before any `parallel:`). The generator is then RESUMED at a DEEPER
/// nursery floor: inside a `parallel:` (nurseries==1) in the parent fiber, alongside a sibling
/// `spawn`. When the resumed `1 / 0` is caught, the recover-catch path calls
/// `drain_escaped_nursery(handler.nursery_len)`. Without the resume-time rebase that stale `0` would
/// truncate the LIVE parallel nursery to `0`, cancelling the sibling `spawn` (`out.send(77)`), and
/// `out.recv()` would then deadlock. A generator provably cannot open a nursery (spawn/parallel are
/// checker-banned inside one), so its handler's escape-drain must be a no-op — `generator_next`
/// rebases every parked frame/handler `nursery_len` to the resuming driver's floor. Serial-only:
/// this drain is deterministic on the cooperative engine (the sibling is an unstarted `PendingCall`
/// when the drain would fire); on M:N the sibling may already be running on another worker, so the
/// bug is not deterministically reachable there — the shared `generator_next` fix covers both.
#[test]
fn generator_crossed_recover_fault_leaves_siblings_intact_serial() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 1
        boom := 1 / 0
        yield boom
    yield 2
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            out.send(77)
        for x in it:
            print(x)
    print(out.recv())
main()";
    assert_eq!(
        vm_outcome(src),
        Ok("2\n77\n".to_string()),
        "the sibling spawn's sentinel must survive the recover-caught fault (nursery_len rebase)",
    );
}

/// Backlog item B, cross-module path: a spawned task calls a module-member fn `genmod.bar(10)` whose
/// body drives a live generator global in ITS OWN module. The generator crosses BY VALUE via the
/// other module's per-task snapshot, so the task drives its own copy: `10 + 1 + 2 = 13`. Byte-identical
/// serial == M:N (was the cross-module reach-gate fault).
#[test]
fn generator_cross_module_member_call_crosses_both() {
    let t = TmpDir::new();
    t.write(
        "genmod.chz",
        "fn gen() -> Iterator[int]:\n    yield 1\n    yield 2\ng := gen()\nfn bar(n: int) -> int:\n    total := n\n    for x in g:\n        total = total + x\n    return total\n",
    );
    let ep = t.write(
        "main.chz",
        "import genmod\nparallel:\n    spawn:\n        r := genmod.bar(10)\n        print(r)\n",
    );
    let (so, _, sr, _) = run_file(&ep);
    let (po, _, pr, _) = run_file_p(&ep);
    sr.expect("serial cross-mod generator crossing");
    pr.expect("M:N cross-mod generator crossing");
    assert_eq!(so, "13\n");
    assert_eq!(po, "13\n");
}

/// A CLEAN cross-module member call (`geomod.helper()` reads no generator) runs clean on both engines
/// with a generator global resident in the imported module.
#[test]
fn genreach_cross_module_clean_member_call_ok_both() {
    let t = TmpDir::new();
    t.write(
        "geomod.chz",
        "fn gen() -> Iterator[int]:\n    yield 1\ng := gen()\nfn helper(n: int) -> int:\n    return n + 1\n",
    );
    let ep = t.write(
        "main.chz",
        "import geomod\nparallel:\n    spawn:\n        r := geomod.helper(10)\n        print(r)\n",
    );
    let (so, _, sr, _) = run_file(&ep);
    let (po, _, pr, _) = run_file_p(&ep);
    sr.expect("serial clean cross-mod member call");
    pr.expect("M:N clean cross-mod member call");
    assert_eq!(so, "11\n");
    assert_eq!(po, "11\n");
}

#[test]
fn bare_func_crosses_spawn_callee_renders_fn_name_parity() {
    // A bare top-level fn captured into a spawn callee crosses as a distinct `WireValue::Func` (not a
    // Closure), so `str()` still renders `<fn NAME>` — identical serial vs M:N. Proves the distinct
    // Func variant preserves bare-func identity across the airlock.
    let src = "\
fn helper(x: int) -> int:
    return x + 1
fn main():
    ch := Channel[str]()
    f := helper
    fn task():
        ch.send(str(f))
    parallel:
        spawn task()
    print(ch.recv())
main()";
    assert_parity_out(src, "<fn helper>\n");
}

// ===== B3.3 (Task 2a) — closures-as-data at spawn/Channel sites RUN on both engines =====
// The checker half (permissive `sendable(Func)` + capture gate) makes these type-check; here we pin
// that they actually RUN identically on the serial + M:N engines (the by-value airlock crossing).

#[test]
fn channel_of_closures_runs_parity() {
    // A `Channel[fn() -> int]`: a capture-free closure is sent over the channel and called by the
    // consumer — crosses by value, runs 42 on both engines.
    let src = "\
fn producer(ch: Channel[fn() -> int]):
    ch.send(fn() -> int: 42)
fn main():
    ch := Channel[fn() -> int]()
    parallel:
        spawn producer(ch)
    f := ch.recv()
    print(f())
main()";
    assert_parity_out(src, "42\n");
}

#[test]
fn closure_returned_across_task_runs_parity() {
    // A CAPTURING closure (`adder(100)` closes over the int `n=100`) returned from a factory and sent
    // over a `Channel[fn(int) -> int]` — its capture is sendable, so it crosses by value; `f(5)` = 105
    // on both engines.
    let src = "\
fn adder(n: int) -> fn(int) -> int:
    return fn(x: int) -> int: x + n
fn producer(ch: Channel[fn(int) -> int]):
    ch.send(adder(100))
fn main():
    ch := Channel[fn(int) -> int]()
    parallel:
        spawn producer(ch)
    f := ch.recv()
    print(f(5))
main()";
    assert_parity_out(src, "105\n");
}

#[test]
fn sendable_capturing_closure_through_channel_still_runs() {
    // A closure capturing ONLY sendable data (an int + a `List[int]`) sent over a `Channel[fn() -> int]`
    // still crosses by value and runs. `21*2 + 1` = 43, both engines.
    let src = "\
fn main():
    n := 21
    nums := [1, 2, 3]
    ch := Channel[fn() -> int]()
    fn calc() -> int:
        return n * 2 + nums[0]
    ch.send(calc)
    f := ch.recv()
    print(f())
main()";
    assert_parity_out(src, "43\n");
}

#[test]
fn parallel_block_defer_flushes_after_join_parity() {
    // A `defer` inside an explicit `parallel:` block flushes AFTER the block's children join at the
    // dedent — same order as the implicit function-body nursery. Pre-fix the defer fired BEFORE the
    // spawned child joined (block-defer/child-body/after); now: child-body/block-defer/after.
    let src = "\
fn log(m: str): print(m)
fn child(): log(\"child-body\")
fn main():
    parallel:
        spawn child()
        defer log(\"block-defer\")
    log(\"after\")
main()";
    assert_parity_out(src, "child-body\nblock-defer\nafter\n");
}

#[test]
fn parallel_block_defer_close_after_join_parity() {
    // A deferred channel close inside `parallel:` must run AFTER the spawned send joins — the natural
    // cleanup pattern. Pre-fix the close raced ahead of the send ("send on a closed channel"); now the
    // send buffers before close, so `ch.len()` is 1.
    let src = "\
fn snd(ch: Channel[int]): ch.send(1)
fn main():
    ch := Channel[int]()
    r := recover:
        parallel:
            spawn snd(ch)
            defer ch.close()
        1
    match r:
        Ok(v): print(\"ok, len={ch.len()}\")
        Err(e): print(\"err: {e.message()}\")
main()";
    assert_parity_out(src, "ok, len=1\n");
}

#[test]
fn parallel_block_defer_break_flushes_once_parity() {
    // Regression for the break/early-jump path: a loop whose body is a `parallel:` block with a
    // `defer` then a `break` still flushes the block's defer EXACTLY once (the jump path drains it;
    // the fall-through JoinNursery+LeaveDeferScope is skipped). Guards that the reorder didn't botch
    // the defer_scopes/nursery_scopes counter bracketing.
    let src = "\
fn log(m: str): print(m)
fn main():
    i := 0
    while i < 3:
        parallel:
            defer log(\"d\")
            break
        i = i + 1
    log(\"done\")
main()";
    assert_parity_out(src, "d\ndone\n");
}

// ----- B1: qualified generic turbofish in expression position (mod.Type[int].Variant / .static) -----

/// The shared multi-generic fixture: an enum `Tree[T]` (variant + method) and a struct `Box[T]`
/// (static method), imported whole-module so the base is only reachable qualified (`shapes.Tree`).
const SHAPES_MOD: &str = "enum Tree[T]:\n    Leaf(T)\n    Branch(Tree[T], Tree[T])\n    fn first(self) -> T:\n        match self:\n            Tree.Leaf(x): return x\n            Tree.Branch(l, r): return l.first()\n\nstruct Box[T]:\n    v: T\n    fn make(x: T) -> Box[T]:\n        return Box(x)\n";

#[test]
fn qualified_enum_variant_turbofish_runs() {
    let out = assert_parity_file(
        &[
            ("shapes.chz", SHAPES_MOD),
            (
                "main.chz",
                "import shapes\nx := shapes.Tree[int].Leaf(9)\nprint(x.first())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "9\n");
}

#[test]
fn qualified_struct_static_turbofish_runs() {
    let out = assert_parity_file(
        &[
            ("shapes.chz", SHAPES_MOD),
            (
                "main.chz",
                "import shapes\nb := shapes.Box[int].make(5)\nprint(b.v)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "5\n");
}

#[test]
fn qualified_combined_turbofish_runs() {
    // Combined qualified turbofish `mod.Type[int].static[U](args)` — the enclosing `[int]` AND the
    // method-own `[U]` are both runtime-erased. The checker accepts it (qualified head recognized),
    // so the compiler must lower it (not fall to CallMethod). Assert VALUE correctness, not just
    // engine agreement (identical-but-wrong bytecode would still agree).
    let out = assert_parity_file(
        &[
            (
                "shapes.chz",
                "struct Box[T]:\n    v: T\n    fn wrap[U](x: T, tag: U) -> Box[T]:\n        return Box(x)\n",
            ),
            (
                "main.chz",
                "import shapes\nb := shapes.Box[int].wrap[str](7, \"hi\")\nprint(b.v)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "7\n");
}

// ----- Map/Set snapshot a struct/enum/newtype key on INSERT (Go value-key model) -----
// A struct/enum/newtype key/element is deep-copied when STORED, so a later mutation of the
// caller's original value cannot reach (corrupt) the stored key. Scalars pass through unchanged;
// map VALUES are never copied; the transient lookup key is never snapshotted.

/// Test A — a struct key is snapshotted on insert: mutating the original after `m[a]=..` leaves
/// the stored key intact (probe by an equal fresh key), and `m.keys()` shows the PRE-mutation key.
#[test]
fn map_struct_key_snapshot_on_insert() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    a := K(1)
    m: Map[K, str] = {}
    m[a] = "one"
    a.x = 2                    # mutate the live key in place
    print(m[K(1)])             # still resolves — stored key was snapshotted at x=1
    print(m.has(a))            # a is now K(2), not a stored key -> false (no fault)
    print(m.get(a))            # None (no fault)
    ks := m.keys()
    print(ks[0].x)             # 1 — the snapshot, not the mutated original
main()
"#;
    assert_parity_out(src, "one\nfalse\nNone\n1\n");
}

/// Test B — a set element is snapshotted, so mutating the original after `{a, ..}` cannot break
/// the set invariant; difference/intersection/union/== stay correct.
#[test]
fn set_element_snapshot_algebra() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    a := K(1)
    s := {a, K(2)}
    a.x = 2                              # would corrupt s if elements aliased
    print(s.len())                       # 2 (snapshots {x=1, x=2})
    print(s.difference({K(2)}).len())    # 1
    print(s.intersection({K(1)}).len())  # 1
    print(s.union({K(2)}).len())         # 2
    print(s == {K(1), K(2)})             # true
main()
"#;
    assert_parity_out(src, "2\n1\n1\n2\ntrue\n");
}

/// Test C — scalar keys (int/str) are unchanged (regression guard for the zero-clone hot path).
#[test]
fn scalar_keys_unchanged() {
    let src = r#"
fn main():
    m := {1: "a"}
    m[1] = "b"
    print(m[1])
    c: Map[str, int] = {}
    c["x"] = 1
    c["x"] = c["x"] + 1
    print(c["x"])
    print(Set([3, 1, 3, 2]).len())
main()
"#;
    assert_parity_out(src, "b\n2\n3\n");
}

/// Test D — map VALUES are NOT snapshotted: mutating a stored value in place is intended.
#[test]
fn map_value_not_snapshotted() {
    let src = r#"
struct V:
    n: int
fn main():
    m: Map[int, V] = {}
    m[1] = V(5)
    m[1].n = 9                 # mutate the stored value in place
    print(m[1].n)              # 9 — value held by reference
main()
"#;
    assert_parity_out(src, "9\n");
}

/// Test — every insert entry point snapshots uniformly (map literal, set literal, set.add,
/// Map(iterable), Set(iterable), map index-set). Mutate an original inserted via each path.
#[test]
fn all_insert_paths_snapshot() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    # map literal
    a := K(1)
    ml := {a: "v"}
    a.x = 9
    print(ml.keys()[0].x)
    # set literal
    b := K(2)
    sl := {b}
    b.x = 9
    print(sl.difference({K(2)}).len())
    # set.add
    c := K(3)
    sa := {K(0)}
    sa.add(c)
    c.x = 9
    print(sa.difference({K(3)}).len())
    # Map(iterable)
    d := K(4)
    mi := Map([(d, "v")])
    d.x = 9
    print(mi.keys()[0].x)
    # Set(iterable)
    e := K(5)
    si := Set([e])
    e.x = 9
    print(si.difference({K(5)}).len())
    # map index-set (general struct-key path)
    f := K(6)
    ms: Map[K, str] = {}
    ms[f] = "v"
    f.x = 9
    print(ms.keys()[0].x)
main()
"#;
    assert_parity_out(src, "1\n0\n1\n4\n0\n6\n");
}

// ----- Regression: snapshot must NOT reuse the airlock deep_clone (its fault/identity-remap modes)
// A struct/enum/newtype key that embeds a cyclic back-edge, a live generator, or an identity-typed
// (closure/channel/…) sub-value used to be stored BY REFERENCE and worked on a plain serial insert.
// The first cut routed the snapshot through `deep_clone` (to_wire/from_wire), which FAULTED on the
// first two and gave the identity sub-value a FRESH handle (so `values_equal`, identity-only for it,
// never matched → lookup miss). `snapshot_value` copies only mutable structural arms + keeps
// identity/by-ref sub-values by handle, so all three insert AND resolve again.

/// A self-cyclic struct key inserts (cycle preserved by the visited-map, no depth-overflow fault)
/// and still resolves — regression for the `deep_clone` "maximum structural depth exceeded" fault.
#[test]
fn cyclic_struct_key_inserts_and_resolves() {
    let src = r#"
struct Node:
    val: int
    next: Option[Node]
    fn hash(self) -> int: return self.val
fn main():
    a := Node(1, None)
    a.next = Some(a)
    s := {a}
    print(s.len())          # 1 — built, not a runtime fault
    m: Map[Node, str] = {}
    m[a] = "x"
    print(m.has(a))         # true — same held key still resolves
main()
"#;
    assert_parity_out(src, "1\ntrue\n");
}

/// A struct key that transitively holds a live generator inserts + resolves — regression for the
/// `deep_clone` "a generator cannot be sent across tasks" fault on a purely sequential insert.
#[test]
fn generator_field_key_inserts_and_resolves() {
    let src = r#"
fn gen() -> Iterator[int]:
    yield 1
struct K:
    g: Iterator[int]
    id: int
    fn hash(self) -> int: return self.id
fn main():
    k := K(gen(), 7)
    m: Map[K, str] = {}
    m[k] = "one"
    print(m.has(k))         # true — was a generator/airlock fault before
    print(m[k])             # one
    s := {k}
    print(s.len())          # 1
main()
"#;
    assert_parity_out(src, "true\none\n1\n");
}

/// A struct key with an identity-typed (closure) field resolves after insert — regression for the
/// `deep_clone` "key not found" miss (the cloned closure field got a fresh handle that
/// `values_equal`, identity-only for closures, could never match against the live key's field).
#[test]
fn closure_field_key_resolves_after_insert() {
    let src = r#"
struct H:
    id: int
    cb: fn() -> int
    fn hash(self) -> int: return self.id
fn main():
    h := H(1, fn() -> int: 1)
    m: Map[H, str] = {}
    m[h] = "x"
    print(m[h])             # x — was "key not found" before (fresh-handle closure field)
    print(m.has(h))         # true
    s := {h}
    print(s.difference({h}).len())   # 0 — same held element compares equal
main()
"#;
    assert_parity_out(src, "x\ntrue\n0\n");
}

/// `map.update`/`map.merge` also snapshot the incoming key (spec: "cover EVERY insert entry point")
/// so the merged map does NOT alias the source map's stored key — mutating one via `keys()` cannot
/// corrupt the other.
#[test]
fn map_update_merge_snapshot_keys() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    o: Map[K, str] = {}
    o[K(1)] = "a"
    # update
    m: Map[K, str] = {}
    m.update(o)
    k := o.keys()[0]
    k.x = 9                 # mutate the SOURCE's stored key
    print(m.has(K(1)))      # true — m's key was snapshotted, not aliased
    print(m.keys()[0].x)    # 1
    # merge
    o2: Map[K, str] = {}
    o2[K(2)] = "b"
    mm := o2.merge(o2)
    k2 := o2.keys()[0]
    k2.x = 8
    print(mm.has(K(2)))     # true
    print(mm.keys()[0].x)   # 2
main()
"#;
    assert_parity_out(src, "true\n1\ntrue\n2\n");
}

/// A very deep (acyclic) struct key must still insert AND resolve when looked up with the SAME held
/// object. Beyond `MAX_STRUCTURAL_DEPTH` the snapshot walk caps and would otherwise store a distinct
/// tail-aliased handle whose structural `values_equal` trips the same depth guard → a silent
/// `key not found` on the very object just inserted. The store path degrades an over-deep key to
/// by-reference (like a cyclic key) so the identity short-circuit resolves it. Same cap prevents the
/// unbounded `cyclic_walk` host-stack overflow (SIGABRT) on such keys. Both engines identical.
#[test]
fn deep_acyclic_struct_key_inserts_and_resolves() {
    let src = r#"
struct Node:
    val: int
    next: Option[Node]
    fn hash(self) -> int: return self.val
fn main():
    n := Node(0, None)
    for i in range(10050):
        n = Node(i, Some(n))
    m: Map[Node, str] = {}
    m[n] = "deep"
    print(m[n])          # deep — held key still resolves past MAX_STRUCTURAL_DEPTH
    print(m.has(n))      # true
    s := {n}
    print(s.len())       # 1
main()
"#;
    assert_parity_out(src, "deep\ntrue\n1\n");
}

// ===== widening follow-ups (adversarial review): alias sinks, variadic float param, `Any` elements

/// A float sink spelled through a type ALIAS coerces exactly like `float` at EVERY sink (let, param,
/// return, struct field, param default, `List[F]` elements). Before the alias table the compiler's
/// syntactic `is_float_ty` never matched `F`, so the checker accepted the widen and the backend
/// emitted no `Op::CoerceFloat` — a runtime `Int` under a static `float` (`x / 2` → `0`).
#[test]
fn widen_float_alias_sinks_coerce() {
    widen_three_engines(
        "type F = float\nfn g(z: F) -> F:\n    return z\nfn k(a: F = 3) -> F:\n    return a\nstruct P:\n    v: F\nx: F = 1\nprint(x / 2)\nprint(g(3) / 2)\nprint(k() / 2)\nprint(P(3).v / 2)\nxs: List[F] = [1, 2.5]\nprint(xs[0] / 2)\n",
        "0.5\n1.5\n1.5\n1.5\n0.5\n",
    );
    // an alias OF an alias resolves too
    widen_three_engines(
        "type F = float\ntype G = F\ny: G = 1\nprint(y / 2)\n",
        "0.5\n",
    );
}

/// A VARIADIC `float` param (`fn f(...zs: float)`) packs its args into a `List[float]`: the callee
/// prologue must NOT `Op::CoerceFloat` that slot (it holds a List — a guaranteed runtime fault on a
/// program the checker just called well-typed). The elements are coerced by the list peephole.
#[test]
fn widen_variadic_float_param_runs() {
    widen_three_engines(
        "fn f(...zs: float):\n    print(zs)\n    print(zs[0] / 2)\nf(1, 2.5)\n",
        "[1.0, 2.5]\n0.5\n",
    );
}

/// A mixed untyped-numeric-CONSTANT literal has `float` elements in EVERY element context, including
/// a `List[Any]` slot and the variadic `...xs: Any` pack — the compiler's peephole is type-blind, so
/// the CHECKER widens there too (it types the element `float`, which `Any` accepts). Checker and
/// backend agree; nothing stores a value the static type does not describe.
#[test]
fn widen_any_collection_const_mix_agrees() {
    widen_three_engines("xs: List[Any] = [1, -2.5]\nprint(xs)\n", "[1.0, -2.5]\n");
    widen_three_engines(
        "fn show(...xs: Any):\n    print(xs)\nshow(1, 2.0 + 0.5)\n",
        "[1.0, 2.5]\n",
    );
    // GUARD (the other direction): a TYPED int element is never touched — neither side widens it, so
    // it stays an `Int` under the `Any` element type.
    widen_three_engines(
        "a := 1\nxs: List[Any] = [a, -2.5]\nprint(xs)\n",
        "[1, -2.5]\n",
    );
}

/// An ALL-int-constant literal under a `List[float]` / `Map[_, float]` annotation adapts (the
/// annotation is the type context) and lands as genuine f64s.
#[test]
fn widen_annotated_all_int_collection_runs() {
    widen_three_engines(
        "xs: List[float] = [1, 2]\nm: Map[str, float] = {\"a\": 1}\nprint(xs)\nprint(m)\nprint(xs[0] / 2)\n",
        "[1.0, 2.0]\n{a: 1.0}\n0.5\n",
    );
}

// ===== adversarial-review fixes: a generic TYPE PARAM shadows a module float alias =====

/// A generic TYPE PARAMETER shadows a module-level `type F = float` alias. The backend's alias table
/// is module-scoped; the checker resolves `F` in the DECLARATION's type-param scope (a `Ty::Param`).
/// So no coercion site (param prologue, `ret_is_float`, ctor field, `let` elem hint) may fire on a
/// value whose static type is the type VARIABLE — else a runtime `Float` sits under a static `int`
/// (silent precision loss, no integer-overflow check) and a `str` instantiation hard-faults on a
/// check-clean program.
#[test]
fn float_alias_shadowed_by_type_param_no_coerce() {
    widen_three_engines(
        "type F = float\nfn g[F](x: F) -> F:\n    return x\nr := g(5)\nprint(r)\nprint(r % 2)\nprint(g(\"hi\"))\n",
        "5\n1\nhi\n",
    );
    // generic struct: ctor float-field coercion + method return + a `List[F]` annotated `let`
    widen_three_engines(
        "type F = float\nstruct S[F]:\n    v: F\n\n    fn get(self) -> F:\n        return self.v\n\nprint(S[int](5).get())\nprint(S[str](\"hi\").get())\nfn h[F](x: F) -> List[F]:\n    xs: List[F] = [x]\n    return xs\nprint(h(5))\n",
        "5\nhi\n[5]\n",
    );
}

/// Over-rejection guard for the generic-method fix: a param DECLARED `float` (on a plain OR a generic
/// struct) still adapts an untyped int constant — the backend's prologue coerces it, so the checker
/// must keep accepting it. Only a param declared as the type VARIABLE (`T` instantiated at float) is
/// rejected (see checker::tests::widen_generic_method_param_at_float_rejected).
#[test]
fn widen_method_float_param_still_adapts() {
    widen_three_engines(
        "struct P:\n    v: float\n\n    fn set(self, x: float):\n        self.v = x\n\np := P(0.0)\np.set(1)\nprint(p.v)\nprint(p.v / 2)\n",
        "1.0\n0.5\n",
    );
    widen_three_engines(
        "struct Box[T]:\n    v: T\n\n    fn scale(self, k: float) -> float:\n        return k\n\nb := Box[str](\"s\")\nprint(b.scale(1) / 2)\n",
        "0.5\n",
    );
}

// ----- checker rejects (bound-method value, index/set_index V-coherence): the RUN-side guards -----
// Both fixes are checker-only, so the risk is OVER-rejection, not divergence. These pin the
// neighbours that must keep RUNNING identically on serial + M:N.

#[test]
fn coherent_index_set_str_parity() {
    // A COHERENT user IndexSet (`index -> str` / `set_index(_, val: str)`): read, write, compound,
    // negative index. The compound's LHS is typed from `index`'s RETURN (`x OP= v` ≡ `x = x OP v`).
    assert_parity_out(
        "\
struct S:
    d: List[str]
    fn index(self, key: int) -> str:
        return self.d[key]
    fn set_index(self, key: int, val: str):
        self.d[key] = val
s := S([\"a\", \"b\"])
print(s[0])
s[0] = \"x\"
print(s[0])
s[1] += \"y\"
print(s[1])
print(s[-1])
",
        "a\nx\nby\nby\n",
    );
}

#[test]
fn compound_index_assign_evaluates_index_once_parity() {
    // Python parity: `t[f()] += v` evaluates the index expression EXACTLY ONCE (the lowering dups
    // it). A side-effecting index must not double-fire — on a user IndexSet, a map, or a list.
    assert_parity_out(
        "\
struct S:
    d: List[int]
    fn index(self, key: int) -> int:
        return self.d[key]
    fn set_index(self, key: int, val: int):
        self.d[key] = val
fn bump(tag: str) -> int:
    print(\"idx {tag}\")
    return 0
s := S([1])
s[bump(\"struct\")] += 1
print(s[0])
m := {\"a\": 1}
m[\"a\"] += 1
print(m[\"a\"])
xs := [1, 2]
xs[bump(\"list\")] += 1
print(xs[0])
",
        "idx struct\n2\n2\nidx list\n2\n",
    );
}

#[test]
fn fn_typed_field_value_parity() {
    // THE closest neighbour to the bound-method reject: a genuinely fn-TYPED FIELD stays a
    // first-class value (`h.f(3)` AND `g := h.f; g(3)`), while a METHOD is call-only.
    assert_parity_out(
        "\
struct H:
    f: fn(int) -> int
    fn twice(self, x: int) -> int:
        return self.f(self.f(x))
fn dbl(x: int) -> int:
    return x * 2
h := H(dbl)
print(h.f(3))
g := h.f
print(g(3))
print(h.twice(3))
",
        "6\n6\n12\n",
    );
}

#[test]
fn asymmetric_index_set_plain_write_parity() {
    // NO-OVER-REJECTION: a plain `obj[k] = v` never READS through `index`, so an asymmetric pair is
    // sound and still runs (only the COMPOUND form is V-coherence-gated). A safe-read container
    // (`index -> int?`) and a widening writer (`index -> str` / `set_index(_, val: int)`).
    assert_parity_out(
        "\
struct T:
    d: Map[int, int]
    fn index(self, key: int) -> int?:
        return self.d.get(key)
    fn set_index(self, key: int, val: int):
        self.d[key] = val
struct W:
    d: List[str]
    fn index(self, key: int) -> str:
        return self.d[key]
    fn set_index(self, key: int, val: int):
        print(\"set {val}\")
t := T({})
t[0] = 9
match t[0]:
    Some(v): print(v)
    None: print(\"none\")
w := W([\"a\"])
w[0] = 1
print(w[0])
",
        "9\nset 1\na\n",
    );
}

/// A range is not a value — but its three SANCTIONED lowerings (`for` iterable, comprehension
/// clause, slice receiver) and the `match` range pattern must still RUN, byte-identically, on both
/// engines. These helpers SKIP the checker, so this pins the COMPILER half of the invariant: the
/// checker now rejects exactly what the compiler cannot lower, and everything below still lowers.
/// Also pins the diagnostic's hint as TRUE: `range(a, b)` really does materialize a `List[int]`.
#[test]
fn range_sanctioned_positions_run_on_both_engines() {
    let src = "
fn main():
    total := 0
    for i in 0..5:
        total = total + i
    print(total)
    print([i for i in 0..3])
    print([i for i in 0..10 if i % 2 == 0])
    print((0..10)[::2])
    print((0..10)[1:8:3])
    print((0..5)[::-1])
    match 3:
        1..5: print(\"in\")
        _: print(\"out\")
    print(range(0, 3))
    print(Set(range(0, 3)).len())
main()
";
    assert_parity(src);
    assert_eq!(
        vm_outcome(src),
        Ok("10\n[0, 1, 2]\n[0, 2, 4, 6, 8]\n[0, 2, 4, 6, 8]\n[1, 4, 7]\n[4, 3, 2, 1, 0]\nin\n[0, 1, 2]\n3\n".to_string())
    );
}

/// The bug: `x := 0..3` used to type-check clean and then die in the COMPILER, so `print("before")`
/// never ran (a compile error surfaced at run time, zero output, exit 1). The checker now rejects
/// it, but the compiler keeps its rejection as a defensive backstop — unreachable from a
/// check-clean program, yet still guarding these checker-SKIPPING helpers and synthesized ASTs.
#[test]
fn bare_range_value_still_rejected_by_the_compiler_backstop() {
    for engine in [vm_outcome, parallel_outcome] {
        let err =
            engine("fn main():\n    print(\"before\")\n    x := 0..3\n    print(x)\nmain()\n")
                .expect_err("a bare range has no runtime value");
        assert!(err.contains("range can only be used"), "got: {err}");
    }
}

/// R1/B1 — BINARY sockets. `write_bytes` / `read_bytes` carry raw bytes over TCP byte-exactly (the
/// payload the str-only `read` can only ever `Err` on), and `read_bytes` at EOF returns the empty
/// `Ok(b"")` sentinel. M:N-only (std.net requires the OS-thread engine).
#[test]
fn socket_bytes_round_trip_binary_payload() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = vec![0u8; 4];
        stream.read_exact(&mut got).unwrap();
        assert_eq!(got, vec![0u8, 255, 0x80, 10], "the client's binary write");
        stream.write_all(&[0u8, 255, 0x80, 10]).unwrap();
        // Close → the client's second read_bytes sees EOF.
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    payload := bytes([0, 255, 128, 10])
    n := sock.write_bytes(payload)?
    print(\"WROTE:{{n}}\")
    got := sock.read_bytes(16, 5000)?
    print(\"LEN:{{got.len()}}\")
    for x in got:
        print(x)
    eof := sock.read_bytes(16, 5000)?
    print(\"EOF:{{eof.len()}}\")
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("bytes_round_trip", &src);
    server.join().unwrap();
    assert_eq!(
        out, "WROTE:4\nLEN:4\n0\n255\n128\n10\nEOF:0\ndone\n",
        "the binary payload must survive byte-exactly: {out:?}"
    );
}

/// R1/B1 — the ESCAPE HATCH: after the str-only `read` returns its sticky `Err("invalid utf-8 …")`,
/// the undecodable bytes stay CARRIED on the socket. `read_bytes` drains that carry first, so the
/// caller recovers the exact bytes instead of being forced to `close()`. This is what makes B1
/// honestly fixed rather than merely mitigated.
#[test]
fn socket_read_bytes_recovers_the_sticky_invalid_utf8_carry() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"hi\xFF\xFE").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(64, 5000):
        Ok(s): print(\"GOT:[\" + s + \"]\")
        Err(e): print(\"ERR\")
    match sock.read(64, 5000):
        Ok(s): print(\"GOT2:[\" + s + \"]\")
        Err(e): print(\"ERR2\")
    b := sock.read_bytes(64, 5000)?
    print(\"BYTES:{{b.len()}}\")
    for x in b:
        print(x)
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn client()
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("bytes_carry", &src);
    server.join().unwrap();
    assert!(out.contains("GOT:[hi]"), "{out:?}");
    assert!(
        out.contains("ERR2"),
        "the invalid-utf-8 Err is sticky: {out:?}"
    );
    assert!(
        out.contains("BYTES:2\n255\n254\n"),
        "read_bytes must recover the carried undecodable bytes byte-exactly: {out:?}"
    );
}

// ===== cancellation points + the serial cancel drain (N6) =====
// Cancel is delivered at CHECKPOINTS (loop back-edges + blocking/park ops), so a STARTED task always
// runs its straight-line prologue and a REGISTERED `defer` always runs on cancel — on BOTH engines.
// Cross-task stdout ORDER stays nondeterministic on both engines (one print = one locked write,
// line-atomic); only the line SET / exit code / did-the-defer-run are parity.

/// N6 — token-sequenced: `consumer` registers `defer cleanup(s)`, hands `boom` a token, then parks in
/// `ch.recv()`; `boom` faults. The defer is GUARANTEED registered before the fault, so both engines
/// must run it (42). Serial used to print `0`: `run_scheduler`'s `run_child(i)?` propagated the
/// faulting child's error straight out and ABANDONED the parked consumer — never resumed, never
/// cancelled, never unwound.
#[test]
fn parity_defer_runs_on_parked_sibling_when_sibling_faults() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture_parallel(src).expect("the fault is recovered, so the program completes");
    assert_eq!(serial, "42\n", "serial: the parked consumer's defer ran");
    assert_eq!(mn, "42\n", "M:N: the parked consumer's defer ran");
    assert_same_lines(&serial, &mn);
}

/// BUG 1 / the PROBE shape — no token: `consumer` PRINTS, THEN registers `defer cleanup(s)`, then
/// parks; `boom` faults immediately. With cancel observed at EVERY instruction the consumer could be
/// killed between its first statement and its `defer` line, so the defer never registered and cleanup
/// silently never ran (measured: 0/20 on M:N). With CANCELLATION POINTS (loop back-edges + blocking
/// ops) a STARTED task always runs its straight-line prologue, so the defer is registered by
/// construction — deterministically 42 on BOTH engines. The pre-defer `print` must survive on both
/// (line SET parity: a cancelled M:N task's buffer is flushed at its task slot, serial printed live).
#[test]
fn parity_probe_defer_runs_when_cancelled_before_its_defer_line() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn consumer(ch: Channel[int], s: Shared[int]):\n    print(\"consumer started\")\n    defer cleanup(s)\n    ch.recv()\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        parallel:\n            spawn consumer(ch, s)\n            spawn boom()\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture_parallel(src).expect("the fault is recovered, so the program completes");
    assert!(
        serial.contains("42\n"),
        "serial: the started consumer's defer ran: {serial:?}"
    );
    assert!(
        mn.contains("42\n"),
        "M:N: the started consumer's defer ran: {mn:?}"
    );
    // Cross-task ORDER is nondeterministic on both engines; the line SET is the contract.
    assert_same_lines(&serial, &mn);
}

/// `os.exit` inside a CANCELLED task's `defer`: the exit code is identical on both engines and is
/// never demoted to the sibling's catchable fault (Exit > Fault, lowest task index wins — serial's
/// drain reduces exits exactly like M:N's `reduce_task_slots`). Line SET parity only; order is not
/// asserted.
#[test]
fn parity_os_exit_inside_a_cancelled_tasks_defer() {
    let src = "import std.os\n\
               fn bye():\n    print(\"cleanup\")\n    os.exit(7)\n\
               fn consumer(ch: Channel[int], go: Channel[int]):\n    defer bye()\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    r := recover:\n        parallel:\n            spawn consumer(ch, go)\n            spawn boom(go)\n        0\n    print(\"unreachable\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mo, _me, _mr, mc) = run_file_p(&entry);
    let (so, _se, _sr, sc) = run_file(&entry);
    assert_eq!(sc, Some(7), "serial: the cancelled defer's os.exit wins");
    assert_eq!(mc, Some(7), "M:N: the cancelled defer's os.exit wins");
    assert!(
        !so.contains("unreachable") && !mo.contains("unreachable"),
        "os.exit hard-halts: serial={so:?} mn={mo:?}"
    );
    assert_same_lines(&so, &mo);
}

/// The probe shape with the FAULTER SPAWNED FIRST — the shape that exposed the deterministic
/// serial-vs-M:N divergence the cancellation-point change first shipped with. M:N is structurally
/// forced to start every spawned fiber (a scope completes only at `done == total`; `take_runnable`
/// never consults the scope cancel), so `talker` runs its prologue, prints, registers its `defer` and
/// unwinds at its `recv` checkpoint — even though `boom` already faulted. Serial's cancel drain used
/// to SKIP a never-started (`Pending`) sibling, so it emitted `{"0"}` while M:N emitted `{"hi","42"}`
/// (20/20 — near-deterministic, not a rare race). The drain now starts every not-`Done` sibling with
/// the cancel tripped, so both engines run the task and both run its `defer`.
#[test]
fn parity_probe_faulter_spawned_first_still_runs_the_siblings_defer() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn talker(ch: Channel[int], s: Shared[int]):\n    print(\"hi\")\n    defer cleanup(s)\n    ch.recv()\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        parallel:\n            spawn boom()\n            spawn talker(ch, s)\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture_parallel(src).expect("the fault is recovered, so the program completes");
    assert!(serial.contains("42\n"), "serial: the defer ran: {serial:?}");
    assert!(mn.contains("42\n"), "M:N: the defer ran: {mn:?}");
    assert!(
        serial.contains("hi\n"),
        "serial: the prologue ran: {serial:?}"
    );
    assert_same_lines(&serial, &mn);
}

/// A never-started straight-line sibling with NO defer, faulter first: M:N runs it to completion
/// (`Done`, output flushed), so serial must too — the line SET is the parity contract.
#[test]
fn parity_straight_line_sibling_runs_even_when_the_scope_is_already_cancelled() {
    let src = "fn noisy():\n    print(\"hi\")\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn noisy()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture_parallel(src).expect("recovered");
    assert!(serial.contains("hi\n"), "serial: {serial:?}");
    assert_same_lines(&serial, &mn);
}

/// Cancellation points and a NATIVE-driven loop: `xs.map(f)` iterates in RUST (`for e in ..
/// guarded(|vm| vm.invoke_value(f, ..))`, call.rs) and emits no `Op::Jump`, so `jump_checked`'s
/// back-edge cannot fire inside it and a straight-line `f` has no back-edge of its own. Without a
/// checkpoint at the native re-entry a cancelled task burns every remaining element to completion
/// (measured on the first cut of this change: "map finished" printed after the sibling had faulted,
/// 5M callbacks deep). The `guarded` checkpoint delivers the cancel between elements — that Rust
/// loop's back-edge.
///
/// The faulter is spawned FIRST so the scope cancel is already tripped when `work` starts: no timing
/// race, and BOTH engines must abort the map (serial's drain starts `work` with the cancel tripped,
/// M:N pops its still-queued fiber after the cancel). A CPU-bound task that is ALREADY RUNNING when
/// the cancel trips is a different, pre-existing story on serial — it is cooperative, so it simply
/// runs to its next park before any sibling gets to fault (that is why the `while true:` spinner test
/// is M:N-only).
#[test]
fn parity_native_hof_loop_is_cancellable() {
    let src = "fn sq(x: int) -> int:\n    return x * x\n\
               fn work(xs: List[int]):\n    ys := xs.map(sq)\n    print(\"map finished\")\n    print(ys.len())\n\
               fn boom():\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    xs := []\n    i := 0\n    while i < 200000:\n        xs.push(i)\n        i = i + 1\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn work(xs)\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture_parallel(src).expect("recovered");
    assert!(
        !serial.contains("map finished"),
        "serial: the cancelled task's native map must abort at a per-element checkpoint: {serial:?}"
    );
    assert!(
        !mn.contains("map finished"),
        "M:N: the cancelled task's native map must abort at a per-element checkpoint: {mn:?}"
    );
    assert_same_lines(&serial, &mn);
}

/// A NESTED nursery's deadlock, with an outer sibling PARKED and holding a registered `defer`. The
/// nested deadlock reaches the outer level as an ordinary child error on both engines, so both cancel
/// the outer scope and both run the parked sibling's `defer` (42). Locks the N5 boundary: a level's
/// OWN deadlock still tears its fibers down without defers, identically on both engines — but a
/// nested one must not diverge.
#[test]
fn parity_nested_deadlock_cancels_the_outer_parked_siblings_defer() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn a(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn dead(d: Channel[int]):\n    d.recv()\n\
               fn b(go: Channel[int]):\n    go.recv()\n    d := Channel[int]()\n    parallel:\n        spawn dead(d)\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn a(ch, go, s)\n            spawn b(go)\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the deadlock is recovered");
    let mn = run_capture_parallel(src).expect("the deadlock is recovered");
    assert_eq!(serial, "42\n", "serial: the parked sibling's defer ran");
    assert_eq!(mn, "42\n", "M:N: the parked sibling's defer ran");
}

/// A GENUINE deadlock (every task parked, nothing cancelled) is still DETECTED — not hung — on both
/// engines. The cancel drain must never swallow it: it is reported from `run_scheduler_level`'s
/// `None` arm, which never routes through `drain_cancelled_children`.
#[test]
fn parity_genuine_deadlock_is_still_detected() {
    let src = "fn waiter(ch: Channel[int]):\n    ch.recv()\n\
               fn main():\n    ch := Channel[int]()\n    r := recover:\n        parallel:\n            spawn waiter(ch)\n            spawn waiter(ch)\n        0\n    print(\"caught\")\nmain()\n";
    let serial = run_capture(src).expect("the deadlock is recovered");
    let mn = run_capture_parallel(src).expect("the deadlock is recovered");
    assert_eq!(serial, "caught\n");
    assert_same_lines(&serial, &mn);
}

/// A `defer` is the cleanup the cancel exists to RUN — so no cancellation checkpoint fires INSIDE a
/// deferred call (`Vm::deferring` ⇒ `cancel_requested()` is false). Without that, every defer of a
/// task that ends on the NORMAL-return path (or faults on its own) while a sibling has already
/// tripped the scope cancel hit the checkpoint at the top of `guarded` (`run_one_deferred` runs every
/// deferred call through it) with `cancelled` still false: the FIRST (LIFO) deferred call returned
/// `cancelled` BEFORE its body ran, and only the rest of the defers executed. Measured on both
/// engines: `cleanup2` was silently swallowed — arbitrary PARTIAL cleanup (one fd released, the next
/// not), while parity stayed green because both engines dropped it identically.
#[test]
fn parity_every_defer_of_a_normally_returning_task_runs_under_a_tripped_cancel() {
    let src = "fn boom() -> int:\n    return 1 / 0\n\
               fn tidy():\n    defer print(\"cleanup1\")\n    defer print(\"cleanup2\")\n    print(\"start\")\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn tidy()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture_parallel(src).expect("recovered");
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("cleanup1\n") && out.contains("cleanup2\n"),
            "{engine}: EVERY registered defer runs (LIFO), not all-but-the-first: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// Structured concurrency — cancelling a scope cancels its DESCENDANT scopes. A nested `parallel:`
/// entered from a task that is then cancelled used to be UNCANCELLABLE: the nested scope got a fresh
/// cancel flag (M:N) / serial handed the level `cancel = None`, so the nested spinner's back-edge
/// checkpoint had no tripped flag to read and looped forever — the teardown never finished and the
/// program HUNG on BOTH engines (measured: `timeout` on both). The nested scope now INHERITS its
/// enclosing scopes' flags (`JoinScope::ancestors` / `Vm::cancel_outer`; serial inherits the `Arc`).
#[test]
fn parity_nested_nursery_inside_a_cancelled_task_is_cancellable() {
    let src = "fn boom() -> int:\n    return 1 / 0\n\
               fn spin():\n    i := 0\n    while true:\n        i = i + 1\n\
               fn t():\n    parallel:\n        spawn spin()\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn t()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered — must not hang");
    let mn = run_capture_parallel(src).expect("recovered — must not hang");
    assert_eq!(serial, "end\n", "serial: the nested spinner was cancelled");
    assert_eq!(mn, "end\n", "M:N: the nested spinner was cancelled");
}

/// A blocking native (`sleep_ms` / `io.*` / `fs.*` / `request`) is a cancellation checkpoint on BOTH
/// engines — the check sits OUTSIDE the M:N-only offload gate. It used to be M:N-only, so a cancelled
/// SERIAL task slept the full duration (stalling the whole teardown — `sleep_ms(60000)` would freeze
/// it for a minute) and then, having no other checkpoint, ran every straight-line statement AFTER the
/// sleep to completion: `{napper start, napper woke, end}` on serial vs `{napper start, end}` on M:N,
/// deterministically. The line SET is the parity contract.
#[test]
fn parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines() {
    let src = "import std.time\n\
               fn boom() -> int:\n    return 1 / 0\n\
               fn napper():\n    print(\"napper start\")\n    time.sleep_ms(3000)\n    print(\"napper woke\")\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn napper()\n        0\n    print(\"end\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let t0 = std::time::Instant::now();
    let (mn, _me, _mr, _mc) = run_file_p(&entry);
    let (serial, _se, _sr, _sc) = run_file(&entry);
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            !out.contains("napper woke"),
            "{engine}: the cancelled task dies AT the blocking native, and never runs past it: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(3000),
        "neither engine's teardown waits out the cancelled task's 3s sleep: {:?}",
        t0.elapsed()
    );
}

/// Run one engine under a HARD DEADLINE: a scheduler bug in this area is a HANG, and a hung test
/// would just wedge the suite. On timeout we panic with the engine name (the hung VM thread is left
/// to die with the test process).
#[cfg(test)]
fn run_with_deadline(
    engine: &str,
    f: impl FnOnce() -> (String, Result<(), RuntimeError>) + Send + 'static,
) -> (String, Result<(), RuntimeError>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let _ = tx.send(f());
        })
        .expect("failed to spawn the deadline thread");
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .unwrap_or_else(|_| {
            panic!("{engine} HUNG: a defer that can never complete must be REPORTED as a deadlock, never a silent hang")
        })
}

/// A `defer` that can NEVER complete (its body `recv`s on a channel nobody will ever send to) inside
/// a CANCELLED task is REPORTED as a deadlock — never a silent hang. A defer is not itself cancellable
/// (`cancel_requested`'s `deferring == 0`), so on M:N that cleanup demotes and blocks in place; the
/// scope is then incomplete BECAUSE of it, which made the N4 cancel-teardown veto
/// (`any_cancelled_scope_awaiting_drain`, mod.rs) permanent and the M:N engine hung SILENTLY while
/// serial reported (measured: M:N `timeout` rc=124, serial rc=1). The veto is now bounded to the
/// trip→`cancel_drain` window (an UNDRAINED PARKED fiber), so the quiesce is detected, the demoted
/// fiber faults in place, and — the task being already cancelled — its error is swallowed and the
/// SIBLING'S real fault is what propagates, identically on both engines.
#[test]
fn parity_a_defer_that_can_never_complete_is_reported_not_hung() {
    let src = "fn cleanup(dead: Channel[int]):\n    print(\"CLEANUP-ENTER\")\n    dead.recv()\n    print(\"CLEANUP-DONE\")\n\
               fn consumer(ch: Channel[int], go: Channel[int], dead: Channel[int]):\n    defer cleanup(dead)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    dead := Channel[int]()\n    parallel:\n        spawn consumer(ch, go, dead)\n        spawn boom(go)\nmain()\n";
    let s = src.to_string();
    let (mn, mn_res) = run_with_deadline("M:N", move || run_program_parallel(&s));
    let s = src.to_string();
    let (serial, se_res) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out, res) in [("serial", &serial, &se_res), ("M:N", &mn, &mn_res)] {
        assert!(
            out.contains("CLEANUP-ENTER\n"),
            "{engine}: the cleanup RAN (it is the cancel's whole point): {out:?}"
        );
        assert!(
            !out.contains("CLEANUP-DONE\n"),
            "{engine}: it can never finish — its recv has no sender: {out:?}"
        );
        let err = res.as_ref().expect_err("the program must FAIL, not hang");
        assert!(
            err.message.contains("index 9 out of bounds"),
            "{engine}: the sibling's real fault is what is reported (the stuck cleanup's own error \
             is swallowed with its cancelled task): {err:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// The regression guard for the demote-path cancel fix: a `defer` whose body BLOCKS (here
/// `time.sleep_ms`) in a CANCELLED task runs to COMPLETION — cleanup is never truncated mid-body. The
/// M:N demote loops used to read the raw `self.cancel` flag instead of `cancel_requested()`, so the
/// blocking op inside the cleanup saw the already-tripped scope cancel and aborted the defer at that
/// call: `CLEANUP-ENTER` then nothing, sentinel 0 on M:N vs 42 on serial. Blocking cleanup is what
/// real cleanup DOES (close a socket, send a final message, flush).
#[test]
fn parity_a_blocking_defer_body_completes_when_the_task_is_cancelled() {
    let src = "import std.concurrency\nimport std.time\n\
               fn cleanup(s: Shared[int]):\n    print(\"CLEANUP-ENTER\")\n    time.sleep_ms(20)\n    s.set(42)\n    print(\"CLEANUP-DONE\")\n\
               fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(\"sentinel=\" + str(s.get()))\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mn, _me, _mr, _mc) = run_file_p(&entry);
    let (serial, _se, _sr, _sc) = run_file(&entry);
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("CLEANUP-DONE\n") && out.contains("sentinel=42\n"),
            "{engine}: a blocking op inside a defer must NOT truncate the cleanup: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// The cleanup's OWN work must survive too: a `parallel:` (or `spawn`) opened INSIDE a cancelled task's
/// `defer` runs to completion on BOTH engines. The `deferring > 0` suppression that makes a defer
/// uncancellable is per-`Vm` and does NOT cross the airlock — a worker fiber is a fresh `Vm` with
/// `deferring == 0` — while the cancel-flag chain DOES cross it (`Vm::scope_ancestors` →
/// `JoinScope::ancestors` → `cancel_outer`). So the child of a cleanup's nursery inherited the
/// already-tripped enclosing flag and died at its first checkpoint (back-edge / blocking op): M:N
/// printed `CLEANUP-ENTER|CLEANUP-DONE|sentinel=0` (the child silently dropped, rc 0) against serial's
/// `sentinel=42` — serial severs the flag in a defer (`run_scheduler`'s `in_defer`). `scope_ancestors`
/// now severs identically, so cleanup that DELEGATES is as uncancellable as cleanup that blocks inline.
#[test]
fn parity_a_nursery_inside_a_cancelled_tasks_defer_runs_to_completion() {
    let src = "import std.concurrency\n\
               fn setit(s: Shared[int]):\n    i := 0\n    while i < 1000:\n        i = i + 1\n    s.set(42)\n    print(\"CHILD-DONE\")\n\
               fn cleanup(s: Shared[int]):\n    print(\"CLEANUP-ENTER\")\n    parallel:\n        spawn setit(s)\n    print(\"CLEANUP-DONE\")\n\
               fn worker(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn worker(ch, go, s)\n            spawn boom(go)\n        0\n    print(\"sentinel=\" + str(s.get()))\nmain()\n";
    let s = src.to_string();
    let (mn, _) = run_with_deadline("M:N", move || run_program_parallel(&s));
    let s = src.to_string();
    let (serial, _) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("CHILD-DONE\n") && out.contains("sentinel=42\n"),
            "{engine}: a task spawned by a cancelled task's cleanup must NOT inherit that cancel: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// N4 (demoted half), at the PROGRAM level: a DEMOTED fiber (a `recv` inside a native HOF callback —
/// `blocked_native`, not `parked`) whose cancel flag is tripped is about to resume and unwind, which is
/// live progress `is_deadlocked`'s counters cannot see. Without the `any_demoted_cancel_pending` veto
/// the quiesce between the faulting sibling's `finish` and that fiber's next `DEMOTE_POLL_BACKOFF` poll
/// reads as a deadlock, and `flag_deadlock` then drops EVERY parked fiber in the sched — including the
/// INNOCENT outer-scope sibling `b`, which is waiting for the value the cleanup is about to send. Fails
/// 7/8 runs with the veto removed (`B-GOT` replaced by a bogus nursery deadlock error).
#[test]
fn parity_a_cancel_wakeable_demoted_fiber_is_not_a_deadlock() {
    let src = "fn b(ch2: Channel[int]):\n    v := ch2.recv()\n    print(\"B-GOT=\" + str(v))\n\
               fn acleanup(ch2: Channel[int]):\n    ch2.send(7)\n    print(\"A-CLEANUP-DONE\")\n\
               fn a(ch: Channel[int], go: Channel[int], ch2: Channel[int]):\n    defer acleanup(ch2)\n    go.send(0)\n    xs := [1]\n    ys := xs.map(fn(x: int) -> int: ch.recv())\n    print(\"A-NEVER\")\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    ch := Channel[int]()\n    ch2 := Channel[int]()\n    go := Channel[int]()\n    parallel:\n        spawn b(ch2)\n        r := recover:\n            parallel:\n                spawn a(ch, go, ch2)\n                spawn boom(go)\n            0\nmain()\n";
    let s = src.to_string();
    let (mn, _) = run_with_deadline("M:N", move || run_program_parallel(&s));
    let s = src.to_string();
    let (serial, _) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("A-CLEANUP-DONE\n") && out.contains("B-GOT=7\n"),
            "{engine}: an innocent parked sibling must not be faulted while a demoted fiber is \
             resuming on a tripped cancel: {out:?}"
        );
        assert!(
            !out.contains("deadlock"),
            "{engine}: no false deadlock: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// KNOWN LIMIT (C5 — no snapshot-park inside a native callback), pinned so it cannot silently change:
/// a `defer` body runs `guarded` (the LIFO unwind drain is host-stack state), so on the SERIAL engine a
/// `recv` inside a cleanup CANNOT park and yield to the sibling that would feed it — it faults in place.
/// M:N has no such limit (the same recv DEMOTES and completes). Both engines run the defer and both
/// report/handle it; what differs is whether the cleanup can finish. Recorded in docs/gaps.md — the fix
/// is C5 (a resumable native re-entry), not this branch. Two shapes, both measured:
///
/// * no cancellation at all (pre-existing on `main`): serial faults the recv with the C5 deadlock
///   message, M:N completes the cleanup;
/// * a CANCELLED task (this branch made serial run the defer at all — on `main` it ran nothing): the
///   in-place fault is swallowed with the cancelled task, so serial's cleanup stops at the recv.
#[test]
fn c5_limit_a_defer_that_recvs_from_a_live_sibling_cannot_park_on_serial() {
    let src = "import std.concurrency\nimport std.time\n\
               fn cleanup(c1: Channel[int], s: Shared[int]):\n    v := c1.recv()\n    s.set(v)\n    print(\"CLEANUP-DONE\")\n\
               fn t1(c1: Channel[int], s: Shared[int]):\n    defer cleanup(c1, s)\n    print(\"T1-BODY\")\n\
               fn t2(c1: Channel[int]):\n    time.sleep_ms(15)\n    c1.send(42)\n    print(\"T2-SENT\")\n\
               fn main():\n    s := Shared(0)\n    c1 := Channel[int]()\n    parallel:\n        spawn t1(c1, s)\n        spawn t2(c1)\n    print(\"sentinel=\" + str(s.get()))\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mn, _me, _mr, _mc) = run_file_p(&entry);
    let (serial, _se, sr, _sc) = run_file(&entry);
    assert!(
        mn.contains("CLEANUP-DONE\n") && mn.contains("sentinel=42\n"),
        "M:N: a demoted recv inside a defer completes: {mn:?}"
    );
    assert!(
        !serial.contains("CLEANUP-DONE\n"),
        "serial: C5 — the recv cannot park inside the unwind: {serial:?}"
    );
    let err = sr.expect_err("serial: the stuck cleanup is REPORTED, never a silent hang");
    assert!(
        format!("{err:?}").contains("deadlock"),
        "serial: reported as a deadlock at the recv site: {err:?}"
    );
}

// ===== R2 — std.io Writer / file-handle type (buffered + streaming file output) =====

/// R2 — `create(path)` opens a truncating write handle; `write` + `close` land the bytes; `read_file`
/// reads them back. Serial and M:N each.
#[test]
fn writer_create_roundtrip_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("out.txt");
        let src = format!(
            "import create, read_file from std.io\nfn main():\n    w := create(\"{}\")?\n    w.write(\"hi\")?\n    w.close()?\n    print(read_file(\"{}\")?)\nmain()\n",
            f.display(),
            f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "hi\n");
    }
}

/// R2 — `append` never truncates: create+write "a", close; append+write "b", close; file == "ab".
#[test]
fn writer_append_no_truncate_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("log.txt");
        let src = format!(
            "import create, append, read_file from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.write(\"a\")?\n    w.close()?\n    w2 := append(\"{f}\")?\n    w2.write(\"b\")?\n    w2.close()?\n    print(read_file(\"{f}\")?)\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "ab\n");
    }
}

/// R2 — a method on a CLOSED writer is a clean `Result::Err` (contains "closed writer"), NOT a panic.
#[test]
fn writer_use_after_close_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("x.txt");
        let src = format!(
            "import create from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.close()?\n    match w.write(\"z\"):\n        Ok(n): print(\"wrote \" + str(n))\n        Err(e): print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (should be a clean Err, not a fault): {r:?}"
        );
        assert!(
            out.contains("closed writer"),
            "want a clean closed-writer Err, got: {out:?}"
        );
    }
}

/// R2 — `write_bytes` round-trips arbitrary binary through `read_bytes`.
#[test]
fn writer_write_bytes_binary_roundtrip_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("bin");
        let src = format!(
            "import create, read_bytes from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.write_bytes(bytes([0, 1, 255]))?\n    w.close()?\n    b := read_bytes(\"{f}\")?\n    print(str(b.len()) + \":\" + str(b[0]) + \",\" + str(b[1]) + \",\" + str(b[2]))\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "3:0,1,255\n");
    }
}

/// R2 — `buffered(stdout())`: N writes then one flush; the whole batch reaches stdout, and the output
/// is BYTE-IDENTICAL serial vs M:N (single task → exact-match). Routes through `emit_out` (the oracle).
#[test]
fn buffered_stdout_one_flush_byte_identical() {
    let t = TmpDir::new();
    let src = "import stdout, buffered from std.io\nfn main():\n    bw := buffered(stdout())\n    bw.write(\"a\")?\n    bw.write(\"b\")?\n    bw.write(\"c\")?\n    bw.flush()?\nmain()\n";
    let entry = t.write("main.chz", src);
    let (serial, _se, sr, _sc) = run_file(&entry);
    let (mn, _me, mr, _mc) = run_file_p(&entry);
    assert!(sr.is_ok() && mr.is_ok(), "faulted: s={sr:?} m={mr:?}");
    assert_eq!(serial, "abc");
    assert_eq!(serial, mn, "serial vs M:N byte-identical");
}

/// R2 — a buffered file writer dropped WITHOUT close still lands its tail (best-effort drop-flush at
/// program exit / heap teardown). Both engines.
#[test]
fn buffered_file_drop_flushes_best_effort_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("drop.txt");
        // Write via a buffered file writer, NEVER flush/close — the handle just goes out of scope.
        let src = format!(
            "import create, buffered from std.io\nfn go():\n    bw := buffered(create(\"{f}\")?)\n    bw.write(\"tail\")?\nfn main():\n    go()\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (_out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        let got = std::fs::read_to_string(&f).unwrap_or_default();
        assert_eq!(got, "tail", "drop-flush must land the buffered tail");
    }
}

/// R2 — `import Writer from std.io` (a pure TYPE with no runtime member value) and `import buffered
/// from std.io` in a RUNNING program: no runtime fault (the `bind_import` skip). Both engines.
#[test]
fn import_writer_type_and_buffered_runs_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let src = "import Writer, buffered, stdout from std.io\nfn tag(w: Writer) -> Writer:\n    return w\nfn main():\n    bw := tag(buffered(stdout()))\n    bw.write(\"ok\")?\n    bw.flush()?\nmain()\n";
        let entry = t.write("main.chz", src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (bind_import skip missing?): {r:?}");
        assert_eq!(out, "ok");
    }
}

/// R2 — a shared `Writer` written by two spawned tasks: order is UNSPECIFIED (Fork A), so compare the
/// line MULTISET (`assert_same_lines`), not exact-match. Both engines.
#[test]
fn shared_writer_across_tasks_same_lines() {
    let t = TmpDir::new();
    let f = t.0.join("shared.txt");
    let src = format!(
        "import create, read_file from std.io\nfn main():\n    w := create(\"{f}\")?\n    parallel:\n        spawn:\n            w.write(\"A\\n\")?\n        spawn:\n            w.write(\"B\\n\")?\n    w.close()?\n    print(read_file(\"{f}\")?)\nmain()\n",
        f = f.display()
    );
    let entry = t.write("main.chz", &src);
    let (serial, _se, sr, _sc) = run_file(&entry);
    let (mn, _me, mr, _mc) = run_file_p(&entry);
    assert!(sr.is_ok() && mr.is_ok(), "faulted: s={sr:?} m={mr:?}");
    assert_same_lines(&serial, &mn);
}

/// R2 — `create` into a nonexistent directory is a clean `Result::Err`, not a panic. Both engines.
#[test]
fn create_into_missing_dir_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("no_such_dir").join("x.txt");
        let src = format!(
            "import create from std.io\nfn main():\n    match create(\"{f}\"):\n        Ok(w): print(\"opened\")\n        Err(e): print(\"ERR\")\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (should be a clean Err): {r:?}");
        assert_eq!(out, "ERR\n");
    }
}

/// R2 — a full `Writer` program TYPE-CHECKS clean through `check_graph` (run_file skips the checker, so
/// this is the guard that the `Ty::Writer` method arm + the harvested method-table seed actually
/// resolve `w.write(...)`/`w.close(...)` at check time — the CLI path).
#[test]
fn writer_program_type_checks_clean() {
    // `main` must return Result to use `?` (no `fn main` exception — see the try-in-nil-fn soundness fix).
    let src = "import create, buffered, stdout, Writer from std.io\n\
               fn tag(w: Writer) -> Writer:\n    return w\n\
               fn main() -> int!:\n    w := create(\"/tmp/x\")?\n    w.write(\"a\")?\n    w.write_bytes(bytes([1]))?\n    w.flush()?\n    w.close()?\n    bw := tag(buffered(stdout(), 4096))\n    bw.write(\"b\")?\n    bw.flush()?\n    return Ok(0)\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let r = crate::checker::check_graph(&graph);
    assert!(
        r.is_ok(),
        "a well-typed Writer program must check clean, got: {r:?}"
    );
}

/// R2 — the bare `Writer` annotation is IMPORT-GATED: a program that names `Writer` WITHOUT importing
/// std.io is rejected at check time with the `import std.io` hint (mirrors Socket/Listener gating).
#[test]
fn writer_annotation_requires_import() {
    let src = "fn tag(w: Writer) -> Writer:\n    return w\nfn main():\n    print(\"hi\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    match crate::checker::check_graph(&graph) {
        Ok(()) => panic!("bare `Writer` without `import std.io` must be rejected"),
        Err(errs) => assert!(
            errs.iter().any(|e| e.message.contains("import std.io")
                || e.message.contains("unknown type 'Writer'")),
            "want an import-std.io hint, got: {errs:?}"
        ),
    }
}

// ===== R2b — std.io Reader / file-handle type (line/chunk streaming file INPUT) =====

/// R2b — `open(path)` opens a read-only handle; `read_line()` streams the file line-by-line
/// (trailing newline stripped, `None` = EOF); `close()` releases the fd. Serial and M:N each.
#[test]
fn reader_open_read_line_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            Some(ln): print(ln)\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\ntwo\nthree\n");
    }
}

/// R2b — a file whose last line has NO trailing newline: `read_line` still yields it, then `None`.
#[test]
fn reader_read_line_no_trailing_newline_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "a\nb").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            Some(ln): print(\"[\" + ln + \"]\")\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "[a]\n[b]\n");
    }
}

/// R2b — line-terminator stripping matches the module-level `io.read_line` UNCONDITIONALLY: a CRLF
/// (`\r\n`) line and a final bare-`\r` line (classic-Mac / no trailing `\n`) both come back with the
/// `\r` gone. Guards the anti-drift contract (a nested `\r`-only-if-`\n` strip retained the bare `\r`).
#[test]
fn reader_read_line_strips_bare_cr_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "a\r\nb\r").unwrap(); // CRLF line, then a bare-CR final line (no \n)
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            Some(ln): print(\"[\" + ln + \"]\")\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "[a]\n[b]\n");
    }
}

/// R2b — `r.lines()` is a BODIED Chezzi method on the `native struct Reader` (mixing Rust-backed
/// `native fn` sigs with a pure-Chezzi generator method on one native handle). It streams the file
/// line-by-line lazily (a generator over `read_line()`). Both engines.
#[test]
fn reader_lines_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    for ln in r.lines():\n        print(ln)\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\ntwo\nthree\n");
    }
}

/// R2b — `r.lines()` yields LAZILY: an early `break` after the first line must NOT drain the file
/// into a list. Proves the generator suspends between lines (does not snapshot the file). Both engines.
#[test]
fn reader_lines_lazy_early_break_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    for ln in r.lines():\n        print(ln)\n        break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\n");
    }
}

/// R2b — `read_bytes(n)` chunks the file: exactly-n bytes until the short final chunk, then empty
/// bytes (`len == 0`) = EOF. Both engines.
#[test]
fn reader_read_bytes_chunk_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("bin");
        std::fs::write(&f, [10u8, 20, 30, 40, 50, 60]).unwrap(); // 6 bytes
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    c1 := r.read_bytes(4)?\n    c2 := r.read_bytes(4)?\n    c3 := r.read_bytes(4)?\n    print(str(c1.len()) + \":\" + str(c1[0]) + \",\" + str(c1[3]))\n    print(str(c2.len()) + \":\" + str(c2[0]) + \",\" + str(c2[1]))\n    print(str(c3.len()))\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "4:10,40\n2:50,60\n0\n");
    }
}

/// R2b — `read_bytes` on a CLOSED reader is a clean `Result::Err` (contains "closed reader"), NOT a
/// panic. Both engines.
#[test]
fn reader_use_after_close_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("x.txt");
        std::fs::write(&f, "data").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    r.close()?\n    match r.read_bytes(4):\n        Ok(b): print(\"got \" + str(b.len()))\n        Err(e): print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (should be a clean Err, not a fault): {r:?}"
        );
        assert!(
            out.contains("closed reader"),
            "want a clean closed-reader Err, got: {out:?}"
        );
    }
}

/// R2b — `open` on a nonexistent file is a clean `Result::Err`, not a panic. Both engines.
#[test]
fn reader_open_missing_file_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("no_such_dir").join("x.txt");
        let src = format!(
            "import open from std.io\nfn main():\n    match open(\"{f}\"):\n        Ok(r): print(\"opened\")\n        Err(e): print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (should be a clean Err): {r:?}");
        assert!(
            out.starts_with("ERR:"),
            "want a clean open Err, got: {out:?}"
        );
    }
}

/// R2b — `import Reader from std.io` (a pure TYPE with no runtime member value) + send the handle
/// across a `spawn` boundary and read in the task: no runtime fault (the `bind_import` skip + airlock
/// sites). Both engines.
#[test]
fn import_reader_type_and_send_across_spawn_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file_p] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let src = format!(
            "import Reader, open from std.io\nfn first(r: Reader) -> Option[str]:\n    return r.read_line()\nfn main():\n    r := open(\"{f}\")?\n    parallel:\n        spawn:\n            match first(r):\n                Some(ln): print(ln)\n                None: print(\"empty\")\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (bind_import/airlock missing?): {r:?}"
        );
        assert_eq!(out, "hello\n");
    }
}

/// R2b — a full `Reader` program TYPE-CHECKS clean through `check_graph` (the CLI path; `run_file`
/// skips the checker). Guards that `Ty::Reader`'s method arm + the harvested method-table seed resolve
/// `r.read_line()`/`r.read_bytes(..)`/`r.close()` at check time.
#[test]
fn reader_program_type_checks_clean() {
    // `main` must return Result to use `?` (no `fn main` exception — see the try-in-nil-fn soundness fix).
    let src = "import open, Reader from std.io\n\
               fn tag(r: Reader) -> Reader:\n    return r\n\
               fn main() -> int!:\n    r := tag(open(\"/tmp/x\")?)\n    match r.read_line():\n        Some(ln): print(ln)\n        None: print(\"eof\")\n    b := r.read_bytes(8)?\n    print(str(b.len()))\n    r.close()?\n    return Ok(0)\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let r = crate::checker::check_graph(&graph);
    assert!(
        r.is_ok(),
        "a well-typed Reader program must check clean, got: {r:?}"
    );
}

/// R2b — the bare `Reader` annotation is IMPORT-GATED: naming `Reader` WITHOUT importing std.io is
/// rejected at check time with the `import std.io` hint (mirrors Writer gating).
#[test]
fn reader_annotation_requires_import() {
    let src = "fn tag(r: Reader) -> Reader:\n    return r\nfn main():\n    print(\"hi\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    match crate::checker::check_graph(&graph) {
        Ok(()) => panic!("bare `Reader` without `import std.io` must be rejected"),
        Err(errs) => assert!(
            errs.iter().any(|e| e.message.contains("import std.io")
                || e.message.contains("unknown type 'Reader'")),
            "want an import-std.io hint, got: {errs:?}"
        ),
    }
}

/// B3 — a fn-LOCAL aggregate mutated inside a spawn: task deep-copies per task (the airlock), so both
/// engines AGREE (serial == M:N == 3): the parent's list is untouched by the isolated task copy. Task 1
/// gave the MODULE-GLOBAL form the same isolation (both engines snapshot module globals per task now —
/// see `serial_module_global_direct_mutation_forms_isolate_parity`), so this fn-local case is just the
/// sibling boundary: a captured local was already deep-copied on both engines and still is.
#[test]
fn module_global_aggregate_mutation_in_task_parity() {
    let src = "fn main():\n    xs := [1, 2, 3]\n    parallel:\n        spawn:\n            xs.push(99)\n    print(xs.len())\nmain()\n";
    assert_parity_out(src, "3\n");
}

/// Task 1 — a module-global mutated via a cross-module FN CALL from a task isolates on BOTH engines.
/// `counter.bump()` reassigns `count` inside the helper module; each spawned task snapshots the module
/// globals (serial now deep-copies at the spawn boundary exactly as M:N does), so the parent's post-join
/// read sees the PRE-task value. Before the fix serial printed 2 (shared) / M:N printed 0 (snapshot);
/// after it both print 0. This program COMPILES today (a cross-module fn-call reassign escaped every
/// old checker gate) and used to DIVERGE at runtime — the exact gaps.md §B3 (A) residual.
#[test]
fn serial_module_global_method_call_mutation_isolates_parity() {
    let out = assert_parity_file(
        &[
            (
                "counter.chz",
                "count := 0\nfn bump():\n    count = count + 1\nfn get() -> int:\n    return count\n",
            ),
            (
                "main.chz",
                "import counter\nfn main():\n    parallel:\n        spawn:\n            counter.bump()\n            counter.bump()\n    print(counter.get())\nmain()\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "0\n");
}

/// Task 1 — gaps.md §B3 (D): aliasing a module-global aggregate into a task-local (`local := xs`) then
/// mutating the alias. The receiver root resolves to `local`, so the old flow-blind gate never caught it
/// and the program silently diverged (serial 4 / M:N 3). With serial snapshotting module globals per
/// task, the alias points at the task's OWN copy — both engines now read 3.
#[test]
fn serial_module_global_task_local_alias_isolates_parity() {
    let src = "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            local := xs\n            local.push(99)\n    print(xs.len())\nmain()\n";
    let out = parity_entry(src);
    assert_eq!(out, "3\n");
}

/// Task 1 — the ESCAPE HATCH must survive the serial snapshot change: `Atomic`/`Shared`/`Channel` cross
/// the spawn boundary by shared `Arc` core (via `to_snap`), NOT by deep copy, so a task-side mutation IS
/// visible to the parent. Two tasks each `add(1)` to a module-global `Atomic`; the parent reads 2 on both
/// engines. This is the regression guard for trap #1 (a hand-rolled copier would clone the Arc away → 0).
#[test]
fn atomic_incremented_in_task_visible_to_parent_parity() {
    let src = "import std.concurrency\na := Atomic(0)\nfn main():\n    parallel:\n        spawn:\n            a.add(1)\n        spawn:\n            a.add(1)\n    print(a.load())\nmain()\n";
    let out = parity_entry(src);
    assert_eq!(out, "2\n");
}

/// Task 1 — direct in-block mutation forms (previously REJECTED by the frozen-module-global gates, now
/// AtomicInt — monomorphic lock-free int atomic. Basic load/store/exchange/cas roundtrip, both engines.
#[test]
fn atomic_int_roundtrip_parity() {
    let src = "import std.concurrency\nfn main():\n    a := AtomicInt(5)\n    a.store(10)\n    print(a.load())\n    print(a.exchange(20))\n    print(a.cas(20, 99))\n    print(a.load())\nmain()\n";
    assert_eq!(parity_entry(src), "10\n10\ntrue\n99\n");
}

/// AtomicInt.add MUST keep the i64-overflow FAULT (checked CAS-loop, NOT raw fetch_add that wraps).
#[test]
fn atomic_int_add_overflow_parity() {
    let src = "import std.concurrency\nfn main():\n    a := AtomicInt(9223372036854775807)\n    print(a.add(1))\nmain()\n";
    let msg = parity_entry_fault(src);
    assert!(msg.contains("integer overflow in Add"), "{msg}");
}

/// AtomicInt.sub MUST keep the i64-overflow FAULT symmetrically (i64::MIN - 1).
#[test]
fn atomic_int_sub_overflow_parity() {
    let src = "import std.concurrency\nfn main():\n    a := AtomicInt(-9223372036854775808)\n    print(a.sub(1))\nmain()\n";
    let msg = parity_entry_fault(src);
    assert!(msg.contains("integer overflow in Sub"), "{msg}");
}

/// High-contention: 8 tasks × 10000 `add(1)` on one shared AtomicInt == 80000 exactly (lock-free
/// correctness — a broken non-atomic RMW would lose updates under M:N).
#[test]
fn atomic_int_contention_parity() {
    let src = "import std.concurrency\na := AtomicInt(0)\nfn main():\n    parallel:\n        for _ in 0..8:\n            spawn:\n                for _ in 0..10000:\n                    a.add(1)\n    print(a.load())\nmain()\n";
    assert_eq!(parity_entry(src), "80000\n");
}

/// Reserved-name hole guard: `import AtomicInt from std.concurrency` must RUN (bind_import skip),
/// not check-pass-then-runtime-trap. Both engines.
#[test]
fn atomic_int_from_import_runs_parity() {
    let src = "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(0)\n    a.add(3)\n    print(a.load())\nmain()\n";
    assert_eq!(parity_entry(src), "3\n");
}

/// Wide-int returns: a value outside ±2^62 (a legal Chezzi int, boxed as Obj::BigInt) must round-trip
/// through load/exchange/add — the value-producing arms MUST box via make_int, not Value::int (inline-
/// only: debug-asserts + release-truncates for |n| >= 2^62). 2^62 = 4611686018427387904; INT_MAX_INLINE
/// = 2^62-1. Mirrors Mutex-backed Atomic, which is correct via from_wire→make_int.
#[test]
fn atomic_int_wide_value_returns_parity() {
    // load a wide stored value
    assert_eq!(
        parity_entry(
            "import std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387904)\n    print(a.load())\nmain()\n"
        ),
        "4611686018427387904\n",
    );
    // exchange returns the wide OLD value
    assert_eq!(
        parity_entry(
            "import std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387904)\n    print(a.exchange(0))\n    print(a.load())\nmain()\n"
        ),
        "4611686018427387904\n0\n",
    );
    // add carries the counter across the inline boundary (no i64 overflow → must return boxed)
    assert_eq!(
        parity_entry(
            "import std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387903)\n    print(a.add(1))\nmain()\n"
        ),
        "4611686018427387904\n",
    );
    // sub carries below the negative inline boundary
    assert_eq!(
        parity_entry(
            "import std.concurrency\nfn main():\n    a := AtomicInt(-4611686018427387904)\n    print(a.sub(1))\nmain()\n"
        ),
        "-4611686018427387905\n",
    );
}

/// deleted): list `.push`, map index-assign, struct field-assign, set `.add`, bytearray `.extend`, and a
/// bare reassign. Each mutates the task's OWN module-global copy → invisible to the parent → serial == M:N.
#[test]
fn serial_module_global_direct_mutation_forms_isolate_parity() {
    // list .push
    assert_eq!(
        parity_entry(
            "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            xs.push(99)\n    print(xs.len())\nmain()\n"
        ),
        "3\n",
    );
    // map index-assign
    assert_eq!(
        parity_entry(
            "m := {1: 2}\nfn main():\n    parallel:\n        spawn:\n            m[1] = 9\n    print(m[1])\nmain()\n"
        ),
        "2\n",
    );
    // struct field-assign
    assert_eq!(
        parity_entry(
            "struct Box:\n    n: int\ns := Box(0)\nfn main():\n    parallel:\n        spawn:\n            s.n = 9\n    print(s.n)\nmain()\n"
        ),
        "0\n",
    );
    // set .add
    assert_eq!(
        parity_entry(
            "st := {1, 2}\nfn main():\n    parallel:\n        spawn:\n            st.add(9)\n    print(st.len())\nmain()\n"
        ),
        "2\n",
    );
    // bytearray .extend
    assert_eq!(
        parity_entry(
            "ba := bytearray()\nfn main():\n    parallel:\n        spawn:\n            ba.extend([1, 2, 3])\n    print(ba.len())\nmain()\n"
        ),
        "0\n",
    );
    // bare reassign
    assert_eq!(
        parity_entry(
            "g := 0\nfn main():\n    parallel:\n        spawn:\n            g = g + 1\n    print(g)\nmain()\n"
        ),
        "0\n",
    );
}

/// Task 1 — a module global mutated through a directly-spawned free-fn callee (`spawn worker()` where
/// `worker` does `m[1] = 9`) — the old transitive-scan gate's job. Now it just isolates: the callee runs
/// against the task's module-global copy, so the parent's map is untouched on both engines.
#[test]
fn serial_module_global_spawned_callee_mutation_isolates_parity() {
    let src = "m := {1: 2}\nfn worker():\n    m[1] = 9\nfn main():\n    parallel:\n        spawn worker()\n    print(m[1])\nmain()\n";
    assert_eq!(parity_entry(src), "2\n");
}

/// Task 1 — a NESTED serial spawn (a task that itself opens a `parallel:` nursery and spawns) still
/// isolates the module global at every level: the inner task copies the (already-copied) outer view, so
/// its mutation is invisible even to the intermediate task. Guards the recursion in the snapshot path.
#[test]
fn nested_serial_spawn_module_global_isolates_parity() {
    let src = "g := 0\nfn inner():\n    g = g + 100\nfn outer():\n    parallel:\n        spawn inner()\n    g = g + 1\nfn main():\n    parallel:\n        spawn outer()\n    print(g)\nmain()\n";
    assert_eq!(parity_entry(src), "0\n");
}

/// Task 1 — a task that mutates its module-global copy, PARKS on a channel recv, resumes, then reads the
/// global keeps its per-fiber snapshot across the park (the copy lives in `FiberCtx`, travels with the
/// fiber). Both engines agree the task sees its own mutation (1) and the parent sees the untouched global
/// (0). Guards trap #3 (snapshot survives a park).
#[test]
fn channel_park_keeps_module_snapshot_parity() {
    let src = "\
import std.concurrency
g := 0
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            g = g + 1
            v := ch.recv()
            print(\"task sees {g} got {v}\")
        spawn:
            ch.send(7)
    print(\"parent sees {g}\")
main()
";
    // task-order buffered output (decision F): the task's line flushes before the parent's read.
    assert_eq!(parity_entry(src), "task sees 1 got 7\nparent sees 0\n");
}

/// W6-2 (was: …`reads_frozen_parity`, expecting `0`) — a task that mutates a module global and THEN
/// opens a NESTED `parallel:` gives its grandchild the TASK's CURRENT view (1), at every depth, on both
/// engines. The old `0` was a MEMOIZATION ARTIFACT, not design: `ensure_snapshot` memoized the first
/// nursery's snapshot forever and every later worker/nested nursery replayed that frozen `Arc` (decision
/// G1), so the grandchild read the pre-mutation value. A task now snapshots FRESH at its own `spawn`,
/// from the view of whoever spawned it — so the nested rule is uniform and Go-like. Per-task
/// ISOLATION is unchanged (`nested_serial_spawn_module_global_isolates_parity` still reads `0`).
#[test]
fn nested_serial_spawn_mutation_before_nested_reads_fresh_parity() {
    let src = "g := 0\nfn worker():\n    g = g + 1\n    parallel:\n        spawn:\n            print(g)\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
    assert_eq!(parity_entry(src), "1\n");
}

/// W6-2 (was: …`reads_frozen_parity`, expecting `0\n0\n`) — TWO sequential top-level nurseries with
/// ordinary (non-task) parent code mutating an imported module's global BETWEEN them: the second
/// nursery's task sees the MUTATED value (2 * 100). The old frozen `0` came from the never-invalidated
/// `snapshot_memo`; a module-slot write now drops the cache, so each nursery snapshots fresh. Both
/// engines agree by construction (they share the one `ensure_snapshot` choke point).
#[test]
fn sequential_mutation_between_nurseries_reads_fresh_parity() {
    let out = assert_parity_file(
        &[
            (
                "counter.chz",
                "count := 0\nfn bump():\n    count = count + 1\nfn get() -> int:\n    return count\n",
            ),
            (
                "main.chz",
                "import counter\nfn main():\n    parallel:\n        spawn:\n            print(counter.get())\n    counter.bump()\n    counter.bump()\n    parallel:\n        spawn:\n            print(counter.get() * 100)\nmain()\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "0\n200\n");
}

/// W6-19 — a spawned task whose FIRST module-global access is a WRITE. `Op::GetGlobalSlot` calls
/// `ensure_module_faulted`, but `DefineGlobalSlot`/`SetGlobalSlot` did not, so on M:N the write indexed
/// an unfaulted (empty-slots) worker module and PANICKED the pool thread
/// (`index out of bounds: the len is 0`) → `internal error: a parallel task panicked`, while `--serial`
/// printed the right answer. Rooted at `set_global_slot` (one guard, all callers).
#[test]
fn spawn_task_first_global_access_is_write_parity() {
    let src = "g: int = 1\nfn worker():\n    g = 99\n    print(\"worker g =\", g)\nfn main():\n    parallel:\n        spawn worker()\n    print(\"parent g =\", g)\nmain()\n";
    assert_eq!(parity_entry(src), "worker g = 99\nparent g = 1\n");
}

/// W6-2 — the PIN INSTANT, and the reason it exists: a task's view is pinned at its own `spawn`, so an
/// OUTER nursery's queued task keeps that view even though the two engines PREPARE it at different
/// program points — serial at the outer nursery's own join (post-`g = 2`), M:N at the nested nursery's
/// join via `early_enlist_outer` (pre-`g = 2`). Both `A` (outer, spawned before the mutation) and the
/// nested `B` therefore read `1` while the parent reads `2`, byte-identically. Without the pin this shape
/// diverges 2 vs 1. Observed through `Shared` and printed once after the join, because the print ORDER of
/// this shape already differs per engine (a bare-`print` variant is not byte-identical).
#[test]
fn task_snapshot_pins_at_its_own_spawn_parity() {
    let src = "\
import std.concurrency
g: int = 1
fn main():
    a := Shared(0)
    b := Shared(0)
    parallel:
        spawn: a.set(g)
        parallel:
            spawn: b.set(g)
        g = 2
    print(\"A={a.get()} B={b.get()} parent={g}\")
main()
";
    assert_eq!(parity_entry(src), "A=1 B=1 parent=2\n");
}

/// W6-2 — the pin is per-TASK, at its own `spawn`: two spawns into the SAME nursery straddling a global
/// mutation read the value live when EACH ran, identically on both engines. That is the Go rule (a
/// goroutine sees the globals current when `go` ran) and it is what makes the IMPLICIT nursery — which
/// spans a whole module/function body — behave, since a per-NURSERY pin would freeze the whole body at
/// its first bare `spawn` (see `bare_spawn_implicit_nursery_pins_per_spawn_parity`).
#[test]
fn two_spawns_one_nursery_pin_at_their_own_spawn_parity() {
    let src = "\
import std.concurrency
g: int = 1
fn main():
    a := Shared(0)
    b := Shared(0)
    parallel:
        spawn: a.set(g)
        g = 2
        spawn: b.set(g)
    print(\"A={a.get()} B={b.get()} parent={g}\")
main()
";
    assert_eq!(parity_entry(src), "A=1 B=2 parent=2\n");
}

/// W6-2 (regression) — a bare `spawn` binds to the IMPLICIT nursery, which the compiler opens at the top
/// of the module / function body (`Span{1,1}`) and joins at its end. So the snapshot instant must be the
/// SPAWN, never the nursery: pinning per-nursery would freeze the whole body at its first bare `spawn`,
/// replaying every global initialized later as `nil` — the very W6-2 class. Here the second task reads
/// `n`, declared AFTER the first bare `spawn`, and a `List` global's method (the `nil` shape that faults
/// rather than printing wrong).
#[test]
fn bare_spawn_implicit_nursery_pins_per_spawn_parity() {
    let src = "\
import std.concurrency
tot := AtomicInt(0)
spawn: tot.add(1)
n: int = 42
q: List[int] = [1, 2, 3]
spawn: print(\"n = {n} len = {q.len()} sum = {n + 1}\")
";
    assert_eq!(parity_entry(src), "n = 42 len = 3 sum = 43\n");
}

/// W6-2 — the same, one level down: a bare `spawn` inside a FUNCTION body sees the globals live at the
/// spawn, and a later mutation in that body is NOT time-travelled back into it (pre-W6-2 the snapshot was
/// built at the body-end join, so the task read the POST-mutation value).
#[test]
fn bare_spawn_in_a_function_body_pins_per_spawn_parity() {
    let src = "\
import std.concurrency
g: int = 1
fn main():
    seen := Shared(0)
    spawn: seen.set(g)
    g = 2
    return seen
print(main().get())
";
    assert_eq!(parity_entry(src), "1\n");
}

/// QoL: an untyped int-CONSTANT branch of an if/match EXPRESSION widens to `float` when a
/// float-constant sibling branch is present (the `literal_numeric_mix` peephole, shared with list/map
/// literals). This test proves the compiler actually emits `Op::CoerceFloat` on the int branch — the
/// int-taken branch must render as a FLOAT ("1.0"), never leave an `Int` under a static `float` — and
/// that both engines agree.
#[test]
fn if_match_expr_int_float_widen_parity() {
    let src = concat!(
        "fn main():\n",
        "    x := if true: 1 else: 2.5\n", // int branch taken -> must be 1.0
        "    print(x)\n",
        "    print(x + 0.5)\n",
        "    y := if false: 1 else: 2.5\n", // float branch taken -> 2.5
        "    print(y)\n",
        "    z := match true:\n        true: 1\n        _: 2.5\n", // int arm -> 1.0
        "    print(z)\n",
        "    print(str(if true: 1 else: 2.5))\n", // str of the widened value -> "1.0"
        "    e := if false: 1 elif true: 2 else: 3.5\n", // elif chain, int arm taken -> 2.0
        "    print(e)\n",
        "    h := if true: 2.5 elif false: 1 else: 3\n", // float in HEAD, float arm taken -> 2.5
        "    print(h)\n",
        "    g := if false: 2.5 elif true: 1 else: 3\n", // float in HEAD, int arm taken -> 1.0
        "    print(g)\n",
        "main()\n",
    );
    assert_parity_out(src, "1.0\n1.5\n2.5\n1.0\n1.0\n2.0\n2.5\n1.0\n");
}

// ----- 8-byte `Value` (int-favoring pointer-tag): boxing must be invisible to programs -----

#[test]
fn parity_big_int_crosses_inline_box_boundary() {
    // 2^62 and above box as `Obj::BigInt`; 2^62-1 and below stay inline. Crossing the boundary must
    // be invisible: arithmetic, equality, and Display all agree, and both engines match byte-for-byte.
    let src = "\
fn main():
    x := 4611686018427387904
    print(x)
    print(x - 1)
    print(x == x)
    print(x + 1)
    y := 1 << 62
    print(y)
    print(x == y)
    print(x + 100)
main()";
    assert_parity_out(
        src,
        "4611686018427387904\n4611686018427387903\ntrue\n4611686018427387905\n4611686018427387904\ntrue\n4611686018427388004\n",
    );
}

#[test]
fn parity_i64_overflow_still_faults_past_boxing() {
    // Boxing lifts the INLINE ceiling to ±2^62, NOT the i64 ceiling: an operation whose true result
    // exceeds i64 still faults (overflow semantics unchanged), it does not silently box a wider value.
    let src = "\
fn run() -> int!:
    x := 4611686018427387904
    r := recover:
        _ := x * 2
    match r:
        Ok(v): return Ok(v)
        Err(e): print(e.message())
    return Ok(0)
fn main():
    _ := run()
main()";
    assert_eq!(parity_entry(src), "integer overflow in Mul\n");
}

#[test]
fn parity_boxed_float_canonical_eq() {
    // Two independently-boxed equal floats compare `==` and hash equal (a Set dedups them to one);
    // cross-type `1 == 1.0` still holds. Boxing is per-alloc, so equality MUST compare the f64.
    let src = "\
fn main():
    a := 1.5
    b := 3.0 / 2.0
    print(a == b)
    print(a)
    print(1 == 1.0)
    s := {1.5, 3.0 / 2.0}
    print(s.len())
main()";
    assert_parity_out(src, "true\n1.5\ntrue\n1\n");
}

#[test]
fn parity_airlock_bigint_and_float_roundtrip() {
    // A boxed big-int and a boxed float cross the airlock (spawn + Channel) and round-trip on BOTH
    // engines: `from_wire` re-boxes via `make_int`/`box_float` on the destination heap identically.
    let src = "\
fn main():
    ci := Channel[int]()
    cf := Channel[float]()
    parallel:
        spawn:
            ci.send(4611686018427387905)
            cf.send(1.5)
    x := ci.recv()
    f := cf.recv()
    print(x)
    print(x == 4611686018427387905)
    print(f)
    print(f == 1.5)
main()";
    assert_parity_out(src, "4611686018427387905\ntrue\n1.5\ntrue\n");
}

#[test]
fn parity_bin_local_const_wide_literal_on_boxed_local() {
    // `BinLocalConst{slot,val}` fuses `[GetLocal, ConstInt(val), binop]` with an UNBOUNDED `val`.
    // When the local is a boxed BigInt the fast inline path misses and the else-branch must
    // reconstruct the folded constant via `make_int` (boxes wide vals), NOT the inline-only
    // `Value::int` (debug-panics / release-corrupts for |val| > 2^62-1).
    let src = "\
fn f() -> int:
    x := 5000000000000000000
    return x - 4999999999999999999
fn main():
    print(f())
main()";
    assert_parity_out(src, "1\n");
}

#[test]
fn parity_inc_local_wide_delta_on_boxed_local() {
    // `IncLocal{slot,delta}` (the `+=` superinstruction) carries an UNBOUNDED fused `delta`. On a
    // boxed BigInt local the else-branch must push the delta via `make_int`, not the inline-only
    // `Value::int` (which debug-panics / release-corrupts for |delta| > 2^62-1).
    let src = "\
fn f() -> int:
    x := -5000000000000000000
    x += 4999999999999999999
    return x
fn main():
    print(f())
main()";
    assert_parity_out(src, "-1\n");
}

#[test]
fn parity_struct_slice_wide_bound() {
    // The `slice`-protocol path unwraps each bound via `int_val` (accepts a boxed BigInt) then
    // re-wraps it as an `Option[int]` for the user body. A wide bound must round-trip via `make_int`,
    // not the inline-only `Value::int` (debug-panic / release-corrupt at |n| > 2^62-1).
    let src = "\
struct Cut:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> int:
        match start:
            Some(s): return s
            None: return 0
fn main():
    c := Cut([1])
    print(c[9000000000000000000:])
main()";
    assert_parity_out(src, "9000000000000000000\n");
}

// ===== F3 — checker over-rejection fixes, end-to-end RUN parity =====

/// F3 — generic fns over the native reserved handles (`Shared`/`Atomic`) bind `T` from the argument
/// and run type-erased identically on both engines.
#[test]
fn generic_fn_over_native_handles_run_parity() {
    let src = "import std.concurrency\n\
               fn peek[T](s: Shared[T]) -> T:\n\
               \x20   return s.get()\n\
               fn look[T](a: Atomic[T]) -> T:\n\
               \x20   return a.load()\n\
               print(peek(Shared(9)))\n\
               print(look(Atomic(3)))\n";
    assert_parity_out(src, "9\n3\n");
}

/// F3 (subst) — a generic wrapper struct holding a `Channel[T]` constructs, and its channel is
/// sent-to and recv'd-from after `Channel[T]→Channel[int]` substitution.
#[test]
fn generic_wrapper_struct_channel_run_parity() {
    let src = "import std.concurrency\n\
               struct Box[T]:\n\
               \x20   ch: Channel[T]\n\
               b := Box(Channel[int]())\n\
               b.ch.send(7)\n\
               print(b.ch.recv())\n";
    assert_parity_out(src, "7\n");
}

/// FIX 1a — member-level turbofish on a RESERVED built-in receiver whose harvested method declares
/// its own `[U]` (`List.map`) is accepted, not rejected "takes no type argument(s)". Runs on both
/// engines and prints the mapped list.
#[test]
fn reserved_receiver_method_turbofish_run_parity() {
    let src = "print([1, 2, 3].map[int](fn(x): x * 2))\n";
    assert_parity_out(src, "[2, 4, 6]\n");
}
