//! Golden tests: order-insensitive output, Executor exit drain, the full suite program, std HOF
//! goldens, std.io/os/csv/math goldens.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

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
        golden_entry("import std.io\nfn main():\n    io.print(\"hello\")\nmain()"),
        "hello\n"
    );
}

#[test]
fn parity_std_io_read_write_file() {
    let t = TmpDir::new();
    let data = t.0.join("data.txt").display().to_string();
    let src = format!(
        "import std.io\nfn main():\n    match io.write_file(\"{data}\", \"hello\\nworld\"):\n        ?_: io.print(\"wrote\")\n        !e: io.print(e)\n    match io.read_file(\"{data}\"):\n        ?s: io.print(s)\n        !e: io.print(e)\nmain()"
    );
    let entry = t.write("main.chz", &src);
    let (io_out, _ie, ir, _) = run_file(&entry);
    let (vo, _ve, vr, _) = run_file(&entry);
    assert!(ir.is_ok() && vr.is_ok(), "interp={ir:?} vm={vr:?}");
    assert_eq!(io_out, vo);
    assert_eq!(io_out, "wrote\nhello\nworld\n");
}

#[test]
fn parity_std_io_read_missing_file_errs() {
    // The error text comes from the same `std::fs` call, so it matches; we only
    // assert the Err branch is taken (deterministic regardless of OS message).
    let src = "import std.io\nfn main():\n    match io.read_file(\"/no/such/chezzi/path/xyz\"):\n        ?s: io.print(s)\n        !e: io.print(\"err\")\nmain()";
    assert_eq!(golden_entry(src), "err\n");
}

#[test]
#[cfg(target_os = "linux")]
fn read_file_caps_oversized_input() {
    // /dev/zero is unbounded; read_file must return an Err (the size cap), not OOM.
    let src = "import std.io\nfn main():\n    match io.read_file(\"/dev/zero\"):\n        ?s: io.print(\"ok\")\n        !e: io.print(\"capped\")\nmain()";
    assert_eq!(golden_entry(src), "capped\n");
}

#[test]
fn parity_std_io_read_line_consumes_injected_stdin() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    match io.read_line():\n        ?l: io.print(\"got {l}\")\n        None: io.print(\"eof\")\n    match io.read_line():\n        ?l: io.print(l)\n        None: io.print(\"eof\")\nmain()";
    let out = golden_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["alpha".to_string()]),
        ..Default::default()
    });
    assert_eq!(out, "got alpha\neof\n");
}

