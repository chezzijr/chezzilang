//! VM tests: pipe chains, snapshot cache, cross-module witness defer, TICKET-063 guards.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== Multi-line pipe chains (leading-`|>` line continuation) + `iter.sum` =====

/// A line starting with `|>` continues the previous logical line — same result as the one-liner,
///. Also covers a chain nested inside a fn body.
#[test]
fn pipe_multiline_chain_both_engines() {
    let src = "\
fn dbl(x: int) -> int:
    return x * 2

fn inc(x: int) -> int:
    return x + 1

fn f(n: int) -> int:
    r := n
        |> dbl()
        # keep going
        |> inc()
    return r

r := 5
    |> dbl()
    |> inc()
print(r)
print(f(10))
";
    assert_mc_parity(src, "11\n21\n");
}

/// `iter.sum` delegates to the native List `sum` method — including its empty-list → `0`.
#[test]
fn iter_sum_delegates_and_empty_is_zero() {
    let src = "import std.iter\nprint(iter.sum([1, 2, 3]))\nprint(iter.sum([]))\n";
    let entry = write_temp_chz("iter_sum", src);
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "serial iter.sum faulted: {res:?}");
    assert_eq!(out, "6\n0\n", "serial");
    let (pout, _pe, pres, _pc) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(pres.is_ok(), "M:N iter.sum faulted: {pres:?}");
    assert_eq!(pout, "6\n0\n", "M:N");
}

/// The verbatim docs/reference/syntax/ §11 pipe example must run and print 60.
#[test]
fn docs_syntax_pipe_example_runs_on_both_engines() {
    let src = "\
import std.iter

total := [1, 2, 3, 4]
    |> iter.filter(fn(x: int) -> bool: x % 2 == 0)   # → iter.filter([1,2,3,4], ...)
    |> iter.map(fn(x: int) -> int: x * 10)
    |> iter.sum()
print(total)                                         # 60
";
    let entry = write_temp_chz("docs_s11", src);
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "serial docs §11 faulted: {res:?}");
    assert_eq!(out, "60\n", "serial");
    let (pout, _pe, pres, _pc) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(pres.is_ok(), "M:N docs §11 faulted: {pres:?}");
    assert_eq!(pout, "60\n", "M:N");
}

