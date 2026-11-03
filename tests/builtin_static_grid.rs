//! A builtin type builds a value from an iterable through a static method its `native struct` decl
//! in `std/prelude.chz` declares (`List.from_iter(xs)`, Rust `FromIterator::from_iter`), and a call
//! on the type's name only makes an empty value (`List[int]()`, owner rule: a constructor sets fields
//! only). `bytes(n)` / `bytearray(n)` are `bytes.zeroed(n)` / `bytearray.zeroed(n)`.
//!
//! Axes: type (List, Set, Map, bytes, bytearray) × source (list literal, `Range`, generator,
//! `m.items()`, a Map, a str, bytes, bytearray, an empty literal) × slot (untyped, typed annotation,
//! turbofish `T[..].from_iter`) × form (new: accept, old `T(x)`: reject with a hint). Each accept
//! cell's expected output is the old form's output on the base build (the twin oracle), which is
//! also Python's `list(it)` / `set(it)` / `dict(it)` / `bytes(it)` value. Extra cells: `zeroed`, a
//! missing static, the static as a value, a static called on a value, the shadows, the element
//! rejects.

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, cmd: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-bstatic-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args([cmd, "main.chz"])
        .current_dir(&dir)
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The sources every accept cell can read.
const PRELUDE: &str = "\
import Iterator from std.iter
fn g() -> Iterator[int]:
    yield 1
    yield 2
fn gb() -> Iterator[int]:
    yield 104
    yield 105