/// `io.read_all()` drains the WHOLE remaining stdin to EOF as one `str` (Python `sys.stdin.read()`).
/// Over the injected `Lines` source it reconstructs each line + `\n` (the injected queue is newline-
/// stripped), so two lines come back as "line0\nline1\n". Single entry task ⇒ deterministic exact
/// assert. Also pins `read_all` is NOT `Kind::Blocking` (an offloaded call would hit `OffloadHost`'s
/// stdio `unreachable!`).
#[test]
fn parity_std_io_read_all_drains_injected_stdin() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    io.print(io.read_all())\nmain()";
    let out = golden_entry_cfg(src, || HostConfig {
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
    let src = "import std.io\nfn main():\n    while true:\n        match io.read_char():\n            ?c: io.print(\"[{c}]\")\n            None:\n                io.print(\"done\")\n                break\nmain()";
    let out = golden_entry_cfg(src, || HostConfig {
        stdin: Stdin::lines(["aé".to_string()]),
        ..Default::default()
    });
    // 'a', 'é', then the reconstructed '\n' (prints `[`, newline, `]`), then None → done.
    assert_eq!(out, "[a]\n[é]\n[\n]\ndone\n");
}

/// `io.input(prompt)` = print the prompt (no newline) + flush + `read_line`. Under the BUFFERED sink
/// (every test helper + embedder) `flush` is a no-op and the prompt simply lands in the captured
/// `out`. Also pins that neither fn is `Kind::Blocking` (an offloaded call
/// would hit `OffloadHost`'s stdio `unreachable!`).
#[test]
fn parity_std_io_input_prompt_then_line_and_flush_is_a_noop() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    io.flush()\n    match io.input(\"p: \"):\n        ?l: io.print(\"got {l}\")\n        None: io.print(\"eof\")\nmain()";
    let out = golden_entry_cfg(src, || HostConfig {
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
/// which is the point: WHICH task gets a given line is nondeterministic BY DESIGN.
/// Do NOT "fix" this back to an exact-stdout `assert_eq!`; that is a designed flake. A false EOF
/// would add an `eof` line; a duplicated line would repeat a `got` line. Both change the multiset.
#[test]
fn parity_spawned_tasks_share_stdin_exactly_once() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn t():\n    match io.read_line():\n        ?v: io.print(\"got {v}\")\n        None: io.print(\"eof\")\nfn main():\n    parallel:\n        spawn: t()\n        spawn: t()\n    t()\nmain()";
    let out = golden_entry_cfg_lines(src, || HostConfig {
        stdin: Stdin::lines(["a".to_string(), "b".to_string(), "c".to_string()]),
        ..Default::default()
    });
    assert_lines_multiset(&out, &["got a", "got b", "got c"]);
}

/// Same shared-stdin contract at the OTHER task-entry family: `Executor.submit` (the M:N pool
/// drain). An invariant enforced at one seam is not enforced.
#[test]
fn parity_executor_tasks_share_stdin_exactly_once() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nimport Executor from std.concurrency\nfn t():\n    match io.read_line():\n        ?v: io.print(\"got {v}\")\n        None: io.print(\"eof\")\nfn main():\n    e := Executor()\n    e.submit(t)\n    e.submit(t)\n    e.shutdown()\n    t()\nmain()";
    let out = golden_entry_cfg_lines(src, || HostConfig {
        stdin: Stdin::lines(["a".to_string(), "b".to_string(), "c".to_string()]),
        ..Default::default()
    });
    assert_lines_multiset(&out, &["got a", "got b", "got c"]);
}

#[test]
fn parity_std_io_eprint_goes_to_stderr_not_stdout() {
    let src = "import std.io\nfn main():\n    io.eprint(\"to stderr\")\n    io.print(\"to stdout\")\nmain()";
    // stdout has only the print line, stderr has only the eprint line.
    assert_eq!(golden_entry(src), "to stdout\n");
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
    // formatted "LEVEL message" in order.
    let src = "import std.log\nfn main():\n    lg := log.new()\n    lg.info(\"served\")\n    lg.debug(\"noisy\")\n    lg.warn(\"careful\")\nmain()";
    assert_eq!(golden_entry(src), ""); // nothing on stdout
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
    let src = "import std.io\nimport std.os\nfn main():\n    for a in os.args():\n        io.print(a)\n    match os.env(\"CHEZZI_TEST_VAR\"):\n        ?v: io.print(v)\n        None: io.print(\"no var\")\nmain()";
    let out = golden_entry_cfg(src, || HostConfig {
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
    let src = "import std.io\nimport std.os\nfn main():\n    match os.env(\"DEFINITELY_UNSET_XYZ\"):\n        ?v: io.print(v)\n        None: io.print(\"none\")\nmain()";
    let out = golden_entry_cfg(src, HostConfig::default);
    assert_eq!(out, "none\n");
}

#[test]
fn parity_std_os_getcwd_ok() {
    let src = "import std.io\nimport std.os\nfn main():\n    match os.getcwd():\n        ?p: io.print(\"ok\")\n        !e: io.print(\"err\")\nmain()";
    assert_eq!(golden_entry(src), "ok\n");
}

/// TICKET-198 — a missing defaulted field runs its default re-entrantly while `coerce_json` holds
/// decoded siblings, list elements and map values. Under GC stress (collect before every
/// instruction) they must stay rooted: before the operand-stack rooting this frees `xs[0]`.
#[test]
fn json_decode_field_defaults_survive_gc_stress() {
    let src = r#"import std.json
fn mk() -> List[str]:
    junk := []
    for i in 0..20:
        junk.push("{i}")
    return ["{junk.len()}"]
struct In:
    w: str
    tags: List[str] = mk()
struct Out:
    xs: List[In]
    m: Map[str, In]
    name: str = "d" + "1"
match json.decode[Out](r'{"xs":[{"w":"a"},{"w":"b"},{"w":"c"}],"m":{"k":{"w":"q"}}}'):
    ?o: print(o)
    !e: print(e)
"#;
    assert_eq!(
        vm_run_file_stress(src, crate::native::HostConfig::default()),
        "Out(xs=[In(w='a', tags=['20']), In(w='b', tags=['20']), In(w='c', tags=['20'])], m={'k': In(w='q', tags=['20'])}, name='d1')\n"
    );
}

/// TICKET-198 — the map and tuple arms: a map value and a tuple element are decoded, then a later
/// sibling runs a default. Under GC stress each built child must stay rooted until its container
/// owns it.
#[test]
fn json_decode_map_and_tuple_children_survive_gc_stress() {
    let src = r#"import std.json
fn mk() -> List[str]:
    junk := []
    for i in 0..20:
        junk.push("{i}")
    return ["{junk.len()}"]
struct In:
    w: str
    tags: List[str] = mk()
match json.decode[Map[str, In]](r'{"a":{"w":"x"},"b":{"w":"y"}}'):
    ?m: print(m)
    !e: print(e)
match json.decode[(In, In)](r'[{"w":"p"},{"w":"q"}]'):
    ?t: print(t)
    !e: print(e)
"#;
    assert_eq!(
        vm_run_file_stress(src, crate::native::HostConfig::default()),
        "{'a': In(w='x', tags=['20']), 'b': In(w='y', tags=['20'])}\n(In(w='p', tags=['20']), In(w='q', tags=['20']))\n"
    );
}

/// Task 1 — the `Executor` PROGRAM-EXIT drain (`drain_live_executors`) runs each queued job
/// against EMPTY frames (it runs AFTER `run()` popped the top-level
/// frame). This exercises that path under `gc_stress` (collect before every instruction): both
/// un-`shutdown()`'d jobs mutate a module-global aggregate (forcing the snapshot path) and allocate,
/// and the drain must complete with no error and full isolation (the parent's `xs` untouched).
///
/// No GC pin is needed: the ONLY thing that swaps `module_objs` out of the VM is [`Vm::swap_ctx`],
/// which moves the fiber's heap in the same call, so a swapped-out view always parks together with
/// the heap its `GcRef`s index and `collect()` (which walks only the LIVE heap) can never sweep it.
#[test]
fn executor_exit_drain_module_globals_survive_gc_stress() {
    let src = "\
import std.concurrency
xs := [1, 2, 3]
fn worker():
    recover: xs.push(99)
    junk := []
    for i in 0..30:
        junk.push([\"{i}\", \"{i + 1}\"])
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
    vm.drain_live_executors().unwrap();
    assert_eq!(vm.out, b"");
}

/// Bug 3 — a module-qualified generic fn (`geo.empty_list[int]()`) type-checks AND runs
/// (type args are erased, so this exercises the checker fix end-to-end).
#[test]
fn parity_qualified_generic_fn_turbofish() {
    let geo = ("geo.chz", "fn empty_list[T]() -> List[T]:\n    return []\n");
    let main = (
        "main.chz",
        "import geo\nfn main():\n    xs := geo.empty_list[int]()\n    xs.push(1)\n    print(xs)\n    ys: List[str] = geo.empty_list()\n    print(ys)\nmain()",
    );
    assert_eq!(golden_file_entry(&[geo, main], "main.chz"), "[1]\n[]\n");
}

/// A from-imported global RE-DECLARED at module scope (`:=`) is the module's OWN binding — assigning
/// it is legal and runs (the rebind gate must not fire on a name the import no longer
/// owns).
#[test]
fn parity_from_import_then_module_scope_redeclare() {
    let st = ("st.chz", "COUNT := 0\n");
    let main = (
        "main.chz",
        "import COUNT from st\nCOUNT := COUNT + 1\nCOUNT = COUNT + 1\nfn main():\n    print(COUNT)\nmain()",
    );
    assert_eq!(golden_file_entry(&[st, main], "main.chz"), "2\n");
}

/// Bug 4 acceptance: the std string module is `std.string`, so importing it UN-aliased does not
/// bind the reserved name `str` — the global `str()` ctor and the qualified module fn both work in
/// the SAME module.
#[test]
fn parity_import_std_string_and_str_ctor() {
    let src = "import std.string
fn main():
    print(\"5\")
    print(string.pad_left(\"a\", 3, \"-\"))\nmain()";
    assert_eq!(golden_entry(src), "5\n--a\n");
}

#[test]
fn parity_std_str_pure_chezzi_with_mixed_native_import() {
    // std.string is a real Chezzi file (crate/std/string.chz); std.io is native — both in one program.
    let src = "import std.io\nimport std.string as text\nfn main():\n    io.print(text.repeat(\"ab\", 3))\n    io.print(text.reverse(\"hello\"))\n    io.print(text.pad_left(\"7\", 3, \"0\"))\n    if text.is_empty(\"\"):\n        io.print(\"empty\")\n    for line in text.split_lines(\"a\\nb\\nc\"):\n        io.print(line)\nmain()";
    assert_eq!(golden_entry(src), "ababab\nolleh\n007\nempty\na\nb\nc\n");
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
            golden_entry(&src),
            format!("{want}\n"),
            "free fn mismatch for `{args}`"
        );
    }
}

/// An empty `fill` LIVELOCKED the free fn's prepend loop (zero output, no diagnostic). It must now
/// fault — with the same message as the native method —, EAGERLY: a `width` the
/// receiver already satisfies does not excuse it.
#[test]
fn parity_std_str_pad_left_empty_fill_faults() {
    for args in [r#""a", 5, """#, r#""abc", 1, """#] {
        let src = format!(
            "import std.string as text\nfn main():\n    print(text.pad_left({args}))\nmain()"
        );
        let msg = golden_entry_fault(&src);
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

/// A spread of programs exercising every feature class, each with its expected outcome.
const PROGRAMS: &[(&str, Result<&str, &str>)] = &[
    // arithmetic + promotion + truncation
    (
        "print(7 / 2)\nprint(1 + 2.0)\nprint(2.5 * 2.0)\nprint(10 % 3)",
        Ok("3\n3.0\n5.0\n1\n"),
    ),
    // string concat + interpolation + escapes
    (
        "fn main():\n    n := \"x\"\n    print(\"a{n}b {1 + 2} {{lit}}\")\nmain()",
        Ok("axb 3 {lit}\n"),
    ),
    // comparison + equality + bool logic
    (
        "print(1 < 2)\nprint(2 == 2.0)\nprint(true and false)\nprint(false or true)\nprint(not true)",
        Ok("true\ntrue\nfalse\ntrue\nfalse\n"),
    ),
    // lists, indexing, len
    (
        "print([1, 2, 3])\nprint([10, 20, 30][2])\nprint([1, 2].len())",
        Ok("[1, 2, 3]\n30\n2\n"),
    ),
    // structs + methods
    (
        "struct P:\n    x: int\n    y: int\n    fn sum(self) -> int:\n        return self.x + self.y\nfn main():\n    p := P(3, 4)\n    print(p)\n    print(p.sum())\nmain()",
        Ok("P(x=3, y=4)\n7\n"),
    ),
    // enums + match + payload binding
    (
        "enum S:\n    C(int)\n    Sq(int)\nfn a(s: S) -> int:\n    match s:\n        S.C(r): return r * r\n        S.Sq(n): return n * n\nfn main():\n    print(a(S.C(3)))\n    print(a(S.Sq(4)))\nmain()",
        Ok("9\n16\n"),
    ),
    // generic enum (type-erased): same enum at two element types + match payload substitution
    (
        "enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\nfn sum(t: Tree[int]) -> int:\n    match t:\n        Tree.Leaf: return 0\n        Tree.Node(v, l, r): return sum(l) + v + sum(r)\nfn main():\n    t: Tree[int] = Tree.Node(2, Tree.Node(1, Tree.Leaf, Tree.Leaf), Tree.Node(3, Tree.Leaf, Tree.Leaf))\n    print(sum(t))\nmain()",
        Ok("6\n"),
    ),
    // closures
    (
        "fn adder(n: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + n\nfn main():\n    f := adder(10)\n    print(f(5))\nmain()",
        Ok("15\n"),
    ),
    // ? operator (Ok + Err propagation)
    (
        "fn d(a: int, b: int) -> int!:\n    if b == 0:\n        return !\"zero\"\n    return ?(a / b)\nfn use() -> int!:\n    r := d(10, 0)?\n    return ?r\nfn main():\n    match use():\n        ?v: print(v)\n        !e: print(e)\nmain()",
        Ok("zero\n"),
    ),
    // for + while loops
    (
        "fn main():\n    t := 0\n    for i in 0..100:\n        t += i\n    print(t)\n    n := 5\n    while n > 0:\n        n -= 1\n    print(n)\nmain()",
        Ok("4950\n0\n"),
    ),
    // builtins
    (
        "print(List.from_iter(0..4))\nprint((\"7\".to_int() ?? 0) + 1)\nprint((3).to_float())\nprint([1, 2, 3].len())\nprint(\"42\")",
        Ok("[0, 1, 2, 3]\n8\n3.0\n3\n42\n"),
    ),
    // recursion
    (
        "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nfn main():\n    print(fib(15))\nmain()",
        Ok("610\n"),
    ),
    // inferred return type (no `-> T`): runtime is unaffected
    (
        "fn add(a: int, b: int) -> int:\n    return a + b\nfn classify(n: int) -> int?:\n    if n == 0:\n        return ?0\n    return None\nfn main():\n    print(add(2, 3))\n    match classify(0):\n        ?v: print(v)\n        None: print(\"none\")\nmain()",
        Ok("5\n0\n"),
    ),
    // expression-valued match (multiline) + if (inline)
    (
        "fn lookup(k: int) -> int?:\n    if k == 0:\n        return None\n    return ?k\nfn main():\n    found := match lookup(7):\n        ?v: v\n        None: -1\n    print(found)\n    sign := if found > 0: \"pos\" else: \"neg\"\n    print(sign)\n    none := match lookup(0):\n        ?v: v\n        None: -1\n    print(none)\nmain()",
        Ok("7\npos\n-1\n"),
    ),
    // ----- M6: core-type methods (str) -----
    (
        "print(\"abcd\".len())\nprint(\"Hi There\".upper())\nprint(\"Hi There\".lower())\nprint(\"  pad  \".trim())",
        Ok("4\nHI THERE\nhi there\npad\n"),
    ),
    // str conforms to Error: message() returns the string itself
    ("print(\"boom\".message())", Ok("boom\n")),
    // Go-style Result[T, E]: custom struct error (T!E), match, message() dispatch
    (
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db {self.code}\"\nfn q(ok: bool) -> int!DbErr:\n    if ok:\n        return ?1\n    return !DbErr(503)\nfn main():\n    match q(false):\n        ?v: print(v)\n        !e: print(e.message())\n    match q(true):\n        ?v: print(v)\n        !e: print(e.message())\nmain()",
        Ok("db 503\n1\n"),
    ),
    // default-Error path: Err(str) flows as Result[int, Error], consumed via message()
    (
        "fn parse(ok: bool) -> int!:\n    if ok:\n        return ?42\n    return !\"bad input\"\nfn main():\n    match parse(false):\n        ?v: print(v)\n        !e: print(e.message())\nmain()",
        Ok("bad input\n"),
    ),
    // ----- M11 Phase B: recover boundary -----
    // recover catches index-OOB; Ok path wraps the trailing value
    (
        "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"recovered: {e.message()}\")\nmain()",
        Ok("recovered: index 9 out of bounds (len 2)\n"),
    ),
    // recover catches divide-by-zero
    (
        "fn main():\n    r := recover:\n        10 / 0\n    match r:\n        ?v: print(v)\n        !e: print(\"err: {e.message()}\")\nmain()",
        Ok("err: division by zero\n"),
    ),
    // recover catches integer overflow
    (
        "fn main():\n    r := recover:\n        9223372036854775807 * 2\n    match r:\n        ?v: print(v)\n        !e: print(\"ovf\")\nmain()",
        Ok("ovf\n"),
    ),
    // recover ok-path wraps the value
    (
        "fn main():\n    r := recover: 2 + 3\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"err\")\nmain()",
        Ok("ok 5\n"),
    ),
    // a fault three calls deep is caught at the boundary (no per-call wrapping)
    (
        "fn a() -> int:\n    return b()\nfn b() -> int:\n    return c()\nfn c() -> int:\n    return [1][9]\nfn main():\n    r := recover:\n        a()\n    match r:\n        ?v: print(v)\n        !e: print(\"deep recovered\")\nmain()",
        Ok("deep recovered\n"),
    ),
    // `?` inside recover short-circuits to `r` (try-block): the Err lands in `r`, and code
    // AFTER the recover still runs — the enclosing fn returns a plain str, so this only works
    // if `?` did NOT exit the function.
    (
        "fn d(b: int) -> int!:\n    if b == 0:\n        return !\"zero\"\n    return ?(10 / b)\nfn use() -> str:\n    r := recover:\n        x := d(0)?\n        x + 1\n    match r:\n        ?v: return \"ok\"\n        !e: return \"caught {e.message()}\"\nfn main():\n    print(use())\nmain()",
        Ok("caught zero\n"),
    ),
    // `?` Ok path inside recover: value unwrapped, trailing expression becomes the Ok result
    (
        "fn d(b: int) -> int!:\n    return ?(10 / b)\nfn main():\n    y := 0\n    r := recover:\n        x := d(2)?\n        y = x + 1\n    match r:\n        ?_: print(\"ok {y}\")\n        !e: print(e.message())\nmain()",
        Ok("ok 6\n"),
    ),
    // side effects before a caught fault PERSIST (keep semantics)
    (
        "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        ?v: print(\"ok\")\n        !e: print(\"recovered\")\n    print(\"x={x}\")\nmain()",
        Ok("recovered\nx=99\n"),
    ),
    // nested recover: the inner boundary catches, the outer sees a normal value
    (
        "fn main():\n    z := 0\n    r := recover:\n        inner := recover:\n            [1][9]\n        match inner:\n            ?v: z = v\n            !e: z = 0\n    match r:\n        ?_: print(\"outer ok {z}\")\n        !e: print(\"outer err\")\nmain()",
        Ok("outer ok 0\n"),
    ),
    // recovered value composes with `?` after the boundary\n
    (
        "fn run() -> int!:\n    r := recover: [10, 20][0]\n    v := r?\n    return ?(v + 1)\nfn main():\n    match run():\n        ?v: print(v)\n        !e: print(e.message())\nmain()",
        Ok("11\n"),
    ),
    (
        "print(\"a,b,c\".split(\",\"))\nprint(\",\".join([\"a\", \"b\", \"c\"]))",
        Ok("['a', 'b', 'c']\na,b,c\n"),
    ),
    (
        "print(\"abc\".starts_with(\"ab\"))\nprint(\"abc\".starts_with(\"z\"))\nprint(\"abc\".contains(\"b\"))\nprint(\"abc\".contains(\"q\"))",
        Ok("true\nfalse\ntrue\nfalse\n"),
    ),
    // chained core-type methods
    (
        "print(\"  Hello,World  \".trim().lower().split(\",\"))",
        Ok("['hello', 'world']\n"),
    ),
    // ----- M6: core-type methods (list) -----
    (
        "fn main():\n    xs := [1, 2]\n    xs.push(3)\n    xs.push(4)\n    print(xs)\n    print(xs.len())\nmain()",
        Ok("[1, 2, 3, 4]\n4\n"),
    ),
    // ----- error messages -----
    (
        "print(1 / 0)",
        Err("runtime error (line 1, col 7): division by zero"),
    ),
    (
        "print([1, 2][9])",
        Err("runtime error (line 1, col 7): index 9 out of bounds (len 2)"),
    ),
    (
        "print(1 + \"x\")",
        Err("runtime error (line 1, col 7): cannot apply Add to int and str"),
    ),
    (
        "fn loop(n: int) -> int:\n    return loop(n + 1)\nfn main():\n    print(loop(0))\nmain()",
        Err(
            "runtime error (line 2, col 12): maximum call depth (10000) exceeded (infinite recursion?)",
        ),
    ),
    // M6 method error messages
    (
        "print(\"hi\".upper(\"extra\"))",
        Err("runtime error (line 1, col 7): upper() expects 0 argument(s), got 1"),
    ),
    (
        "print(\"hi\".frobnicate())",
        Err("runtime error (line 1, col 7): type str has no method 'frobnicate'"),
    ),
    (
        "print(\",\".join([1, 2]))",
        Err(
            "runtime error (line 1, col 7): join() expects a list of str, got an element of type int",
        ),
    ),
    (
        "print((5).upper())",
        Err("runtime error (line 1, col 8): type int has no method 'upper'"),
    ),
    // arg-eval order: a bad method/receiver with an erroring arg must report the error from the
    // arg, not the receiver — the VM evaluates args (operands) before the call.
    (
        "print((5).frob(1 / 0))",
        Err("runtime error (line 1, col 16): division by zero"),
    ),
    (
        "print(\"hi\".frob(1 / 0))",
        Err("runtime error (line 1, col 17): division by zero"),
    ),
    // ----- entry model: no auto-main; a top-level `?` exits, a bare discarded Err/None does not -----
    ("fn main():\n    print(\"hi\")", Ok("")), // main defined but never called → no output
    ("fn bad() -> int!:\n    return !\"boom\"\nbad()", Ok("")), // bare top-level error → discarded, W8-2 warns, no exit
    (
        "x := (!\"oops\")?",
        Err("runtime error (line 1, col 7): unhandled error: oops"),
    ), // top-level `?` Err → unhandled error
    ("fn g() -> int?:\n    return None\ng()", Ok("")),          // bare None → discarded, no exit
    (
        "fn f() -> int!:\n    return !\"x\"\nr := f()\nprint(\"handled\")",
        Ok("handled\n"),
    ), // Err bound = handled → no exit
    (
        "fn main():\n    print(\"before\")\n    x := (!\"boom\")?\n    print(\"after\")\nmain()",
        Ok("before\n"),
    ), // `?` inside main, main() itself is a bare discarded drop → no exit
    // a user enum shadowing `Err` is a normal value: bare one must NOT exit, `?` must reject it
    (
        "enum Signal:\n    Err(int)\n    Quiet\nSignal.Err(5)\nprint(\"made it\")",
        Ok("made it\n"),
    ),
    (
        "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := (!5)?\n    return x\nf()",
        Ok(""),
    ), // f() -> int is not a carrier type: no runtime check, no W8-2 warning either
    // unhandled top-level error INSIDE a top-level block (interp: call_depth 0, VM: is_toplevel)
    (
        "fn bad() -> int!:\n    return !\"boom\"\nif true:\n    bad()\nprint(\"after\")",
        Ok("after\n"),
    ), // bare Err in `if` → discarded, no exit
    (
        "fn bad() -> int!:\n    return !\"boom\"\nfor i in 0..1:\n    bad()\nprint(\"after\")",
        Ok("after\n"),
    ), // bare Err in `for` → discarded, no exit
    (
        "fn d() -> int!:\n    return !\"z\"\nif true:\n    x := d()?\n    print(x)",
        Err("runtime error (line 4, col 10): unhandled error: z"),
    ), // top-level `?` in block → exit
];

#[test]
fn parity_full_suite_vm_vs_interp() {
    for (src, expect) in PROGRAMS {
        match expect {
            Ok(want) => assert_eq!(
                vm_outcome(src).expect("program should run"),
                *want,
                "for:\n{src}"
            ),
            Err(want) => assert_eq!(&vm_outcome(src).unwrap_err(), want, "for:\n{src}"),
        }
    }
}

// A manifest `module:function` entrypoint whose fn returns `Err(..)` must surface it as an unhandled
// runtime error (rc=1), symmetric with the unhandled-top-level-Err rule — not silently discard the
// return value (routes through `run_file_inner`→`invoke_entrypoint`).
// (2026-07-18 bug-hunt: `?` in a nil entry fn used to swallow the error; the fix lets the entry be
// `-> T!` and use `?`, and this surfaces a returned Err.)
#[test]
fn manifest_entrypoint_err_surfaced_both_engines() {
    let dir = TmpDir::new();
    let entry = dir.write("main.chz", "fn main() -> int!:\n    return !\"boom\"\n");
    let (_out, _err, outcome, _rc) = crate::vm::run_file_entry(&entry, "main");
    let e = outcome.expect_err("entry fn returning Err must surface a runtime error");
    assert!(
        e.message.contains("unhandled error: boom"),
        "expected 'unhandled error: boom', got {:?}",
        e.message
    );
    // W13-21 — the entry call's coordinate must name the entry fn's declaration, not render a bare
    // `line 1, col 1` with no file.
    let rendered = crate::vm::format_trace(&e);
    assert!(
        rendered.contains("main.chz:1:"),
        "the entry fault must name the entry file, got {rendered}"
    );
    assert!(
        e.span.file != 0,
        "the entry span must carry the entry module's file id"
    );
}

// GUARD: an entry fn returning `Ok(..)` runs clean (rc=0) — the surfacing gate is Err/None only.
#[test]
fn manifest_entrypoint_ok_runs_clean_both_engines() {
    let dir = TmpDir::new();
    let entry = dir.write(
        "main.chz",
        "fn main() -> int!:\n    print(\"ran\")\n    return ?0\n",
    );
    let (out, _err, outcome, _rc) = crate::vm::run_file_entry(&entry, "main");
    assert!(
        outcome.is_ok(),
        "entry fn returning Ok must run clean, got {outcome:?}"
    );
    assert!(out.contains("ran"), "expected 'ran' in output");
}

#[test]
fn parity_index_assign() {
    assert_golden_out(
        "xs := [1, 2, 3]\nxs[1] = 9\nxs[0] += 4\nxs[2] -= 1\nprint(xs)\n",
        "[5, 9, 2]\n",
    );
}

#[test]
fn parity_index_assign_out_of_bounds() {
    assert_eq!(
        vm_outcome("xs := [1, 2, 3]\nxs[9] = 0\nprint(xs)\n").unwrap_err(),
        "runtime error (line 2, col 1): index 9 out of bounds (len 3)"
    );
}

#[test]
fn parity_compound_index_oob_vs_rhs_error_order() {
    // Compound `xs[i] += rhs` on an out-of-bounds `i` where `rhs` ALSO errors: the bounds-check
    // error must win. The VM reads the target (bounds-check) before `rhs`.
    assert_eq!(
        vm_outcome("xs := [1, 2, 3]\nz := 0\nxs[5] += 1 / z\n").unwrap_err(),
        "runtime error (line 3, col 1): index 5 out of bounds (len 3)"
    );
}

#[test]
fn parity_compound_index_oob_skips_rhs_side_effect() {
    // On an out-of-bounds compound assign, the rhs side effect must not run.
    let src = "fn side() -> int:\n    print(\"rhs ran\")\n    return 0\nxs := [1, 2, 3]\nxs[5] += side()\nprint(\"after\")\n";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 5, col 1): index 5 out of bounds (len 3)",
        "for:\n{src}"
    );
}

#[test]
fn parity_field_assign() {
    assert_golden_out(
        "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\np.y += 3\nprint(p.x)\nprint(p.y)\n",
        "9\n5\n",
    );
}

// NOTE: method_type_params / param_protocol / method_default_args were parity-only; they are
// now full golden tests (`golden_*_chz_matches_expected_and_interp` above), which assert exact
// output against a literal golden, so the weaker `parity_*` file tests were removed.

#[test]
fn parity_hof_param() {
    let src = "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nprint(apply(inc, 4))\n";
    assert_golden_out(src, "5\n");
    assert_eq!(vm_outcome(src).unwrap(), "5\n");
}

/// Integer `sum()` must use checked arithmetic and raise the SAME recoverable
/// "integer overflow in Add" runtime error as `+`, not silently wrap.
#[test]
fn parity_list_sum_overflow() {
    let src = "print([9223372036854775807, 1].sum())\n";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 1, col 7): integer overflow in Add",
        "for:\n{src}"
    );
    let err = vm_outcome(src).expect_err("expected a runtime error, not wraparound");
    assert!(
        err.contains("integer overflow in Add"),
        "unexpected error: {err}"
    );
}

/// A list containing any float takes the float path — would-overflow ints must NOT fault
/// (guards against the int checked_add being hoisted above any_float).
#[test]
fn parity_list_sum_mixed_float() {
    let src = "print([9223372036854775807.0, 1.0, 0.0].sum())\n";
    assert_golden_out(src, "9.223372036854776e+18\n");
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
    assert_golden_out(src, "n1\nn2\nn3\n");
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
    assert_golden_out(src, "2\n");
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
    assert_golden_out(src, "2\nbb\n");
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
    assert_golden_out(src, "abc\n");
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
    assert_golden_out(src, "a\nb\ndd\nccc\n");
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
    assert_golden_out(src, "1\n3\n");
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
    // generic-erased, so it prints `2`.
    let src = "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): ident(x))\n    print(ys[0] + 1)\nmain()\n";
    assert_golden_out(src, "2\n");
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
}

#[test]
fn parity_fold_closure_free_generic_call() {
    // Bug D (adversarial-review fix): `xs.fold(0, fn(acc, x): ident(x))` where `ident[T](x: T) -> T`
    // type-checks (fold's `U` pinned `int` by `init`, closure body re-inferred `int`). The reducer
    // returns the last element via identity → `s = 3`, `print(s + 1)` = `4`. Generic-erased runtime.
    let src = "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(x))\n    print(s + 1)\nmain()\n";
    assert_golden_out(src, "4\n");
    assert_eq!(vm_outcome(src).unwrap(), "4\n");
}

#[test]
fn parity_free_fn_hof_map() {
    // Bug D free-fn analog: `mymap([1,2,3], fn(x): x*2)` where `mymap[U](..., fn(int)->U) -> List[U]`
    // type-checks to List[int] (the closure-return loop-back now runs on the free-fn path too). Runtime
    // is generic-erased, so it prints `3` (ys=[2,4,6], ys[0]+1).
    let src = "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): x * 2)\n    print(ys[0] + 1)\nmain()\n";
    assert_golden_out(src, "3\n");
    assert_eq!(vm_outcome(src).unwrap(), "3\n");
}

#[test]
fn parity_free_fn_hof_apply_sibling() {
    // `apply[A,B](f: fn(A)->B, a: A) -> B` with `apply(fn(x): x*2, 5)`: A pinned int by the sibling
    // value arg, B (return-only) recovered from the closure body → int, so `y + 1` = 11.
    let src = "fn apply[A, B](f: fn(A) -> B, a: A) -> B:\n    return f(a)\nfn main():\n    y := apply(fn(x): x * 2, 5)\n    print(y + 1)\nmain()\n";
    assert_golden_out(src, "11\n");
    assert_eq!(vm_outcome(src).unwrap(), "11\n");
}

#[test]
fn parity_free_fn_hof_sibling_closure_param() {
    // adversarial-review bug 1: `pair[T](f: fn()->T, g: fn(T)->int) -> int` with
    // `pair(fn(): 5, fn(x): x + 1)`. `T` is recovered from the FIRST closure's concrete return (int)
    // before the un-inferable-param probe runs, so the SECOND closure's `x: T` param is not rejected.
    // Runtime is generic-erased → prints `6`.
    let src = "fn pair[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f())\nfn main():\n    print(pair(fn(): 5, fn(x): x + 1))\nmain()\n";
    assert_golden_out(src, "6\n");
    assert_eq!(vm_outcome(src).unwrap(), "6\n");
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
/// parallel: nursery, spawn, Executor incl. submit, Shared/RwShared/Atomic, concurrent
/// collections, cancel, timer, and crossings). It is deterministic by construction, so it is two
/// tests at once: the 49 self-checks must all PASS, and the output must byte-match `.expected`.
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
}

/// M6 golden: core-type methods run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_methods_via_run_file() {
    let path = fixture("examples/methods.chz");
    let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_mutate_via_run_file() {
    let path = fixture("examples/mutate.chz");
    let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `examples/collection_ops.chz` — golden coverage of every collection operator (gap #3): list
/// `+`/`*` and set `| & - ^` (multi-element, empty, type-correct mixes). Uses `std.io`, so it
/// runs through `run_file` (module resolution) and asserts byte-identical output against
/// `.expected`.
#[test]
fn golden_collection_ops_via_run_file() {
    let path = fixture("examples/collection_ops.chz");
    let expected = std::fs::read_to_string(fixture("examples/collection_ops.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Golden: `Self` usable in inherent struct/enum method signatures + bodies (not
/// protocols-only). Runs end-to-end on the VM and byte-matches both the `.expected` file and the
/// M:N engine.
#[test]
fn golden_self_method_via_run_file() {
    let path = fixture("examples/self_method.chz");
    let expected = std::fs::read_to_string(fixture("examples/self_method.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Golden: the `Contains` operator protocol (L5) — `x in obj` dispatches to a user
/// `contains(self, item) -> bool` on structs (incl. generic `Box[T]`) and enums. Byte-matches the
/// `.expected` file and the M:N engine.
#[test]
fn golden_contains_protocol_via_run_file() {
    let path = fixture("examples/contains_protocol.chz");
    let expected = std::fs::read_to_string(fixture("examples/contains_protocol.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// Golden: compound assignment (`+=`/`-=`/…) honors struct/enum operator overloading —
/// `a += V(10)` produces the same value as `a = a + V(10)`. Byte-matches the `.expected` file and
/// the M:N engine.
#[test]
fn golden_compound_overload_via_run_file() {
    let path = fixture("examples/compound_overload.chz");
    let expected = std::fs::read_to_string(fixture("examples/compound_overload.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
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
}

// ----- std.io TTY detection (gaps §6) — separated block near golden_std_demo to shrink the
// hand-resolved conflict with the concurrent List-methods task also editing this file. -----
/// `io.isatty()`/`isatty_stdin()`/`isatty_stderr()` each return a plain `bool` without faulting. The
/// value depends on how the harness's real fds are wired (a terminal → true, a pipe/redirect → false
/// — libtest's capture doesn't touch the fd), so we assert bool-SHAPE, NOT a fixed value.
#[test]
fn golden_isatty_via_run_file() {
    let out = golden_entry(
        "import std.io\nio.print(\"{io.isatty()}\")\nio.print(\"{io.isatty_stdin()}\")\nio.print(\"{io.isatty_stderr()}\")\n",
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
/// survives — proving one consistent env source (per-VM HostConfig, deterministic).
#[test]
fn golden_os_setenv_environ_consistency() {
    let src = "import std.os\nos.setenv(\"K\", \"V\")\nmatch os.env(\"K\"):\n    ?v: print(v)\n    None: print(\"NONE\")\nprint(os.environ()[\"K\"])\nprint(os.environ()[\"SEED\"])\n";
    let out = golden_entry_cfg(src, || {
        let mut env = std::collections::HashMap::new();
        env.insert("SEED".to_string(), "1".to_string());
        crate::native::HostConfig {
            env: std::sync::Arc::new(std::sync::Mutex::new(env)),
            ..Default::default()
        }
    });
    assert_eq!(out, "V\nV\n1\n");
}

/// getpid/platform/temp_dir/home_dir: assert SHAPE (values are machine-dependent — no fixed
/// literal). pid>0, platform nonempty, temp_dir
/// nonempty, home_dir is Some/None-shaped.
#[test]
fn golden_os_queries() {
    let out = golden_entry(
        "import std.os\nprint(\"{os.getpid() > 0}\")\nprint(os.platform())\nprint(\"{os.temp_dir() != \"\"}\")\nmatch os.home_dir():\n    ?_: print(\"H\")\n    None: print(\"NH\")\n",
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

/// chdir(abs) -> Ok on a real dir, Err on a missing one. Takes the fs scratch lock + restores
/// cwd (chdir mutates PROCESS-GLOBAL cwd).
#[test]
fn golden_os_chdir() {
    let _g = crate::native::fs::FS_SCRATCH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::current_dir().expect("cwd");
    let out = golden_entry(
        "import std.os\nmatch os.chdir(os.temp_dir()):\n    ?_: print(\"OK\")\n    !_: print(\"ERR\")\nmatch os.chdir(\"/no/such/chezzi/dir\"):\n    ?_: print(\"OK\")\n    !_: print(\"ERR\")\n",
    );
    std::env::set_current_dir(&saved).expect("restore cwd");
    assert_eq!(out, "OK\nERR\n");
}

/// environ() must yield keys in a DETERMINISTIC (sorted) order. The source is a std HashMap whose
/// iteration order is randomized per-instance, so `os_environ` sorts before lowering — otherwise two
/// separate runs (each a fresh HashMap instance) could diverge in key order. This ITERATES the map (not key-index),
/// exercising the ordering the three key-lookup tests never touch.
#[test]
fn golden_os_environ_deterministic_order() {
    let src = "import std.os\nfor k in os.environ().keys():\n    print(k)\n";
    let out = golden_entry_cfg(src, || {
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

/// setenv inside a spawned task must be visible to the parent after the nursery joins. The
/// M:N engine gives each worker a SHARED (Arc) handle to the one env map, not a per-worker clone, so
/// the write survives back to the parent. Process-global env, like Python `os.environ` / Go
/// `os.Setenv` (visible across threads). Without sharing, this would print "unset" instead.
#[test]
fn golden_os_setenv_visible_across_tasks() {
    let src = "import std.io\nimport std.os\nfn t():\n    os.setenv(\"TASKVAR\", \"fromtask\")\nfn main():\n    parallel:\n        spawn: t()\n    match os.env(\"TASKVAR\"):\n        ?v: io.print(v)\n        None: io.print(\"unset\")\nmain()";
    assert_eq!(
        golden_entry_cfg(src, crate::native::HostConfig::default),
        "fromtask\n"
    );
}

/// Additive std.math trig/exp/log intrinsics run end-to-end on the VM and byte-match the
/// `.expected` file.
#[test]
fn golden_math_more_via_run_file() {
    let path = fixture("examples/math_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/math_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.bisect` — binary search + sorted-insert over the full boundary matrix (empty, all-equal,
/// both ends, duplicate left/right boundary, in-place insort). Runs end-to-end on the VM, byte-matches
/// the `.expected` file.
#[test]
fn golden_bisect_via_run_file() {
    let path = fixture("examples/bisect.chz");
    let expected = std::fs::read_to_string(fixture("examples/bisect.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.memoize` — `memoize1` caches per distinct argument (single-eval proven by a captured
/// call-counter: two distinct args → counter == 2, cache hits do NOT re-run `f`). Runs end-to-end on
/// the VM, byte-matches the `.expected` file.
#[test]
fn golden_memoize_via_run_file() {
    let path = fixture("examples/memoize.chz");
    let expected = std::fs::read_to_string(fixture("examples/memoize.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.duration` — Go-like time spans (pure-Chezzi). `to_string`/`parse` across the shape space,
/// parse round-trips, malformed → Err, accessors + arithmetic. Deterministic (no clock) → runs
/// end-to-end on the VM, byte-matches the `.expected` file.
#[test]
fn golden_duration_via_run_file() {
    let path = fixture("examples/duration.chz");
    let expected = std::fs::read_to_string(fixture("examples/duration.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

// ----- std.csv (gaps §7) — separate labeled block to shrink the hand-resolved conflict with the
// concurrent std.os task also editing this file. -----
/// `std.csv` — RFC 4180 parse/format round-trip over every hard case (embedded comma, `""` escaped
/// quote, embedded newline in a quoted field, empty field, sandwiched empty record, unicode) plus
/// direct parse asserts (empty input -> [], trailing separator -> no spurious record). Runs
/// end-to-end on the VM, byte-matches the `.expected` file.
#[test]
fn golden_csv_via_run_file() {
    let path = fixture("examples/csv.chz");
    let expected = std::fs::read_to_string(fixture("examples/csv.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `Channel.trip()` — the manual level-trigger latch (behind `std.cancel`'s `done()`). Tripping
/// makes the channel permanently ready (`recv`/`try_recv` yield `true`, fanning out). Deterministic
/// on every engine → a golden test.
#[test]
fn golden_channel_trip_via_run_file() {
    let path = fixture("examples/channel_trip.chz");
    let expected = std::fs::read_to_string(fixture("examples/channel_trip.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.cancel` — a manual token: `cancelled()`/`reason()` before/after `cancel()`, `done()`
/// readiness, idempotent re-cancel. Pure `Shared`/latch, no timing → a golden test.
#[test]
fn golden_cancel_manual_via_run_file() {
    let path = fixture("examples/cancel_manual.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_manual.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.cancel` — timeouts + `wait:` integration (`done()` as a wait arm). All cases deterministic
/// (pre-loaded channels / already-elapsed deadline) → a golden test.
#[test]
fn golden_cancel_timeout_wait_via_run_file() {
    let path = fixture("examples/cancel_timeout_wait.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/cancel_timeout_wait.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
}

/// `std.cancel` — cooperative CPU-loop cancellation on the M:N engine: a sibling's manual
/// `cancel()` aborts the polling worker early (preemption), so it has no `.expected` (cf.
/// `examples/parallel_cancel.chz`).
#[test]
fn cancel_cpu_aborts_early_on_mn() {
    let path = fixture("examples/cancel_cpu.chz");
    let (par_out, _e, par_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, "worker aborted early\n",
        "the M:N engine should preempt + abort"
    );
}