/// Regression net for the check-time `return`-in-`defer:`/`spawn:` rejection: every legal nesting
/// of `defer:`/`spawn:`/`parallel:` (plus a nested `fn` that DOES `return`, declared inside each block)
/// must keep running identically. If the checker guard over-rejects, this goes red.
#[test]
fn defer_spawn_nesting_matrix_parity() {
    let src = "\
fn f() -> int:
    defer:
        fn g() -> int:
            return 40
        print(\"nested-fn-in-defer {g()}\")
        defer:
            print(\"defer-in-defer\")
        parallel:
            spawn print(\"spawn-call-in-defer\")
        parallel:
            spawn:
                print(\"spawn-block-in-defer\")
    parallel:
        spawn:
            fn h() -> int:
                return 41
            print(\"nested-fn-in-spawn {h()}\")
            defer:
                print(\"defer-in-spawn\")
    return 1
print(f())
";
    assert_mc_parity(
        src,
        "nested-fn-in-spawn 41\ndefer-in-spawn\nnested-fn-in-defer 40\nspawn-call-in-defer\nspawn-block-in-defer\ndefer-in-defer\n1\n",
    );
}

// ---------------------------------------------------------------------------------------------
// R1 — the bytes native seam (`NativeRet::Bytes` / `Host::arg_bytes` / `NativeArg::Bytes`).
// ---------------------------------------------------------------------------------------------

/// Run a std-module-importing program on BOTH engines (a `import std.*` program needs the resolver,
/// so it runs from a file) and assert identical stdout.
///
/// TYPE-CHECKS FIRST — `run_file`/`run_file` are resolve → compile → run and skip `check_graph`
/// (the known test-helper-vs-CLI divergence), so without this gate a test could assert a program
/// green that `chezzi run` rejects outright. Every R1 program here is what the CLI would run.
fn assert_mc_parity_file(tag: &str, src: &str, expected: &str) {
    let entry = write_temp_chz(tag, src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        panic!("program must type-check ({tag}), got: {errs:?}");
    }
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "VM faulted: {res:?}");
    assert_eq!(out, expected, "VM output");
    let (out_p, _e, res_p, _c) = run_file(&entry);
    assert!(res_p.is_ok(), "M:N engine faulted: {res_p:?}");
    assert_eq!(out_p, expected, "M:N engine");
    let _ = std::fs::remove_file(&entry);
}

/// R1 step 1 — a native fn RETURNING bytes lowers to `Obj::Bytes` (`NativeRet::Bytes`).
#[test]
fn native_ret_bytes_lowers_to_bytes() {
    let src = "\
import std.encoding

fn go() -> int!:
    b := encoding.base64_decode_bytes(\"AAD/\")?
    print(b.len())
    print(b[0])
    print(b[2])
    return ?0

fn main():
    match go():
        ?_: print(\"ok\")
        !e: print(\"ERR:\" + e.message())
main()
";
    assert_mc_parity_file("r1_ret_bytes", src, "3\n0\n255\nok\n");
}

/// R1 step 2 — `Host::arg_bytes` carries a `bytes` arg across the seam. The seam is `bytes`-ONLY: a
/// `bytearray` buffer is converted with `bytes(ba)` (checker rule 7b29552, pinned by
/// `checker::tests::bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert`), and that
/// converted buffer hashes to the same digest as the literal.
#[test]
fn host_arg_bytes_accepts_bytes_and_a_converted_bytearray() {
    let src = "\
import std.crypto
import std.encoding

fn main():
    print(crypto.sha256_bytes(b\"abc\"))
    print(crypto.sha256_bytes(bytes(bytearray(b\"abc\"))))
    print(encoding.base64_encode_bytes(b\"\\x00\\xff\"))
main()
";
    let d = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_mc_parity_file("r1_arg_bytes", src, &format!("{d}\n{d}\nAP8=\n"));
}

/// R1 step 2 — a non-bytes arg to a bytes-taking native. In real code this is a CHECK-time error
/// (the param is typed `bytes`; the assignability rule is pinned by
/// `checker::tests::bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert`), so the
/// seam's runtime arg-type fault is pinned DIRECTLY on `VmHost`
/// rather than through a .chz program that could never compile.
#[test]
fn host_arg_bytes_rejects_non_bytes() {
    use crate::native::Host;
    let entry = write_temp_chz(
        "r1_arg_bytes_bad",
        "import std.crypto\n\nfn main():\n    print(crypto.sha256_bytes(7))\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let errs =
        crate::checker::check_graph(&graph).expect_err("a non-bytes arg must not type-check");
    let _ = std::fs::remove_file(&entry);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected bytes, found int")),
        "{errs:?}"
    );

    let mut vm = Vm::new(Arc::new(empty_program()));
    let mut host = VmHost {
        vm: &mut vm,
        args: vec![Value::int(7)],
    };
    let err = host.arg_bytes(0).expect_err("int is not bytes");
    assert_eq!(err.message, "argument 0 must be bytes, got int");
}

/// R1 step 3 — `Vm::extract_native_args` carries a `bytes` arg across the D5 off-heap
/// handoff (without this NO blocking native can take a bytes arg: extraction returns `None`, the
/// call silently runs INLINE and pins a core worker — invisible black-box, hence this direct test).
#[test]
fn extract_native_args_carries_bytes() {
    use crate::native::NativeArg as A;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let b = Value::obj(vm.heap.alloc(Obj::Bytes(vec![0u8, 255].into_boxed_slice())));
    assert_eq!(
        vm.extract_native_args(&[b, Value::int(3)]),
        Some(vec![A::Bytes(vec![0, 255]), A::Int(3)])
    );
    // A `bytearray` is NOT a seam arg (the checker rejects it at a `bytes` param): extraction bails
    // to `None` → the call would run inline, never off-heap with a stale copy of a mutable buffer.
    let ba = Value::obj(vm.heap.alloc(Obj::ByteArray(vec![1u8, 2])));
    assert_eq!(vm.extract_native_args(&[ba]), None);
}

/// R1 step 3 — the off-heap host SERVES a pre-extracted bytes arg (leaving it on the trait default
/// would make a blocking bytes native fail ONLY under M:N).
#[test]
fn offload_host_serves_bytes_arg() {
    use crate::native::{Host, NativeArg as A};
    let mut h = OffloadHost {
        args: vec![A::Bytes(vec![7, 8]), A::Str("x".into())],
    };
    assert_eq!(h.arg_bytes(0).unwrap(), vec![7u8, 8]);
    assert!(h.arg_bytes(1).is_err());
}

/// R1 step 4 — a real BINARY file (NULs, 0xFF, invalid UTF-8) round-trips byte-exactly through
/// `io.write_bytes` → `io.read_bytes` (both BLOCKING natives — the end-to-end proof of the
/// `NativeArg::Bytes` offload path), and `io.read_file` on it errs with a hint at `read_bytes`.
#[test]
fn io_bytes_round_trips_a_binary_file() {
    let path = std::env::temp_dir().join(format!("chezzi_r1_bin_{}.dat", std::process::id()));
    let p = path.to_str().unwrap().to_string();
    let src = format!(
        "\
import std.io

fn go() -> int!:
    b := bytes([0, 255, 254, 128])
    io.write_bytes(\"{p}\", b)?
    back := io.read_bytes(\"{p}\")?
    print(back.len())
    for x in back:
        print(x)
    print(back == b)
    match io.read_file(\"{p}\"):
        ?s: print(\"decoded?! \" + s)
        !e: print(e.message().contains(\"read_bytes\"))
    return ?0

fn main():
    match go():
        ?_: print(\"ok\")
        !e: print(\"ERR:\" + e.message())
main()
"
    );
    assert_mc_parity_file("r1_io_bin", &src, "4\n0\n255\n254\n128\ntrue\ntrue\nok\n");
    let _ = std::fs::remove_file(&path);
}

/// R1 — REGRESSION PIN: a `bytes` / `bytearray` value already crosses the task airlock
/// (`WireValue::Bytes`/`ByteArray`). R1 must not regress it.
#[test]
fn bytes_crosses_the_task_airlock() {
    let src = "\
fn worker(ch: Channel[bytes], b: bytes):
    ch.send(b)

fn main():
    ch := Channel[bytes](1)
    parallel:
        spawn worker(ch, b\"\\x00A\\xff\")
    got := ch.recv()
    print(got.len())
    print(got[0])
    print(got[2])
    ba := bytearray(b\"\\x01\")
    parallel:
        spawn print(ba.len())
main()
";
    assert_mc_parity(src, "3\n0\n255\n1\n");
}

/// **W6-3 structural ratchet — every intrinsic protocol grant must be CALLABLE at runtime.**
///
/// The checker grants built-ins protocol conformance INTRINSICALLY (no user method) at the
/// `grant_intrinsic` early-outs in `checker::proto::satisfies_args_d`, and the granted method must
/// therefore be callable from an erased generic body. That pairing was honored for 2 of ~11 grants and
/// broke the rest (`type int has no method 'add'` after `check: ok`).
///
/// The rows are keyed on `(protocol, method, receiver-KIND)` because the receiver kind is the axis
/// W6-3 actually failed on: `compare`/`str` *were* paired, but their interceptions were type-gated
/// narrower than the checker's grant set. This test generates one probe program PER ROW from a
/// receiver-literal table and a call template, and RUNS it. Widening a grant to one
/// more type therefore cannot pass: `grant_intrinsic`'s `debug_assert` demands the new row, and the
/// row demands a probe that actually runs.
///
/// The probes are deliberately NOT generic — `run_capture` compiles without the checker and the
/// compiler is type-blind, so `a := <literal>` + `a.<method>(…)` reaches the exact same
/// `Vm::do_method_call` dispatch an erased `[T: P]` body does, with no bound/type-arg noise. The
/// generic-body spelling, operator equivalence, and fault-text equality are covered by
/// `tests/chz/spec/intrinsic_proto_methods_test.chz`.
#[test]
fn intrinsic_grants_all_have_vm_arms() {
    use crate::checker::proto::{INTRINSIC_PROTO_METHODS, INTRINSIC_UNPAIRED, RecvKind};
    /// One receiver per kind `Checker::intrinsic_recv_kind` can classify a grant as — the value to
    /// probe with, plus the literals/types the parameterized bounds need. `"-"` = the kind has no
    /// grant needing that column. `struct` needs ONE struct satisfying all three struct-granted
    /// protocols at once: zero fields and no `hash` (intrinsic `Hashable`) plus `next(self) -> int?`
    /// (`Iterator`, and `Iterable` through it).
    struct Recv {
        kind: RecvKind,
        prelude: &'static str,
        lit: &'static str,
        key: &'static str,
        val: &'static str,
        elem_ty: &'static str,
        key_ty: &'static str,
        val_ty: &'static str,
        slice_ty: &'static str,
    }
    let r = |kind, prelude, lit, key, val, elem_ty, key_ty, val_ty, slice_ty| Recv {
        kind,
        prelude,
        lit,
        key,
        val,
        elem_ty,
        key_ty,
        val_ty,
        slice_ty,
    };
    // The `Contains` column: a needle literal and its type per kind (`"-"` = the kind has no
    // `Contains` grant, so the bound spells `int` as a stand-in). Every kind is named.
    let item_of = |kind: RecvKind| -> (&'static str, &'static str) {
        match kind {
            RecvKind::List | RecvKind::Set => ("1", "int"),
            RecvKind::Map => ("\"a\"", "str"),
            RecvKind::Str => ("\"b\"", "str"),
            RecvKind::Bytes | RecvKind::ByteArray => ("98", "int"),
            RecvKind::Int
            | RecvKind::Float
            | RecvKind::Bool
            | RecvKind::Tuple
            | RecvKind::Struct
            | RecvKind::Cursor
            | RecvKind::Enum
            | RecvKind::Option
            | RecvKind::Result
            | RecvKind::Func
            | RecvKind::Protocol
            | RecvKind::Channel
            | RecvKind::Shared
            | RecvKind::RwShared
            | RecvKind::Atomic => ("-", "int"),
        }
    };
    let recvs: &[Recv] = &[
        r(RecvKind::Int, "", "7", "-", "-", "-", "-", "-", "-"),
        r(RecvKind::Float, "", "1.5", "-", "-", "-", "-", "-", "-"),
        r(RecvKind::Bool, "", "true", "-", "-", "-", "-", "-", "-"),
        r(
            RecvKind::Str,
            "",
            "\"abc\"",
            "0",
            "-",
            "str",
            "int",
            "str",
            "str",
        ),
        r(
            RecvKind::Bytes,
            "",
            "b\"abc\"",
            "0",
            "-",
            "int",
            "int",
            "int",
            "bytes",
        ),
        r(
            RecvKind::ByteArray,
            "",
            "bytearray(b\"abc\")",
            "0",
            "1",
            "int",
            "int",
            "int",
            "bytearray",
        ),
        r(
            RecvKind::List,
            "",
            "[1, 2, 3]",
            "0",
            "9",
            "int",
            "int",
            "int",
            "List[int]",
        ),
        r(
            RecvKind::Set,
            "",
            "Set([1, 2])",
            "-",
            "-",
            "int",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Map,
            "",
            "{\"a\": 1}",
            "\"a\"",
            "9",
            "(str, int)",
            "str",
            "int",
            "-",
        ),
        r(
            RecvKind::Struct,
            "struct Cur:\n    fn next(self) -> int?:\n        return None\n",
            "Cur()",
            "-",
            "-",
            "int",
            "-",
            "-",
            "-",
        ),
        // D1 — the kinds the widened `Eq` grant added. `enum`/`option`/`result` are ONE runtime
        // shape (`Obj::Enum`), so their probes are what pin the `Obj::Enum` dispatch fallback.
        r(RecvKind::Tuple, "", "(1, 2)", "-", "-", "-", "-", "-", "-"),
        r(
            RecvKind::Enum,
            "enum En:\n    A\n    B\n",
            "En.A",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Option,
            "fn ho() -> int?:\n    return 1\n",
            "ho()",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Result,
            "fn hr() -> int!str:\n    return 1\n",
            "hr()",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        // W7-54 — a function value's `Eq` grant. Only the `eq` template applies; it uses no other
        // column.
        r(
            RecvKind::Func,
            "fn g(x: int) -> int:\n    return x\n",
            "g",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        // W7-52 — a protocol-typed (existential) value's `Eq` grant. Can't write `a: Sized_ =
        // Tag(1)` (the probe template is a bare `a := {lit}`, no annotation), so the prelude carries
        // a factory whose RETURN type is the protocol — `a := mkp()` then really infers `a: Ty::
        // Protocol("Sized_", [])`, not `Ty::Struct("Tag", [])`. Only the `eq` template applies.
        r(
            RecvKind::Protocol,
            "protocol Sized_:\n    fn size(self) -> int\nstruct Tag:\n    n: int\n    fn size(self) -> int:\n        return self.n\nfn mkp() -> Sized_:\n    return Tag(1)\n",
            "mkp()",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        // TICKET-257 — the built-in cursor (`Iterator[T]`) and the four handle kinds.
        r(
            RecvKind::Cursor,
            "",
            "[1, 2].iter()",
            "-",
            "-",
            "int",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Channel,
            "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\nfn mkc() -> Channel[int]:\n    c := Channel[int](1)\n    c.close()\n    return c\n",
            "mkc()",
            "-",
            "-",
            "int",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Shared,
            "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\n",
            "Shared(1)",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::RwShared,
            "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\n",
            "RwShared(1)",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        r(
            RecvKind::Atomic,
            "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\n",
            "Atomic(1)",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
    ];
    // Every `RecvKind` has a receiver.
    for k in RecvKind::ALL {
        assert!(
            recvs.iter().any(|r| r.kind == k),
            "RecvKind::{k} has no receiver in intrinsic_grants_all_have_vm_arms"
        );
    }
    // (method, call template) — `{r}` is the receiver, `{k}`/`{v}` the index key/value.
    let calls: &[(&str, &str)] = &[
        ("compare", "{r}.compare(b)"),
        ("eq", "{r}.eq(b)"),
        ("str", "{r}.str()"),
        ("hash", "{r}.hash()"),
        ("message", "{r}.message()"),
        ("as_path", "{r}.as_path()"),
        ("iter", "{r}.iter()"),
        ("next", "{r}.next()"),
        ("index", "{r}.index({k})"),
        ("set_index", "{r}.set_index({k}, {v})"),
        ("slice", "{r}.slice(0..1)"),
        ("contains", "{r}.contains({i})"),
        ("bitand", "{r}.bitand(b)"),
        ("bitor", "{r}.bitor(b)"),
        ("bitxor", "{r}.bitxor(b)"),
        ("shl", "{r}.shl(b)"),
        ("shr", "{r}.shr(b)"),
        ("add", "{r}.add(b)"),
        ("sub", "{r}.sub(b)"),
        ("mul", "{r}.mul(b)"),
        ("div", "{r}.div(b)"),
        ("mod", "{r}.mod(b)"),
        ("neg", "{r}.neg()"),
    ];
    // Build the probe for a row, or `None` if the tables can't express it (which the assertions
    // below turn into a failure — a row must never be silently skipped).
    let recv_of = |kind: RecvKind| recvs.iter().find(|r| r.kind == kind);
    let probe = |method: &str, kind: RecvKind| -> Option<String> {
        let rv = recv_of(kind)?;
        let (_, tmpl) = *calls.iter().find(|(m, _)| *m == method)?;
        // An unfilled ("-") column the template needs = a missing table entry, not a skip.
        if (tmpl.contains("{k}") && rv.key == "-")
            || (tmpl.contains("{v}") && rv.val == "-")
            || (tmpl.contains("{i}") && item_of(kind).0 == "-")
        {
            return None;
        }
        let call = tmpl
            .replace("{r}", "a")
            .replace("{k}", rv.key)
            .replace("{v}", rv.val)
            .replace("{i}", item_of(kind).0);
        // TICKET-250 — a `next` owner is its own cursor for `iter` only where the call is typed
        // through the `Iterable` requirement (`Op::CallHook`); a concrete `a.iter()` on it is a
        // check error (`has no method 'iter'`). So this one row is probed through a bound.
        if method == "iter" && kind == RecvKind::Struct {
            return Some(format!(
                "import Iterable, Iterator from std.iter\n{}fn via[S: Iterable[int]](s: S) -> Iterator[int]:\n    return s.iter()\na := {}\nb := {}\nres := via(a)\n",
                rv.prelude, rv.lit, rv.lit
            ));
        }
        // BIND the result: a bare `Option`/`Result`-valued expression statement auto-propagates
        // (`unhandled error: None`), which would mask the dispatch this probe is about.
        Some(format!(
            "{}a := {}\nb := {}\nres := {call}\n",
            rv.prelude, rv.lit, rv.lit
        ))
    };
    // The CHECKER half: bind the receiver to a `[T: Protocol]` param (no method call needed — the
    // bound alone forces the conformance decision). Type-checking this per row (a) proves the grant
    // for that (protocol, receiver-kind) really EXISTS, so a stale row fails, and (b) is what actually
    // executes `Checker::grant_intrinsic`, whose `debug_assert` fires when a grant has no row — i.e.
    // this loop is what makes widening a grant to one more TYPE a test failure.
    let bound_probe = |protocol: &str, kind: RecvKind| -> String {
        let rv = recv_of(kind).expect("every kind has a receiver");
        // A parameterized protocol's bound must state its type args (arity is checked), so spell them
        // from the receiver's own element/key/value/slice types. A `"-"` column means the kind has no
        // grant for that protocol, so the arg is irrelevant to the (rejecting) answer — `int` stands in
        // rather than skipping the cell, because a SKIPPED cell is a hole in the matrix.
        let t = |c: &'static str| if c == "-" { "int" } else { c };
        let bound = match protocol {
            "Iterable" | "Iterator" => format!("{protocol}[{}]", t(rv.elem_ty)),
            "Index" | "IndexSet" => format!("{protocol}[{}, {}]", t(rv.key_ty), t(rv.val_ty)),
            "Slice" => format!("Slice[{}]", t(rv.slice_ty)),
            "Contains" => format!("Contains[{}]", item_of(kind).1),
            _ => protocol.to_string(),
        };
        // A std protocol other than the prelude's `Any` and `Error` binds only through an import.
        let import = match crate::checker::std_protocol_owner(protocol) {
            Some("std.prelude") | None => String::new(),
            Some(owner) => format!("import {protocol} from {owner}\n"),
        };
        format!(
            "{import}{}fn probe[T: {bound}](a: T):\n    pass\nprobe({})\n",
            rv.prelude, rv.lit
        )
    };
    let type_errors = |src: &str| -> Vec<String> {
        // A probe that imports a native std module (`Shared`, `RwShared`, `Atomic`) resolves only
        // through `build_graph`, the wire the CLI uses; a single-module check cannot see the type.
        if src.contains("import ") {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("chezzi_vm_probe_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            let entry = dir.join("main.chz");
            std::fs::write(&entry, src).unwrap();
            let graph = crate::resolver::build_graph(&entry).expect("probe should resolve");
            let errs = crate::checker::check_graph(&graph)
                .err()
                .unwrap_or_default()
                .iter()
                .map(|e| e.message.clone())
                .collect();
            let _ = std::fs::remove_dir_all(&dir);
            return errs;
        }
        let tokens = crate::lexer::tokenize(src).expect("probe should lex");
        let module = crate::parser::parse(tokens).expect("probe should parse");
        crate::checker::check(&module)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.message.clone())
            .collect()
    };
    // **The (protocol × receiver-kind) MATRIX — the half that catches a widened grant.** An assert
    // inside `grant_intrinsic` can only fire on inputs a test actually feeds it, so sweep the FULL
    // cross product of every registered protocol against every receiver kind, and require the set of
    // cells the checker ACCEPTS to equal the set of registered rows. Adding `Ty::Bytes` to the
    // `Comparable` grant (or `Ty::Float` to `Hashable`, or `Ty::Set` to `index_kv`) flips one cell to
    // accepted and fails HERE — the per-type hole a protocol-keyed table cannot see.
    let protocols: Vec<&str> = {
        let mut ps: Vec<&str> = INTRINSIC_PROTO_METHODS
            .iter()
            .chain(INTRINSIC_UNPAIRED)
            .map(|(p, _, _)| *p)
            .collect();
        ps.sort_unstable();
        ps.dedup();
        ps
    };
    let mut accepted: Vec<(&str, RecvKind)> = Vec::new();
    for p in &protocols {
        for rv in recvs {
            let src = bound_probe(p, rv.kind);
            let errs = type_errors(&src);
            if errs.is_empty() {
                accepted.push((p, rv.kind));
            } else {
                // A registered row the checker does NOT grant is a stale row (or a broken probe).
                assert!(
                    !INTRINSIC_PROTO_METHODS
                        .iter()
                        .chain(INTRINSIC_UNPAIRED)
                        .any(|(tp, _, tk)| tp == p && *tk == rv.kind),
                    "registered row ({p}, {}) claims an INTRINSIC grant the checker does not give: \
                     {errs:?}\n--- probe ---\n{src}",
                    rv.kind
                );
            }
        }
    }
    let mut registered: Vec<(&str, RecvKind)> = INTRINSIC_PROTO_METHODS
        .iter()
        .chain(INTRINSIC_UNPAIRED)
        .map(|(p, _, k)| (*p, *k))
        .collect();
    registered.sort_unstable();
    registered.dedup();
    accepted.sort_unstable();
    accepted.dedup();
    assert_eq!(
        accepted, registered,
        "the (protocol, receiver-kind) cells the checker grants no longer match \
         INTRINSIC_PROTO_METHODS ∪ INTRINSIC_UNPAIRED — a grant was widened or narrowed. Add/remove \
         the row AND make the method callable at runtime (W6-3)"
    );
    for (p, m, kind) in INTRINSIC_PROTO_METHODS {
        let src = probe(m, *kind).unwrap_or_else(|| {
            panic!(
                "no probe for intrinsic grant ({p}, {m}, {kind}) — add its receiver literal / call \
                 template to intrinsic_grants_all_have_vm_arms (W6-3)"
            )
        });
        for (engine, run) in [
            (
                "serial",
                run_capture as fn(&str) -> Result<String, RuntimeError>,
            ),
            ("M:N", run_capture),
        ] {
            if let Err(e) = run(&src) {
                panic!(
                    "intrinsic grant ({p}, {m}, {kind}) is NOT callable on the {engine} engine: \
                     {e}\n--- probe ---\n{src}"
                );
            }
        }
    }
    // A registered carve-out must STAY a fault (see `INTRINSIC_UNPAIRED`) — if a later change makes it
    // work, the row must be retired with it. `INTRINSIC_UNPAIRED` is EMPTY since W6-3b (the raw-
    // collection `Iterator` grant was narrowed away), so this loop and the disjointness loop below are
    // currently no-ops; they stay so the ratchet re-arms the moment a new unpairable grant is added.
    for (p, m, kind) in INTRINSIC_UNPAIRED {
        let src = probe(m, *kind)
            .unwrap_or_else(|| panic!("no probe for unpaired grant ({p}, {m}, {kind})"));
        let Err(err) = run_capture(&src) else {
            panic!("({p}, {m}, {kind}) is the documented carve-out — it must still fault");
        };
        assert!(
            err.to_string().contains(&format!("has no method '{m}'")),
            "unexpected ({p}, {m}, {kind}) fault: {err}"
        );
    }
    // No row may appear in both tables (a paired row that also claims to be a carve-out).
    for row in INTRINSIC_UNPAIRED {
        assert!(
            !INTRINSIC_PROTO_METHODS.contains(row),
            "{row:?} is registered as BOTH paired and unpaired"
        );
    }
}

/// W6-2 — compile + run `src` on the VM and report how many module-global SNAPSHOTS it built
/// (`snapshot_builds`, bumped on every `ensure_snapshot` cache MISS). Reaches a private counter, so it
/// lives here rather than in a Chezzi test; a timing bench can only hint at what this counts exactly.
fn snapshot_builds_for(src: &str) -> usize {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    vm.snapshot_builds
}

/// W6-2 / TICKET-208 / TICKET-213 — the snapshot cache short-circuits only where that is correct.
/// A task sees its globals as of its own start, and [`Vm::fresh_view`] rebuilds the view only when
/// it CHANGED since the cached build:
///
/// * all-immutable globals (`reusable`) → ONE build for the whole run, however many nurseries;
/// * a mutable aggregate global nobody mutates → ONE build too: the heap's view epoch
///   (`Heap::view_epoch`) did not move, so every spawn shares the snapshot (TICKET-208 built one
///   per spawn: 2000 spawns over a 100000-item global held 2000 copies at one worker);
/// * an in-place mutation of anything a global reaches → one build per mutate-then-spawn;
/// * a global ASSIGNMENT between spawns → one build per assignment-then-spawn.
#[test]
fn snapshot_cache_short_circuits_per_epoch_not_per_spawn() {
    let three_nurseries = "\nfor i in 0..3:\n    parallel:\n        spawn: pass\n";
    assert_eq!(
        snapshot_builds_for(&format!("n: int = 1\ns := \"x\"{three_nurseries}")),
        1,
        "all-immutable globals: 3 nurseries must share ONE build"
    );
    assert_eq!(
        snapshot_builds_for(&format!("n: int = 1\nq: List[int] = [1]{three_nurseries}")),
        1,
        "an unmutated aggregate global: 3 nurseries share ONE build"
    );
    assert_eq!(
        snapshot_builds_for(
            "q: List[int] = [1]\nparallel:\n    for i in 0..50:\n        spawn: pass\n"
        ),
        1,
        "50 spawns over an unmutated aggregate global: ONE build"
    );
    assert_eq!(
        snapshot_builds_for(
            "q: List[int] = [1]\nxs: List[int] = [1, 2, 3]\nparallel:\n    for x in xs:\n        spawn: pass\n"
        ),
        1,
        "walking a global list between spawns mutates nothing a global reaches: ONE build"
    );
    assert_eq!(
        snapshot_builds_for("g: int = 0\nparallel:\n    spawn: pass\n    g = 1\n    spawn: pass\n"),
        2,
        "a global assignment between two spawns must refresh the view (one build each)"
    );
}

/// TICKET-213 — every in-place mutation of something a global reaches refreshes the view: one row
/// per mutating door, at the root and nested. Each program spawns, mutates, then spawns twice: the
/// mutation costs exactly one rebuild and the third spawn reuses it.
#[test]
fn an_in_place_mutation_between_spawns_refreshes_the_view_once() {
    let decls = "struct P:\n    x: int\n    ys: List[int]\nq: List[int] = [1]\nqq: List[List[int]] = [[1]]\nmp: Map[str, int] = {\"a\": 1}\nst: Set[int] = {1}\np := P(1, [1])\nps: List[P] = [P(1, [1])]\n";
    for mutation in [
        "q.push(2)",
        "q[0] = 5",
        "q.pop()",
        "qq[0].push(2)",
        "mp[\"b\"] = 2",
        "st.insert(2)",
        "p.x = 2",
        "p.ys.push(2)",
        "ps[0].x = 2",
        "ps[0].ys.push(2)",
    ] {
        assert_eq!(
            snapshot_builds_for(&format!(
                "{decls}parallel:\n    spawn: pass\n    {mutation}\n    spawn: pass\n    spawn: pass\n"
            )),
            2,
            "`{mutation}` between two spawns: one rebuild, and the third spawn reuses it"
        );
    }
}

/// W6-2 — a snapshot BUILD failure is CARRIED on the queued task and raised where the task is PREPARED
/// (its nursery's join), never at the `spawn`: that is what keeps a nursery whose tasks are all cancelled
/// (`break` out of `parallel:`) faultless, and what keeps the `parallel:` body's own output ahead of the
/// fault. White-box because the only program-level trigger is a module global deeper than
/// `MAX_STRUCTURAL_DEPTH`, and `to_snap`'s slow arm is O(n^2) on such a chain (~5100 links) — minutes in a
/// debug build, on `main` as much as here, so it cannot live in the test suite.
#[test]
fn a_carried_snapshot_build_error_is_raised_at_task_preparation() {
    let program = crate::compiler::compile_module_standalone(
        &crate::parser::parse(crate::lexer::tokenize("x := 1\n").expect("lex")).expect("parse"),
    )
    .expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    let span = Span {
        line: 7,
        col: 3,
        file: 0,
    };
    let carried = vm.err("simulated snapshot build failure".to_string(), span);
    // Open a nursery by hand (`Op::EnterNursery`'s lockstep stacks) and queue a task whose pin FAILED.
    vm.nurseries.push(Vec::new());
    vm.mn_scopes.push(None);
    vm.nursery_defer_floors.push(0);
    vm.nursery_spans.push(span);
    vm.eager_scheds.push(None);
    vm.nurseries[0].push(QueuedTask {
        call: PendingCall::Call {
            callee: Value::nil(),
            args: Vec::new(),
            span,
        },
        snap: Err(carried.clone()),
        cell_ids: Vec::new(),
        fresh: 0,
    });
    let raised = vm
        .join_nursery()
        .expect_err("the carried build error must surface at the join, which prepares the task");
    assert_eq!(raised.message, carried.message);
    assert_eq!(raised.span, span, "the error keeps its spawn-site span");
}

/// W7-21 — a module GLOBAL holding a fn VALUE, CALLED through the module (`l.BARE()`). The checker
/// used to reject this (it read only `ModuleSig::functions`) while the runtime always supported it:
/// `Op::CallMethod` on an `Obj::Module` looks the member up in the slot table and calls whatever
/// value it finds. This test PASSES PRE-FIX (`run_file` does not run the checker) and is not the
/// fence — the fence is `checker::tests::module_global_of_fn_type_is_callable_qualified`. What it
/// locks is the other half of the claim: that the bytecode/VM path really executes the form the
/// checker now accepts, identically. Both ancestors print 1: CPython `m.G()` where
/// `G = _one`, Go `pkg.G()` where `var G = one`.
#[test]
fn module_global_fn_value_call_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w721_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "fn one() -> int:\n    return 1\n").unwrap();
    std::fs::write(dir.join("l.chz"), "import k\nBARE := k.one\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import l\nprint(l.BARE())\nz := l.BARE\nprint(z())\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    assert_eq!(vm_out, "1\n1\n");
}

/// M24-5b — a receiver-less member call (a STATIC method) IS an ordinary `spawn`/`defer` target.
/// `Op::SpawnRecv`/`DeferMethod` record a RECEIVER value plus a name and a static has none, so the
/// compiler used to load the TYPE NAME as a value and panic in `global_slot` ("global 'Holder' has
/// no slot") on a program `chezzi check` had just called clean. The inconsistency was internal —
/// `print(H.build(3))` and `defer print(H.build(3))` both worked while `defer H.build(3)` did not —
/// and Go accepts its analogue (`defer pkg.F(x)`), so the fix is to make it WORK.
///
/// The spellings are re-derived from `Compiler::receiverless_call_head`, the one predicate that now
/// decides this, NOT from the old refusal's list: a bare type name (`bare_types` — struct, enum, and
/// a `from`-imported type, whose global slot EXISTS and holds `Nil`, so the old head test missed it
/// and the run died late with "type nil has no method"), a type-level turbofish in both carriers, a
/// combined `Type[T].m[U]` (an `Index` OVER the `Field`, which the old guard never even saw), and a
/// generic `T` reached through its `$w:T` witness. The module-qualified heads are the test below.
#[test]
fn a_static_method_head_is_a_spawn_or_defer_target_runs_both_engines() {
    let holder = "struct Holder:\n    v: int\n    fn build(old: int) -> int:\n        print(old)\n        return old\n";
    let srcs = [
        // bare struct static
        format!("{holder}fn body():\n    defer Holder.build(3)\n    print(\"body\")\nbody()\n"),
        // bare enum static
        "enum E:\n    A\n    fn build(old: int) -> int:\n        print(old)\n        return old\nfn body():\n    defer E.build(3)\n    print(\"body\")\nbody()\n".to_string(),
        // type-level turbofish, single-arg (`Index`) and multi-arg (`TypeApply`) carriers
        "struct Gen[K]:\n    k: K\n    fn build(old: int) -> int:\n        print(old)\n        return old\nfn body():\n    defer Gen[int].build(3)\n    print(\"body\")\nbody()\n".to_string(),
        "struct Gen2[K, V]:\n    k: K\n    v: V\n    fn build(old: int) -> int:\n        print(old)\n        return old\nfn body():\n    defer Gen2[int, str].build(3)\n    print(\"body\")\nbody()\n".to_string(),
        // combined turbofish — the method's own `[U]` wraps the `Field` in an `Index`
        "struct Gen[K]:\n    k: K\n    fn build[U](old: int) -> int:\n        print(old)\n        return old\nfn body():\n    defer Gen[int].build[str](3)\n    print(\"body\")\nbody()\n".to_string(),
        // a generic `T`'s static requirement, reached through the frame's hidden `$w:T` witness
        "protocol Tagged:\n    fn tag(old: int) -> int\nstruct Holder:\n    v: int\n    fn tag(old: int) -> int:\n        print(old)\n        return old\nfn body[T: Tagged](x: T):\n    defer T.tag(3)\n    print(\"body\")\nbody(Holder(1))\n".to_string(),
    ];
    for src in &srcs {
        // the frame's own work first, then the deferred/spawned static — Go's ordering
        assert_eq!(run_capture(src).unwrap(), "body\n3\n", "VM on: {src}");
        assert_eq!(
            run_capture(src).unwrap(),
            "body\n3\n",
            "repeat run on: {src}"
        );
    }
    // The SPAWNED twin. A task starts at its `spawn` (721b9f18, Go's `go f()`), so the two lines
    // race on the M:N engine — compare the line multiset, never the exact order.
    let spawned = format!(
        "{holder}fn body():\n    parallel:\n        spawn Holder.build(3)\n    print(\"body\")\nbody()\n"
    );
    crate::vm::assert_same_lines(&run_capture(&spawned).unwrap(), "body\n3\n");
    // …and an INSTANCE method target is untouched.
    assert_eq!(
        run_capture(
            "struct Holder:\n    v: int\n    fn make(self, old: int) -> int:\n        print(old)\n        return old\nfn body():\n    defer Holder(1).make(3)\n    print(\"body\")\nbody()\n"
        )
        .unwrap(),
        "body\n3\n"
    );
}

/// M24-5b — the MODULE-reached static heads, which need real files: a `from`-imported type
/// (`import Holder from lib` — its global slot exists and holds `Nil`, so this one used to fail LATE
/// with `type nil has no method 'build'` rather than panicking), the qualified `lib.Holder.build(3)`
/// (which used to fault with `module 'lib' has no member 'Holder'`), and the qualified turbofish.
/// All three now emit exactly the `Op::CallStatic` their eager spelling emits.
#[test]
fn module_reached_static_heads_are_defer_targets_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_m245b_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "struct Holder:\n    v: int\n    fn build(old: int) -> int:\n        print(old)\n        return old\nstruct Gen[K]:\n    k: K\n    fn build(old: int) -> int:\n        print(old)\n        return old\n",
    )
    .unwrap();
    let entries = [
        "import Holder from lib\nfn body():\n    defer Holder.build(3)\n    print(\"body\")\nbody()\n",
        "import lib\nfn body():\n    defer lib.Holder.build(3)\n    print(\"body\")\nbody()\n",
        "import lib\nfn body():\n    defer lib.Gen[int].build(3)\n    print(\"body\")\nbody()\n",
    ];
    for (i, src) in entries.iter().enumerate() {
        let entry = dir.join(format!("main{i}.chz"));
        std::fs::write(&entry, src).unwrap();
        let (vm_out, _e, vm_res, _) = run_file(&entry);
        let (_par_out, _pe, par_res, _) =
            run_file_with(&entry, crate::native::HostConfig::default());
        assert!(vm_res.is_ok(), "VM faulted on {src}: {vm_res:?}");
        assert!(par_res.is_ok(), "M:N faulted on {src}: {par_res:?}");
        assert_eq!(vm_out, "body\n3\n", "VM output on: {src}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// M24-5 — the CROSS-MODULE `defer` target. `defer lib.reset(c)` lowers as `Op::DeferMethod` on the
/// module object, so the hidden witness rides on the member call's widened `argc` exactly as the
/// eager `lib.reset(c)` rides on `Op::CallMethod`'s. The checker half is
/// `checker::tests::witness_fn_is_a_cross_module_defer_target_ok`; this is the RUNNING half — that
/// the key threaded across the module boundary constructs the CALLER's type,. Two
/// concrete types, so a wrong witness cannot pass by coincidence. (`spawn lib.f(..)` stays refused
/// by the unrelated non-sendable-module-receiver rule.)
#[test]
fn cross_module_witness_defer_target_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_m245_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nstruct Tag:\n    s: str\n    fn default() -> Tag:\n        return Tag(\"none\")\nfn reset[T: Default](old: T) -> T:\n    print(T.default())\n    return T.default()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nfn body():\n    defer lib.reset(lib.Counter(1))\n    defer lib.reset(lib.Tag(\"x\"))\n    print(\"body\")\nbody()\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    // body first (Go's ordering), then LIFO — each deferral constructing ITS OWN caller-side type
    assert_eq!(vm_out, "body\nTag(s='none')\nCounter(n=7)\n");
}

/// Proves `CHEZZI_THREADS` actually reaches [`worker_count`] in THIS test process, rather than being
/// silently ignored — the second-schedule differential gate (`docs/contributing/bug-discovery.md` Tier 2,
/// `CHEZZI_THREADS=2 cargo test`) is worthless if the knob does nothing: a dropped env read would
/// make the "second" run byte-identical to the default one and nobody would notice. Locked against
/// `over_memory_trips_on_an_all_native_task_body` (the one test that transiently forces a count) so
/// this can't read its critical section and flake.
///
/// - default `cargo test` (`CHEZZI_THREADS` unset): `worker_count()` must equal the auto-sized
///   `available_parallelism()` — not some override leaked from another test.
/// - `CHEZZI_THREADS=N cargo test`: `worker_count()` must be exactly `N`. If the startup hook were
///   ever skipped, or read the var too late, this would see the host's core count instead and fail —
///   that failure IS the proof the wiring works, not a duplicate of it.
#[test]
fn chezzi_threads_env_reaches_worker_count() {
    let _guard = super::TEST_WORKER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let want = match std::env::var("CHEZZI_THREADS") {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<usize>()
            .expect("CHEZZI_THREADS set to a non-integer — the outer env is broken, not this test"),
        _ => 0,
    };
    let got = super::worker_count();
    if want == 0 {
        let auto = std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(1)
            .max(1);
        assert_eq!(
            got, auto,
            "CHEZZI_THREADS is unset but worker_count() != available_parallelism() — an override \
             leaked from another test"
        );
    } else {
        assert_eq!(
            got, want,
            "CHEZZI_THREADS='{want}' did not reach worker_count() — the test-startup hook \
             (vm::test_baseline_worker_count) is not wired"
        );
    }
}

#[test]
fn cmp_int_f64_is_exact_at_the_i64_and_2_53_boundaries() {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let cases: &[(i64, f64, Option<std::cmp::Ordering>)] = &[
        (9007199254740993, 9007199254740992.0, Some(Greater)),
        (9223372036854775807, 9223372036854775808.0, Some(Less)),
        (i64::MIN, -9223372036854775808.0, Some(Equal)),
        (i64::MIN, -9223372036854777856.0, Some(Greater)),
        (5, f64::INFINITY, Some(Less)),
        (5, f64::NEG_INFINITY, Some(Greater)),
        (5, f64::NAN, None),
        (3, 3.0, Some(Equal)),
        (0, -0.0, Some(Equal)),
        (3, 3.5, Some(Less)),
        (-4, -3.5, Some(Less)),
    ];
    for (v, w, expected) in cases.iter().copied() {
        assert_eq!(
            super::arith::cmp_int_f64(v, w),
            expected,
            "cmp_int_f64({v}, {w})"
        );
    }
}

/// TICKET-016 / W8-3 arm 2 — `update` nested inside `update` on the SAME `Shared` box must FAULT
/// (deadlock: self-held guard) instead of hanging undetected. Planning's decision: a same-box
/// re-entry is the length-1 wait-for cycle, so it faults; only a genuine hang would time out.
#[test]
fn ticket_016_shared_update_nested_in_update_faults() {
    let src = "import Shared from std.concurrency\n\
s := Shared(10)
fn bump(v: int) -> int:
    s.update(fn(w: int) -> int: w + 1)
    return v + 1
s.update(bump)
print(s.get())
";
    let entry = write_temp_chz("ticket016_update_in_update", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(5));
    let _ = std::fs::remove_file(&entry);
    let (_out, _err, res, _code) =
        result.expect("expected update-in-update to return within 5s (fault), not hang");
    assert!(
        res.is_err(),
        "expected `update` nested in `update` on the same Shared box to fault (W8-3), got {res:?}"
    );
}

/// TICKET-016 / W8-3 arm 1 — a `set` nested inside `update` on the SAME `Shared` box must FAULT
/// (deadlock: self-held guard) instead of being silently overwritten by `update`'s post-closure
/// write-back.
#[test]
fn ticket_016_shared_update_nested_set_silently_lost() {
    let src = "import Shared from std.concurrency\n\
s := Shared(10)
fn bump(v: int) -> int:
    s.set(99)
    return v + 1
s.update(bump)
print(s.get())
";
    let entry = write_temp_chz("ticket016_update_nested_set", src);
    let (_out, _err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    let e = res.expect_err("expected a fault, got ok");
    assert!(
        e.message.contains("already holds the update guard"),
        "expected the fault message to name the self-held guard, got: {}",
        e.message
    );
}

/// TICKET-016 / W8-3 — a cross-task `set` racing an in-flight `update` on a DIFFERENT task must
/// BLOCK behind the guard and land, not be silently lost (Go's mutex loses nothing).
#[test]
fn ticket_016_cross_task_set_racing_update_is_not_lost() {
    let src = "\
import Shared from std.concurrency
import std.time
s := Shared(0)
fn slow(x: int) -> int:
    time.sleep_ms(300)
    return x + 1
parallel:
    spawn: s.update(slow)
    spawn:
        time.sleep_ms(100)
        s.set(100)
print(\"s = {s.get()}\")
";
    let entry = write_temp_chz("ticket016_cross_task_set_race", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "s = 100\n",
        "expected the cross-task set to block and land: {out:?}"
    );
}

/// TICKET-016 / W8-3 — an AB-BA wait-for CYCLE across two `Shared` boxes must fault, not hang
/// undetected. A 200ms sleep before each nested `update` forces the cycle to form.
#[test]
fn ticket_016_cross_box_update_cycle_faults() {
    let src = "\
import Shared from std.concurrency
import std.time
a := Shared(1)
b := Shared(2)
fn bumpb(x: int) -> int:
    time.sleep_ms(200)
    b.update(fn(y: int) -> int: y + 1)
    return x + 1
fn bumpa(y: int) -> int:
    time.sleep_ms(200)
    a.update(fn(x: int) -> int: x + 1)
    return y + 1
parallel:
    spawn: a.update(bumpb)
    spawn: b.update(bumpa)
print(\"done\")
";
    let entry = write_temp_chz("ticket016_cross_box_cycle", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(15));
    let _ = std::fs::remove_file(&entry);
    let (_out, _err, res, _code) =
        result.expect("expected the AB-BA cycle to fault within 15s, not hang");
    assert!(
        res.is_err(),
        "expected an AB-BA wait-for cycle to fault, got {res:?}"
    );
}

/// TICKET-063 (W10-15) — a `Shared` update guard waiter parked behind a sibling's blocking `recv`
/// must fault `deadlock`, not hang forever. The waiter holds no scheduler of its own progress
/// promise (the recv it feeds never arrives), and `s.update`'s guard wait used to be accounted
/// `inflight` — which vetoes the deadlock verdict unconditionally — so this hung before the fix.
#[test]
fn ticket_063_guard_waiter_behind_a_recv_faults_deadlock() {
    let src = "\
import Shared from std.concurrency
s := Shared(0)
ch := Channel[int]()
parallel:
    spawn:
        s.update(fn(x: int) -> int: x + ch.recv())
    spawn:
        s.set(5)
print(\"never\")
";
    let entry = write_temp_chz("ticket063_guard_waiter_behind_recv", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(15));
    let _ = std::fs::remove_file(&entry);
    let (_out, _err, res, _code) =
        result.expect("expected the guard waiter to fault within 15s, not hang");
    let msg = res
        .as_ref()
        .err()
        .map(|e| e.message.clone())
        .unwrap_or_default();
    assert!(msg.contains("deadlock"), "expected \"deadlock\" in {msg:?}");
}

/// TICKET-063 — the same shape as `ticket_063_guard_waiter_behind_a_recv_faults_deadlock`, but the
/// WAITER is the inline nursery body itself rather than a spawned sibling. Without
/// `Vm::block_party_guard` marking the body blocked, `SchedCore::any_body_injecting` vetoes the
/// deadlock predicate forever for this shape.
#[test]
fn ticket_063_guard_waiter_in_the_nursery_body_faults_deadlock() {
    let src = "\
import Shared from std.concurrency
s := Shared(0)
ch := Channel[int]()
parallel:
    spawn:
        s.update(fn(x: int) -> int: x + ch.recv())
    s.set(5)
print(\"never\")
";
    let entry = write_temp_chz("ticket063_guard_waiter_in_body", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(15));
    let _ = std::fs::remove_file(&entry);
    let (_out, _err, res, _code) =
        result.expect("expected the guard waiter to fault within 15s, not hang");
    assert!(res.is_err(), "expected a deadlock fault, got {res:?}");
}

/// TICKET-063 — the no-false-fault control: a guard waiter that COULD be fed by a live sibling must
/// not be faulted. `time.sleep_ms(100)` orders spawn 2's `s.set(5)` behind spawn 1's guard acquire
/// (copying `ticket_016_cross_task_set_racing_update_is_not_lost`'s race-forcing pattern), and
/// spawn 3's 150ms sleep keeps the 100ms sleep comfortably under it, so the holder receives `7`
/// from spawn 3, stores it, releases the guard, and `s.set(5)` lands last — the only reachable
/// output is `s = 5`. `ticket_016_cross_task_set_racing_update_is_not_lost` is the second control: a
/// guard waiter behind a 300ms sleeping holder must still print `s = 100`.
#[test]
fn ticket_063_guard_waiter_is_not_faulted_while_a_sibling_can_feed_the_holder() {
    let src = "\
import Shared from std.concurrency
import std.time
s := Shared(0)
ch := Channel[int]()
parallel:
    spawn:
        s.update(fn(x: int) -> int: x + ch.recv())
    spawn:
        time.sleep_ms(100)
        s.set(5)
    spawn:
        time.sleep_ms(150)
        ch.send(7)
print(\"s = {s.get()}\")
";
    let entry = write_temp_chz("ticket063_guard_waiter_fed_by_sibling", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "s = 5\n",
        "expected the holder to settle at 5: {out:?}"
    );
}

/// TICKET-016 / W8-3 — the no-false-fault control: two tasks that each touch a DISJOINT pair of
/// boxes, sharing NO box between them, must complete normally, not be reported as a deadlock. The
/// two tasks must not nest into each other's box — that IS a cycle and is covered by
/// `ticket_016_cross_box_update_cycle_faults`.
#[test]
fn ticket_016_cross_box_update_without_cycle_still_runs() {
    let src = "\
import Shared from std.concurrency
a := Shared(1)
b := Shared(2)
c := Shared(3)
d := Shared(4)
fn bumpb(x: int) -> int:
    b.update(fn(y: int) -> int: y + 1)
    return x + 1
fn bumpd(x: int) -> int:
    d.update(fn(y: int) -> int: y + 1)
    return x + 1
parallel:
    spawn: a.update(bumpb)
    spawn: c.update(bumpd)
print(\"done\")
";
    let entry = write_temp_chz("ticket016_cross_box_no_cycle", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "done\n",
        "expected the no-cycle case to complete: {out:?}"
    );
}

/// TICKET-016 / W8-3, reversed by TICKET-246 (owner answer Q2 (a), plan step 9; the name is kept
/// per DEC-016). The state lives in the box, so no pre-update copy exists: `get` inside the box's
/// own `update` takes the update guard and faults instead of self-deadlocking. The fault is
/// catchable, the update did not happen, and the box keeps serving.
#[test]
fn ticket_016_get_inside_update_still_reads_pre_guard_value() {
    let src = "\
import Shared from std.concurrency
s := Shared(10)
fn peek(v: int) -> int:
    print(s.get())
    return v + 1
r := recover: s.update(peek)
match r:
    ?_: print(\"no fault\")
    !e: print(e.message().contains(\"already holds the update guard\"))
print(s.get())
";
    let entry = write_temp_chz("ticket016_get_inside_update", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "true\n10\n",
        "expected the guard fault then 10: {out:?}"
    );
}

/// TICKET-246 — the `RwShared` twin of the test above: `get` (and `read`) inside the box's own
/// `write` closure fault with the guard text. `read` nested in `read` and `write` nested in
/// `read` stay legal, because the reader closure runs after its lock dropped.
#[test]
fn ticket_246_rwshared_get_inside_its_own_write_faults() {
    let src = "\
import RwShared from std.concurrency
s := RwShared(10)
fn peek(v: int) -> int:
    print(s.get())
    return v + 1
fn peek_read(v: int) -> int:
    return v + s.read(fn(x): x)
fn bump(v: int) -> int:
    s.write(fn(x): x + v)
    return v
for f in [peek, peek_read]:
    r := recover: s.write(f)
    match r:
        ?_: print(\"no fault\")
        !e: print(e.message().contains(\"already holds the update guard\"))
print(s.get())
print(s.read(fn(x): x + s.get()))
print(s.read(bump), s.get())
";
    let entry = write_temp_chz("ticket246_rwshared_get_inside_write", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(out, "true\ntrue\n10\n20\n10 20\n", "got: {out:?}");
}

/// TICKET-016 / W8-3 — `write` nested in `read` is the one legal `RwShared` crossing and must still
/// persist.
#[test]
fn ticket_016_rwshared_write_inside_read_still_persists() {
    let src = "\
import RwShared from std.concurrency
r := RwShared(5)
fn peek(v: int) -> int:
    r.write(fn(y: int) -> int: y + 100)
    return v
print(r.read(peek))
print(r.read(fn(v: int) -> int: v))
";
    let entry = write_temp_chz("ticket016_rwshared_write_in_read", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(out, "5\n105\n", "expected 5 then 105: {out:?}");
}

/// TICKET-016 / W8-25 — the module-global closure crossing INTO a `spawn:` (the `Lowered::Closure`
/// wire form, not `WireValue`) must carry the airlock snapshot.
#[test]
fn ticket_016_closure_over_module_global_crossing_into_spawn() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency
n := 1
f := fn(x: int) -> int: x * n
n = 100
parallel:
    spawn: print(f(3))
";
    let entry = write_temp_chz("ticket016_global_into_spawn", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(out, "300\n", "expected 300: {out:?}");
}

/// TICKET-016 / W8-25 review finding — a spawned task's OWN write to a module global must stay
/// visible to a module-level closure it calls, when that closure never crosses a channel or spawn
/// boundary at all (it existed before the `parallel:` block and is called from inside the same
/// task that wrote the global). The per-task module snapshot (`ensure_snapshot`/`fault_module`)
/// replicates every module global — including `f` — into the task's own module copy; `f` must keep
/// doing a live `Op::GetGlobalSlot` read against that copy, not a value installed frozen at the
/// snapshot's pre-write state.
#[test]
fn ticket_016_spawned_task_own_write_visible_to_module_closure_it_calls() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency
n := 1
f := fn(x: int) -> int: x * n
parallel:
    spawn:
        recover:
            n = 100
        print(f(3))
";
    let entry = write_temp_chz("ticket016_spawn_own_write_visible", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    // TICKET-169 (D4 layer C): `n = 100` above now faults, so `f` reads the receiver's unwritten
    // `n` (`1`).
    assert_eq!(
        out, "3\n",
        "expected f to read the receiver's unwritten n (3), got {out:?}"
    );
}

/// TICKET-016 / W8-25 — a crossed closure that READS AND WRITES the same module global reads its
/// own late load of the RUNNING task's copy, so its own write is always visible to its next read.
#[test]
fn ticket_016_closure_over_module_global_written_by_closure_stays_late() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency
n := 1
ch := Channel[fn(int) -> int]()
fn outer() -> fn(int) -> int:
    fn f(x: int) -> int:
        n = n + x
        return n
    return f
parallel:
    spawn:
        recover:
            n = 100
        ch.send(outer())
g := ch.recv()
print(g(3))
";
    let entry = write_temp_chz("ticket016_global_read_write", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "4\n",
        "expected 4 (late load, not the snapshot 103): {out:?}"
    );
}

/// TICKET-016 / W8-25, reversed by owner decision D2 (TICKET-137) — a closure NESTED inside a crossed
/// closure reads the module globals of the task that RUNS it, exactly like the crossed closure does:
/// the sender's `n = 100` lands in the sender's copy, so `g(3)` prints `3`. Go/CPython print `300`
/// (one live global); the divergence is the deliberate per-task rule — share with `Shared`/`Channel`.
#[test]
fn ticket_016_closure_over_module_global_read_by_nested_closure() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency
n := 1
ch := Channel[fn(int) -> int]()
parallel:
    spawn:
        recover:
            n = 100
        ch.send(fn(x: int) -> int: x * (fn() -> int: n)())
g := ch.recv()
print(g(3))
";
    let entry = write_temp_chz("ticket016_global_nested_closure", src);
    let (out, err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?} err={err}");
    assert_eq!(
        out, "3\n",
        "a nested closure must read the receiving task's own copy of `n` (D2): {out:?}"
    );
}

/// TICKET-016 / W8-25, reversed by owner decision D2 (TICKET-137) — a closure's reference to a
/// MODULE GLOBAL is a late global load, not a capture, so it is neither moved nor copied at the
/// `parallel:`/`spawn:` airlock: it reads the module globals of the task that RUNS it. At module
/// scope the closure crossed through the `Channel` sees the RECEIVER's un-mutated global (`n=1`),
/// not the sender's write (`n=100`), so `f(3)` prints `3`. This is deliberate (each task owns a copy
/// of every module global); Go/CPython print `300` (one live global). The identical body inside
/// `fn main()` (over a LOCAL, which the airlock does copy) prints `300` — see
/// `ticket_016_closure_over_local_in_fn_crosses_airlock_correctly` for the contrast.
#[test]
fn ticket_016_closure_over_module_global_loses_value_at_airlock() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency

n := 1
ch := Channel[fn(int) -> int]()
parallel:
    spawn:
        recover:
            n = 100
        ch.send(fn(x: int) -> int: x * n)
f := ch.recv()
print(f(3))
";
    let entry = write_temp_chz("ticket016_global_capture", src);
    let (out, _err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?}");
    assert_eq!(
        out, "3\n",
        "a closure's module-global reference must read the receiving task's own copy (D2, `3`; \
         Go/CPython print 300), got {out:?}"
    );
}

/// TICKET-016 / W8-25 contrast — the identical shape over a captured LOCAL (not a module global)
/// already crosses the airlock correctly: the airlock copies a captured local by value, so the
/// closure's `n` is fixed at capture/copy time and the receiver sees the sender's write.
#[test]
fn ticket_016_closure_over_local_in_fn_crosses_airlock_correctly() {
    let src = "\
import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency

fn main():
    n := 1
    ch := Channel[fn(int) -> int]()
    parallel:
        spawn:
            recover:
                n = 100
            ch.send(fn(x: int) -> int: x * n)
    f := ch.recv()
    print(f(3))

main()
";
    let entry = write_temp_chz("ticket016_local_capture", src);
    let (out, _err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?}");
    // TICKET-169 (D4 layer C): the captured local `n` crossed the airlock and is marked, so
    // `n = 100` now faults instead of sharing it.
    assert_eq!(out, "3\n", "captured-local write now faults: {out:?}");
}

/// TICKET-016 — a bounded acquire returns `Ok(None)` on timeout instead of blocking, still detects
/// self-held re-entry, and hands the guard over once it is free.
#[test]
fn ticket_016_bounded_update_guard_acquire_yields_instead_of_blocking() {
    let key = usize::MAX - 9001;
    let budget = Some(std::time::Duration::from_millis(5));
    let held = acquire_update_guard_within(key, u64::MAX - 1, budget)
        .expect("a free box must not report a cycle")
        .expect("a free box must hand the guard over");
    assert!(matches!(
        acquire_update_guard_within(key, u64::MAX - 1, budget),
        Err(GuardCycle::SelfHeld)
    ));
    let t0 = std::time::Instant::now();
    assert!(matches!(
        acquire_update_guard_within(key, u64::MAX - 2, budget),
        Ok(None)
    ));
    assert!(t0.elapsed() < std::time::Duration::from_millis(500));
    drop(held);
    assert!(matches!(
        acquire_update_guard_within(key, u64::MAX - 2, budget),
        Ok(Some(_))
    ));
}

/// TICKET-063 — `guard_wait_satisfiable` answers the same three outcomes `acquire_update_guard_within`
/// would, without acquiring.
#[test]
fn ticket_063_guard_wait_satisfiable_answers_the_acquire_outcome() {
    let key = usize::MAX - 9002;
    let holder = u64::MAX - 11;
    let other = u64::MAX - 12;
    assert!(
        crate::vm::core::guard_wait_satisfiable(key, holder),
        "an unowned key must be satisfiable"
    );
    let held = acquire_update_guard_within(key, holder, Some(std::time::Duration::from_millis(5)))
        .expect("a free box must not report a cycle")
        .expect("a free box must hand the guard over");
    assert!(
        crate::vm::core::guard_wait_satisfiable(key, holder),
        "the holder itself must be satisfiable"
    );
    assert!(
        !crate::vm::core::guard_wait_satisfiable(key, other),
        "another task must not be satisfiable while the holder holds it"
    );
    drop(held);
    assert!(
        crate::vm::core::guard_wait_satisfiable(key, other),
        "once the held guard is dropped the other task must be satisfiable again"
    );
}

/// TICKET-193 — `await_update_guard_free` reports a free guard without taking it, times out on a
/// held one, and still faults a self-held re-entry.
#[test]
fn ticket_193_await_update_guard_free_never_takes_the_guard() {
    use crate::vm::core::await_update_guard_free;
    let key = usize::MAX - 9003;
    let (waiter, holder) = (u64::MAX - 13, u64::MAX - 14);
    let budget = Some(std::time::Duration::from_millis(5));
    assert_eq!(await_update_guard_free(key, waiter, budget), Ok(true));
    let held = acquire_update_guard_within(key, holder, Some(std::time::Duration::ZERO))
        .expect("a free box must not report a cycle")
        .expect("await must not have taken the guard");
    assert_eq!(await_update_guard_free(key, waiter, budget), Ok(false));
    assert_eq!(
        await_update_guard_free(key, holder, budget),
        Err(GuardCycle::SelfHeld)
    );
    drop(held);
    assert_eq!(await_update_guard_free(key, waiter, budget), Ok(true));
}

/// W8-22: an error with no recorded span answers `None`, never a guess. A user-constructed
/// `Err("boom")` was never stamped by `recover:`, and a struct error's only requirement is
/// `message()` — neither carries an origin.
#[test]
fn caught_error_location_is_nil_without_a_recorded_span() {
    // A user-constructed `Err(...)` returned as a plain value (no fault, so no `recover:`
    // boundary ever runs) was never stamped.
    let src = "fn a() -> int!:\n    return !\"boom\"\nfn main():\n    match a():\n        ?v: print(v)\n        !e:\n            print(e.line())\n            print(e.col())\n            print(e.file())\nmain()\n";
    assert_eq!(run(src), "None\nNone\nNone\n");

    let src_struct = "struct MyErr:\n    code: int\n    fn message(self) -> str:\n        return \"code {self.code}\"\nfn b() -> int!:\n    return !MyErr(7)\nfn main():\n    match b():\n        ?v: print(v)\n        !e: print(e.line())\nmain()\n";
    assert_eq!(run(src_struct), "None\n");
}

/// W8-22: a fault caught by `recover:` carries the ORIGIN of the fault, not the `recover:` site.
/// The uncaught form of this source faults at line 3 col 15 (`xs[9]`).
#[test]
fn caught_error_carries_the_fault_origin_span() {
    let src = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e:\n            print(e.line())\n            print(e.col())\nmain()\n";
    assert_eq!(run(src), "3\n13\n");
}

/// TICKET-126 (W13-24) — TICKET-118 added `SchedCore::cancelled_scope_awaiting_drain` to the idle
/// path of `take_runnable`, called on every idle pass that reaches it regardless of whether any
/// cancel has EVER been tripped. A channel ping-pong with no cancel at all still pays that scan on
/// every idle cycle of every idle worker, which is the measured 17.9% regression at the default
/// worker count. A cancel-generation guard (checked before the scan) should make the scan count stay
/// small and roughly proportional to the number of `send`/`recv` round trips, not to the number of
/// idle passes forced by a larger worker pool.
///
/// Forces `worker_count(8)` so idle workers vastly outnumber the two real tasks, magnifying the idle
/// scan cost the same way the filed measurement's 28-core default pool did.
#[test]
fn ticket126_idle_cancel_scan_is_not_paid_when_no_cancel_ever_trips() {
    #[rustfmt::skip]
    let in_parent = crate::vm::rerun_in_child(module_path!(), "ticket126_idle_cancel_scan_is_not_paid_when_no_cancel_ever_trips");
    if in_parent {
        return;
    }
    struct Workers(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Workers {
        fn drop(&mut self) {
            crate::vm::set_worker_count(crate::vm::test_baseline_worker_count());
        }
    }
    let _workers = Workers(
        crate::vm::TEST_WORKER_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    crate::vm::set_worker_count(8);

    crate::vm::CANCEL_SCAN_CALLS.store(0, Ordering::Relaxed);
    let src = "\
fn pp():
    ping := Channel[int](0)
    pong := Channel[int](0)
    parallel:
        spawn:
            i := 0
            while i < 2000:
                ping.send(i)
                n := pong.recv()
                i = i + 1
        spawn:
            i := 0
            while i < 2000:
                v := ping.recv()
                pong.send(v)
                i = i + 1
fn main():
    pp()
    print(\"done\")
main()
";
    assert_eq!(run(src), "done\n");
    let scans = crate::vm::CANCEL_SCAN_CALLS.load(Ordering::Relaxed);
    assert!(
        scans < 4000,
        "idle cancel-drain scan ran {scans} times for 2000 round trips with no cancel ever \
         tripped — it must be gated by a cancel-generation check, not re-run on every idle pass"
    );
}

/// TICKET-126 (W13-24) — the idle drain scan is skipped only while no cancel was tripped since the
/// last scan that found nothing: a trip after a clean scan must re-arm it, and a scan that found a
/// scope must not record the generation (else the next pass skips a family still owed its drain).
#[test]
fn mnsched_drain_scan_reruns_after_a_cancel_generation_bump() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err(), 0);
    let ancestor = Arc::new(AtomicBool::new(false));
    sched.lock().scopes[0].ancestors.push(Arc::clone(&ancestor));
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    sched.park(core_key(&core), Arc::clone(&core), f);
    assert_eq!(
        sched.lock().drain_scan_due(&sched),
        None,
        "nothing tripped: no scope owes a drain"
    );
    crate::vm::trip_cancel_flag(&ancestor);
    assert_eq!(
        sched.lock().drain_scan_due(&sched),
        Some(0),
        "a trip after a clean scan must re-arm the scan"
    );
    assert_eq!(
        sched.lock().drain_scan_due(&sched),
        Some(0),
        "a scan that found a scope must not record the generation"
    );
}

/// TICKET-126 (W13-24) — every production cancel-flag store goes through `trip_cancel_flag`, which
/// bumps `CANCEL_GEN`. A bare store is invisible to the gated drain scan of `take_runnable` and
/// brings back W13-8's false deadlock for a parked fiber whose ancestor was cancelled.
#[test]
fn ticket126_every_cancel_store_goes_through_trip_cancel_flag() {
    for (name, src) in vm_sources() {
        let flat: String = src.chars().filter(|c| !c.is_whitespace()).collect();
        for bypass in ["cancel.store(true", "c.store(true"] {
            assert_eq!(
                flat.matches(bypass).count(),
                0,
                "src/vm/{name}: a bare {bypass} bypasses trip_cancel_flag"
            );
        }
    }
}

/// TICKET-126 (W13-24) — the `CANCEL_GEN` bump is `Release` and the gate's read is `Acquire`. With
/// both sides `Relaxed` the pair establishes no happens-before, so `drain_scan_due` can see the
/// bumped generation, read the flag it was bumped for as still `false`, record that generation, and
/// leave a fiber parked under a cancelled ancestor undrained — W13-8's false deadlock. No mutex
/// orders the pair for us: `Executor.cancel()` stores an ancestor flag under no core lock and
/// `scope_cancel_tripped` reads it under one. A source-text rule, because no test can observe a
/// memory ordering.
#[test]
fn ticket126_cancel_gen_bump_is_release_and_the_gate_read_is_acquire() {
    let flat: String = vm_sources()
        .iter()
        .flat_map(|(_, text)| text.chars())
        .filter(|c| !c.is_whitespace())
        .collect();
    for needed in [
        "CANCEL_GEN.fetch_add(1,Ordering::Release)",
        "CANCEL_GEN.load(Ordering::Acquire)",
    ] {
        assert!(
            flat.contains(needed),
            "src/vm: {needed} is missing; that release/acquire pair is what orders a cancel-flag store before the gated drain scan"
        );
    }
    for weak in [
        "CANCEL_GEN.fetch_add(1,Ordering::Relaxed)",
        "CANCEL_GEN.load(Ordering::Relaxed)",
    ] {
        assert!(
            !flat.contains(weak),
            "src/vm: {weak} loses a trip; a scanner can see the bumped generation while the flag it was bumped for still reads false (W13-8)"
        );
    }
}

/// TICKET-185 (Family A, wave 17): a cap-0 `send` must return only once a receiver has TAKEN the
/// value. A sender that `recv`s right after its `send` must never take its own value back.
/// Go prints `got 1` / `back 2`; Chezzi printed `back 1` then a false `deadlock`.
#[test]
fn unbuffered_send_never_returns_its_own_value_to_the_sender() {
    let src = r#"fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            print("got", ch.recv())
            ch.send(2)
        spawn:
            spin := 0
            for i in 0..2000000:
                spin += i
            ch.send(1)
            print("back", ch.recv())
main()
"#;
    for _ in 0..5 {
        let out = run_capture(src);
        let lines: Vec<String> = match &out {
            Ok(s) => s.lines().map(str::to_string).collect(),
            Err(e) => vec![format!("fault: {}", e.message)],
        };
        assert!(
            out.is_ok()
                && lines.contains(&"got 1".to_string())
                && lines.contains(&"back 2".to_string()),
            "sender took its own value back: {out:?}"
        );
    }
}

/// TICKET-192: `RwShared[Map].get_key` scans the flat `WireValue::Map` entry vector, so one lookup is
/// O(n) and n lookups on an n-entry shared map are O(n^2). Go `sync.Map` / a CPython `dict` behind a
/// `Lock` are O(1) per lookup. Pinned with a COUNT of the wire nodes rebuilt (TICKET-233): a key and
/// a value per lookup, against every stored entry per lookup for a scan.
///
/// TICKET-246: the box decodes its initial map ONCE into its own heap (2000 keys + 2000 values),
/// so the count is that plus two per lookup: 8001. A scan would be 4 000 000.
#[test]
fn rwshared_map_get_key_is_not_linear() {
    let src = "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    n := 2000\n    m: Map[int, int] = {}\n    i := 0\n    while i < n:\n        m[i] = i\n        i = i + 1\n    box := ConcurrentMap(concurrency.RwShared(m))\n    s := 0\n    i = 0\n    while i < n:\n        match box.get(i):\n            ?v: s = s + 1\n            None: s = s\n        i = i + 1\n    print(s)\nmain()\n";
    let (out, touches) = super::golden_tests::golden_entry_counting(src, &WIRE_DECODE_NODES);
    assert_eq!(out, "2000\n");
    assert!(
        (4000..=8100).contains(&touches),
        "2000 ConcurrentMap.get on a 2000-entry map rebuilt {touches} wire nodes (want 4000..=8100) -- O(n) per lookup"
    );
}

/// TICKET-192: `RwShared[Map].set_key` writes one entry in place instead of re-encoding the whole map
/// (what `write` does). 2000 inserts then 2000 overwrites on one box, pinned with the same count.
///
/// TICKET-246: a writer decodes its key and its value into the box heap and touches nothing else,
/// so 4000 writes rebuild 8000 nodes (plus the final `get`). A re-encode would be millions.
///
/// TICKET-278: `insert` returns the replaced `V?`, so each overwrite also decodes its `?old` result:
/// about one more node per overwrite: 10003 in all against 8001 before. Still O(1) per write.
#[test]
fn rwshared_map_set_key_is_not_linear() {
    let src = "import std.concurrency\nimport Atomic, Executor, RwShared, Shared from std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    n := 2000\n    m: Map[int, int] = {}\n    box := ConcurrentMap(concurrency.RwShared(m))\n    i := 0\n    while i < n:\n        _ := box.insert(i, i)\n        i = i + 1\n    i = 0\n    while i < n:\n        _ := box.insert(i, i + 1)\n        i = i + 1\n    print(box.len())\n    match box.get(1999):\n        ?v: print(v)\n        None: print(-1)\nmain()\n";
    let (out, touches) = super::golden_tests::golden_entry_counting(src, &WIRE_DECODE_NODES);
    assert_eq!(out, "2000\n2000\n");
    assert!(
        (4000..=10100).contains(&touches),
        "2000 ConcurrentMap.insert inserts + 2000 overwrites rebuilt {touches} wire nodes (want 4000..=10100) -- O(n) per write"
    );
}

/// TICKET-194 X1: an op that does not wait is not a cancellation point. A job that already returned
/// 7 must deliver it when `ex.cancel()` lands after the job returned (TICKET-289: a Task outcome
/// settles once, so a later cancel changes nothing). Go and CPython keep a finished job's result.
#[test]
fn returned_job_keeps_its_result_across_cancel() {
    let src = r#"
import Executor from std.concurrency
lost := 0
for _ in 0..20:
    ex := Executor()
    gate := Channel[int](0)
    fn job() -> int:
        v := gate.recv()
        return v
    t := ex.submit(job)
    gate.send(7)
    match t.get():
        ?_: pass
        !_: lost += 1
    ex.cancel()
    match t.get():
        ?v: lost += v - 7
        !_: lost += 1
print("lost={lost}")
"#;
    let out = pmap_both("t194_x1", src);
    assert_eq!(out, "lost=0\n", "a returned job's result was cut by cancel");
}

/// TICKET-195 C2: a child's panic must be reported when the owner's `defer: ch.recv()` can never
/// complete; today the owner's stuck defer reports `deadlock` and `boom` is lost.
#[test]
fn child_fault_survives_owner_stuck_defer() {
    let src = r#"import std.time
fn viadefer(ch: Channel[int]):
    defer:
        print(ch.recv())
    time.sleep_ms(1000)

fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            time.sleep_ms(50)
            panic("boom")
        viadefer(ch)

main()
"#;
    let entry = write_temp_chz("t195_c2", src);
    let (_out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("expected a fault").message;
    assert!(msg.contains("boom"), "child fault lost, got: {msg}");
}