fn gp() -> Iterator[(str, int)]:
    yield (\"a\", 1)
m := {\"a\": 1, \"b\": 2}
ba := bytearray()
ba.push(104)
ba.push(105)
";

/// (expression, expected display). Each expression is printed in its own statement.
const ACCEPT: &[(&str, &str)] = &[
    // List: every source, untyped.
    ("List.from_iter([1, 2])", "[1, 2]"),
    ("List.from_iter(Range(0, 3))", "[0, 1, 2]"),
    ("List.from_iter(g())", "[1, 2]"),
    ("List.from_iter(m.items())", "[('a', 1), ('b', 2)]"),
    ("List.from_iter(m)", "[('a', 1), ('b', 2)]"),
    ("List.from_iter(\"hi\")", "['h', 'i']"),
    ("List.from_iter(b\"hi\")", "[104, 105]"),
    ("List.from_iter(ba)", "[104, 105]"),
    // List: turbofish.
    ("List[int].from_iter([1, 2])", "[1, 2]"),
    ("List[int].from_iter(Range(0, 3))", "[0, 1, 2]"),
    ("List[int].from_iter(g())", "[1, 2]"),
    (
        "List[(str, int)].from_iter(m.items())",
        "[('a', 1), ('b', 2)]",
    ),
    ("List[str].from_iter(\"hi\")", "['h', 'i']"),
    ("List[int].from_iter(b\"hi\")", "[104, 105]"),
    ("List[int].from_iter([])", "[]"),
    // Set.
    ("Set.from_iter([1, 1, 2])", "{1, 2}"),
    ("Set.from_iter(Range(0, 3))", "{0, 1, 2}"),
    ("Set.from_iter(g())", "{1, 2}"),
    ("Set.from_iter(m.items())", "{('a', 1), ('b', 2)}"),
    ("Set.from_iter(m)", "{('a', 1), ('b', 2)}"),
    ("Set.from_iter(\"hi\")", "{'h', 'i'}"),
    ("Set.from_iter(b\"hi\")", "{104, 105}"),
    ("Set.from_iter(ba)", "{104, 105}"),
    ("List[int?].from_iter([?5])", "[5]"),
    ("Set[int].from_iter([1, 1, 2])", "{1, 2}"),
    ("Set[int].from_iter([])", "Set()"),
    // Map: every source that yields (key, value) pairs.
    ("Map.from_iter([(\"a\", 1)])", "{'a': 1}"),
    ("Map.from_iter(gp())", "{'a': 1}"),
    ("Map.from_iter(m.items())", "{'a': 1, 'b': 2}"),
    ("Map.from_iter(m)", "{'a': 1, 'b': 2}"),
    ("Map[str, int].from_iter(m.items())", "{'a': 1, 'b': 2}"),
    ("Map[str, int].from_iter([])", "{}"),
    // bytes: an iterable of ints (bytes iterate as ints, `for x in b\"hi\"` gives 104, 105).
    ("bytes.from_iter([104, 105])", "b'hi'"),
    ("bytes.from_iter(Range(104, 106))", "b'hi'"),
    ("bytes.from_iter(gb())", "b'hi'"),
    ("bytes.from_iter(b\"hi\")", "b'hi'"),
    ("bytes.from_iter(ba)", "b'hi'"),
    ("bytes.from_iter([])", "b''"),
    // bytearray.
    ("bytearray.from_iter([104, 105])", "bytearray(b'hi')"),
    ("bytearray.from_iter(Range(104, 106))", "bytearray(b'hi')"),
    ("bytearray.from_iter(gb())", "bytearray(b'hi')"),
    ("bytearray.from_iter(b\"hi\")", "bytearray(b'hi')"),
    ("bytearray.from_iter(ba)", "bytearray(b'hi')"),
    ("bytearray.from_iter([])", "bytearray(b'')"),
    // zeroed.
    ("bytes.zeroed(0)", "b''"),
    ("bytes.zeroed(3)", "b'\\x00\\x00\\x00'"),
    ("bytearray.zeroed(0)", "bytearray(b'')"),
    ("bytearray.zeroed(3)", "bytearray(b'\\x00\\x00\\x00')"),
    // The empty forms stay.
    ("List[int]()", "[]"),
    ("Set[int]()", "Set()"),
    ("Map[str, int]()", "{}"),
    ("bytearray()", "bytearray(b'')"),
];

/// (statement block, expected stdout): the typed slot, the copies, and the static as a value.
const ACCEPT_STMTS: &[(&str, &str)] = &[
    ("a: List[int] = List.from_iter([])\nprint(a)\n", "[]\n"),
    ("a: List[int] = List.from_iter(g())\nprint(a)\n", "[1, 2]\n"),
    ("a: Set[int] = Set.from_iter([])\nprint(a)\n", "Set()\n"),
    ("a: Map[str, int] = Map.from_iter([])\nprint(a)\n", "{}\n"),
    (
        "a: Map[str, int] = Map.from_iter(m)\nprint(a)\n",
        "{'a': 1, 'b': 2}\n",
    ),
    ("a: bytes = bytes.from_iter([104])\nprint(a)\n", "b'h'\n"),
    (
        "a: bytearray = bytearray.from_iter([])\nprint(a)\n",
        "bytearray(b'')\n",
    ),
    // `Map.from_iter(m)` and `bytearray.from_iter(ba)` are copies.
    (
        "c := Map.from_iter(m)\nc[\"z\"] = 9\nprint(m.len(), c.len())\n",
        "2 3\n",
    ),
    (
        "c := bytearray.from_iter(ba)\nc.push(0)\nprint(ba.len(), c.len())\n",
        "2 3\n",
    ),
    (
        "xs := [1]\nc := List.from_iter(xs)\nc.push(2)\nprint(xs, c)\n",
        "[1] [1, 2]\n",
    ),
    // The static read as a value, as a user struct's generic static is: a function type fixes it.
    (
        "f: fn(List[int]) -> List[int] = List.from_iter\nprint(f([3]))\n",
        "[3]\n",
    ),
    // The method turbofish fixes the static's own `S`, as for a user struct's `Foo.mk[List[int]]`.
    (
        "f := bytes.from_iter[List[int]]\nprint(f([104]))\n",
        "b'h'\n",
    ),
    (
        "fl := List[int].from_iter[List[int]]\nprint(fl([1]))\n",
        "[1]\n",
    ),
];

#[test]
fn builtin_static_accept_cells() {
    let mut src = String::from(PRELUDE);
    let mut want = String::new();
    for (e, out) in ACCEPT {
        src.push_str(&format!("print({e})\n"));
        want.push_str(out);
        want.push('\n');
    }
    let (ok, got) = chezzi("accept", "run", &src);
    assert!(
        ok && got == want,
        "program:\n{src}\nwant:\n{want}\ngot:\n{got}"
    );
    for (i, (body, out)) in ACCEPT_STMTS.iter().enumerate() {
        let src = format!("{PRELUDE}{body}");
        let (ok, got) = chezzi(&format!("stmt{i}"), "run", &src);
        assert!(
            ok && got == *out,
            "program:\n{src}\nwant:\n{out}\ngot:\n{got}"
        );
    }
}

/// (program, a line the type error must contain). Every old form with an argument names its new
/// form; the new forms reject what the decl's signature rejects.
const REJECT: &[(&str, &str)] = &[
    // The old forms with an argument.
    ("print(List([1]))", "List() takes no argument"),
    (
        "print(List([1]))",
        "help: build a List from an iterable with `List.from_iter(xs)`",
    ),
    (
        "print(List[int]([1]))",
        "help: build a List from an iterable with `List[int].from_iter(xs)`",
    ),
    (
        "print(Set([1]))",
        "help: build a Set from an iterable with `Set.from_iter(xs)`",
    ),
    (
        "print(Set[int]([1]))",
        "help: build a Set from an iterable with `Set[int].from_iter(xs)`",
    ),
    (
        "print(Map([(1, 2)]))",
        "help: build a Map from an iterable with `Map.from_iter(xs)`",
    ),
    (
        "print(Map[int, int]([(1, 2)]))",
        "help: build a Map from an iterable with `Map[int, int].from_iter(xs)`",
    ),
    ("print(bytes([1]))", "bytes() takes no argument"),
    (
        "print(bytes([1]))",
        "help: build a bytes from an iterable with `bytes.from_iter(xs)`",
    ),
    (
        "print(bytes(b\"x\"))",
        "help: build a bytes from an iterable with `bytes.from_iter(xs)`",
    ),
    (
        "print(bytes(3))",
        "help: make n zero bytes with `bytes.zeroed(n)`",
    ),
    ("print(bytearray([1]))", "bytearray() takes no argument"),
    (
        "print(bytearray(b\"x\"))",
        "help: build a bytearray from an iterable with `bytearray.from_iter(xs)`",
    ),
    (
        "print(bytearray(3))",
        "help: make n zero bytes with `bytearray.zeroed(n)`",
    ),
    // An untyped empty source cannot fix the element type, as `[]` cannot.
    (
        "x := List.from_iter([])",
        "cannot infer the element type of `x`: write `x: List[int] = ...` or `x := List[int]()`",
    ),
    // A missing static.
    (
        "print(List.nope(1))",
        "type 'List' has no static method 'nope'",
    ),
    (
        "print(bytes.nope(1))",
        "type 'bytes' has no static method 'nope'",
    ),
    // A static is reached through the type, never through a value.
    (
        "print([1].from_iter([2]))",
        "'from_iter' is a static method of 'List'; call it as `List.from_iter(...)`",
    ),
    // The static as a value with nothing to fix its type parameters, as for a user struct.
    ("f := List.from_iter", "'List.from_iter' is generic and"),
    // The type read as a value stays a type; the help names the statics that build one.
    (
        "f := List",
        "'List' is a type, not a value — use one of its static methods (`List.from_iter(…)`)",
    ),
    (
        "f := bytes",
        "'bytes' is a type, not a value — use one of its static methods (`bytes.from_iter(…)`, `bytes.zeroed(…)`)",
    ),
    // A local named `List` is a value, so `.from_iter` is its member.
    (
        "List := 3\nprint(List.from_iter([1]))",
        "type int has no method 'from_iter'",
    ),
    // The element rejects come from the decl's bounds.
    (
        "print(Set.from_iter([[1]]))",
        "type List[int] does not satisfy Hashable",
    ),
    (
        "print(Map.from_iter([1]))",
        "iterator element type int does not match the declared element type (K, V)",
    ),
    (
        "print(Map.from_iter([([1], 2)]))",
        "type List[int] does not satisfy Hashable",
    ),
    (
        "print(bytes.from_iter([\"a\"]))",
        "iterator element type str does not match the declared element type int",
    ),
    (
        "print(List[str].from_iter([1]))",
        "iterator element type int does not match the declared element type str",
    ),
    // A plain element never wraps into a carrier element (Rust E0271 on the same program): the
    // help names `?x`.
    (
        "print(List[int?].from_iter([5]))",
        "iterator element type int does not match the declared element type int?",
    ),
    (
        "print(List[int?].from_iter([5]))",
        "help: write each element as `?x`: the declared element type is int?",
    ),
    (
        "print(bytes.zeroed(\"a\"))",
        "argument 1 of 'zeroed': expected int, found str",
    ),
    // `int.from_be_bytes(b: bytes, signed: bool = false)`: `bytes` only, as `io.write_bytes`
    // (a `bytearray` converts with `bytes.from_iter(ba)`). The arity texts are the user twins': a
    // static's `'f' expects 2 argument(s)`, a method's `'f' expects 1–2 argument(s)`.
    (
        "print(int.from_be_bytes(bytearray.from_iter([1])))",
        "argument 1 of 'from_be_bytes': expected bytes, found bytearray",
    ),
    (
        "print(int.from_le_bytes())",
        "'from_le_bytes' expects 2 argument(s), got 0",
    ),
    (
        "print(int.from_be_bytes(b\"\", true, 1))",
        "'from_be_bytes' expects 2 argument(s), got 3",
    ),
    (
        "print(int.from_be_bytes(b\"\", 1))",
        "argument 2 of 'from_be_bytes': expected bool, found int",
    ),
    (
        "print((1).to_be_bytes())",
        "'to_be_bytes' expects 1–2 argument(s), got 0",
    ),
    (
        "print((1).to_le_bytes(2, true, 1))",
        "'to_le_bytes' expects 1–2 argument(s), got 3",
    ),
    (
        "print((1).to_be_bytes(\"2\"))",
        "argument 1 of 'to_be_bytes': expected int, found str",
    ),
    (
        "b: bytes = (1).to_be_bytes(2)",
        "cannot assign bytes! to variable of type bytes",
    ),
    (
        "n: int = int.from_be_bytes(b\"\")",
        "cannot assign int! to variable of type int",
    ),
];

/// One fault, one diagnostic: an element mismatch is the bound's failure, never reported twice. The
/// user-generic twin gives the same answer.
const ONE_ERROR: &[&str] = &[
    "xs := List[int].from_iter([\"a\"])",
    "print(bytes.from_iter(\"ab\"))",
    "print(Set[int].from_iter([1.5]))",
    "print(Map.from_iter([1]))",
    "print(List[int?].from_iter([5]))",
    "import Iterable from std.iter\nfn f[S: Iterable[int]](xs: S) -> int:\n    return 1\nprint(f([\"a\"]))",
];

#[test]
fn an_element_mismatch_is_one_error() {
    let mut bad = Vec::new();
    for (i, src) in ONE_ERROR.iter().enumerate() {
        let (ok, got) = chezzi(&format!("one{i}"), "check", &format!("{src}\n"));
        if ok || !got.contains("chezzi: 1 type error") {
            bad.push(format!("--- {src}\ngot:\n{got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn builtin_static_reject_cells() {
    let mut bad = Vec::new();
    for (i, (src, want)) in REJECT.iter().enumerate() {
        let (ok, got) = chezzi(&format!("rej{i}"), "check", &format!("{src}\n"));
        if ok || !got.contains(want) {
            bad.push(format!("--- {src}\nwant: {want}\ngot:\n{got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// (program, the runtime fault): a fault names the call that faulted.
const FAULT: &[(&str, &str)] = &[
    (
        "print(bytes.zeroed(-1))",
        "bytes.zeroed() size -1 must be non-negative",
    ),
    (
        "print(bytearray.zeroed(-1))",
        "bytearray.zeroed() size -1 must be non-negative",
    ),
    (
        "print(bytes.zeroed(9223372036854775807))",
        "bytes.zeroed() size 9223372036854775807 is too large to allocate",
    ),
    (
        "print(bytearray.zeroed(9223372036854775807))",
        "bytearray.zeroed() size 9223372036854775807 is too large to allocate",
    ),
    (
        "print(bytes.from_iter([300]))",
        "bytes.from_iter() element 300 out of range (must be 0..=255)",
    ),
    (
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    yield 300\nprint(bytes.from_iter(g()))",
        "bytes.from_iter() element 300 out of range (must be 0..=255)",
    ),
    (
        "print(bytearray.from_iter([-1]))",
        "bytearray.from_iter() element -1 out of range (must be 0..=255)",
    ),
];

#[test]
fn builtin_static_fault_cells() {
    let mut bad = Vec::new();
    for (i, (src, want)) in FAULT.iter().enumerate() {
        let (ok, got) = chezzi(&format!("fault{i}"), "run", &format!("{src}\n"));
        if ok || !got.contains(want) {
            bad.push(format!("--- {src}\nwant: {want}\ngot:\n{got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
