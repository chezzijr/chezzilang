//! Only keywords are reserved (owner rule, 2026-10-15). Every other name a program can see — a
//! builtin type (`int`, `List`), a builtin fn (`print`, `ord`), a builtin protocol (`Eq`), a std
//! module type (`std.ffi`'s `int32`), a removed spelling (`Result`, `range`) — is an ordinary name:
//! a module-level declaration or import of it is legal and SHADOWS the builtin inside that module,
//! exactly as a local variable already shadows it (`print := 3` inside a fn). Go and Python do the
//! same with their predeclared names.
//!
//! One cell per (declaration kind × namespace): the program declares the name and then USES it, and
//! the run must print what the user's declaration means, never the builtin.

use std::path::PathBuf;
use std::process::Command;

fn run(tag: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-shadow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "main.chz"])
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

/// (tag, program, expected stdout)
const CELLS: &[(&str, &str, &str)] = &[
    // A struct named after a builtin container type: its constructor and its annotation.
    (
        "struct-List",
        "struct List:\n    n: int\nl: List = List(3)\nprint(l.n)\n",
        "3\n",
    ),
    // A struct named after a scalar type. Inside this module `int` IS the struct, as in Go, so its
    // own field takes another type.
    (
        "struct-int",
        "struct int:\n    n: str\nx := int(\"q\")\nprint(x.n)\n",
        "q\n",
    ),
    // An alias named after a scalar type.
    (
        "alias-int",
        "type int = str\nx: int = \"s\"\nprint(x)\n",
        "s\n",
    ),
    // An enum named after a removed carrier spelling.
    (
        "enum-Result",
        "enum Result:\n    Good\n    Bad\nr := Result.Bad\nmatch r:\n    Result.Good: print(\"g\")\n    Result.Bad: print(\"b\")\n",
        "b\n",
    ),
    // A fn named after a builtin fn.
    (
        "fn-ord",
        "fn ord(c: str) -> int:\n    return 7\nprint(ord(\"a\"))\n",
        "7\n",
    ),
    // A fn named after the removed `range()`.
    (
        "fn-range",
        "fn range(n: int) -> int:\n    return n * 2\nprint(range(3))\n",
        "6\n",
    ),
    // A protocol named after a builtin protocol, used as a bound.
    (
        "protocol-Eq",
        "protocol Eq:\n    fn tag(self) -> int\nstruct P:\n    n: int\n    fn tag(self) -> int:\n        return self.n\nfn f[T: Eq](x: T) -> int:\n    return x.tag()\nprint(f(P(5)))\n",
        "5\n",
    ),
    // A std.ffi type name, declared with no import of std.ffi.
    (
        "struct-int32",
        "struct int32:\n    n: int\nprint(int32(6).n)\n",
        "6\n",
    ),
    // A type parameter named after a builtin type.
    (
        "tparam-List",
        "fn f[List](x: List) -> List:\n    return x\nprint(f(8))\n",
        "8\n",
    ),
    // An import alias named after a builtin type.
    (
        "import-as-int",
        "import std.math as int\nprint(int.abs(-9))\n",
        "9\n",
    ),
    // A from-import member bound under a builtin's name.
    (
        "import-member-as-int",
        "import abs as int from std.math\nprint(int(-4))\n",
        "4\n",
    ),
    // A struct named after a builtin handle type, a std type and a removed type spelling.
    (
        "struct-Executor",
        "struct Executor:\n    n: int\nprint(Executor(3).n)\n",
        "3\n",
    ),
    (
        "struct-Iterator",
        "struct Iterator:\n    n: int\nprint(Iterator(2).n)\n",
        "2\n",
    ),
    (
        "struct-tuple",
        "struct tuple:\n    n: int\nprint(tuple(5).n)\n",
        "5\n",
    ),
    // The prelude's `Range`, as a struct and as a fn.
    (
        "struct-Range",
        "struct Range:\n    a: int\nprint(Range(7).a)\n",
        "7\n",
    ),
    (
        "fn-Range",
        "fn Range(a: int) -> int:\n    return a * 3\nprint(Range(2))\n",
        "6\n",
    ),
    // A fn named after a runtime constructor.
    (
        "fn-timer",
        "fn timer(n: int) -> int:\n    return n + 1\nprint(timer(1))\n",
        "2\n",
    ),
    // A nested fn named after a removed variant spelling.
    (
        "nested-fn-Ok",
        "fn outer() -> int:\n    fn Ok() -> int:\n        return 99\n    return Ok()\nprint(outer())\n",
        "99\n",
    ),
    // An enum named after a removed carrier spelling, and one named after a builtin protocol.
    (
        "enum-Option",
        "enum Option:\n    Yes\n    No\nmatch Option.No:\n    Option.Yes: print(\"y\")\n    Option.No: print(\"n\")\n",
        "n\n",
    ),
    (
        "enum-Num",
        "enum Num:\n    A\n    B\nmatch Num.B:\n    Num.A: print(\"a\")\n    Num.B: print(\"b\")\n",
        "b\n",
    ),
    // A protocol named after a builtin type, and one named after a builtin protocol, as bounds.
    (
        "protocol-int",
        "protocol int:\n    fn v(self) -> str\nstruct P:\n    k: int\n    fn v(self) -> str:\n        return \"p\"\nfn f[T: int](x: T) -> str:\n    return x.v()\nprint(f(P(0)))\n",
        "p\n",
    ),
    (
        "protocol-Comparable",
        "protocol Comparable:\n    fn rank(self) -> int\nstruct P:\n    k: int\n    fn rank(self) -> int:\n        return self.k\nfn f[T: Comparable](x: T) -> int:\n    return x.rank()\nprint(f(P(4)))\n",
        "4\n",
    ),
    // Type parameters named after builtin types.
    (
        "tparam-Shared",
        "import Shared from std.concurrency\nfn f[Shared](x: Shared) -> Shared:\n    return x\nprint(f(\"s\"))\n",
        "s\n",
    ),
    // Syntax and operators mean the BUILTIN, whatever the module declares: the user's `Eq`,
    // `Range`, `Error`, `Hashable`, `Comparable` or `Stringable` shadows the name, never `==`,
    // `a..b`, `!e`, a map key, `sort` or `print`.
    (
        "op-eq-under-user-Eq",
        "protocol Eq:\n    fn zap(self) -> int\nstruct P:\n    a: int\n    fn eq(self, other: P) -> bool:\n        return true\nprint(P(1) == P(2))\n",
        "true\n",
    ),
    (
        "range-literal-under-user-Range",
        "struct Range:\n    x: str\nfor i in 0..3:\n    print(i)\nprint(3 in 0..5)\n",
        "0\n1\n2\ntrue\n",
    ),
    (
        "error-value-under-user-Error-protocol",
        "protocol Error:\n    fn code(self) -> int\nfn f(x: int) -> int!:\n    return !\"bad\"\nmatch f(0):\n    ?v: print(v)\n    !e: print(e.message())\n",
        "bad\n",
    ),
    (
        "error-carrier-under-user-Error-struct",
        "struct Error:\n    code: int\nfn f(x: int) -> int!:\n    return x\nprint(f(3) ?? 0)\n",
        "3\n",
    ),
    (
        "map-key-under-user-Hashable",
        "protocol Hashable:\n    fn zap(self) -> int\nstruct P:\n    a: int\n    fn hash(self) -> int:\n        return self.a\n    fn eq(self, other: P) -> bool:\n        return self.a == other.a\nm := {P(1): \"x\"}\nprint(m.len())\n",
        "1\n",
    ),
    (
        "sort-under-user-Comparable",
        "protocol Comparable:\n    fn zap(self) -> int\nxs := [3, 1, 2]\nxs.sort()\nprint(xs)\n",
        "[1, 2, 3]\n",
    ),
    // An alias named after a std.ffi width, with no import of std.ffi.
    (
        "alias-int32",
        "type int32 = str\nx: int32 = \"w\"\nprint(x)\n",
        "w\n",
    ),
];

#[test]
fn a_module_declaration_shadows_the_builtin_of_its_name() {
    let mut wrong = Vec::new();
    for (tag, src, want) in CELLS {
        let (ok, got) = run(tag, src);
        if !ok || got != *want {
            wrong.push(format!("{tag}: want {want:?}, got {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "cells that do not shadow:\n{}",
        wrong.join("\n")
    );
}

/// A keyword stays reserved: `fn if()` is a parse error.
#[test]
fn a_keyword_is_still_reserved() {
    let (ok, got) = run("kw", "fn if() -> int:\n    return 1\n");
    assert!(!ok, "a keyword must not name a fn: {got}");
}

#[test]
fn a_shadowing_type_prints_qualified_next_to_the_builtin() {
    let (ok, got) = run(
        "msg-int",
        "struct int:\n    x: int\nfn f(a: int):\n    pass\nf(int(3))\n",
    );
    assert!(
        !ok && got.contains("expected main.int, found int"),
        "got {got:?}"
    );
}

#[test]
fn a_shadowing_container_prints_qualified_next_to_the_builtin() {
    let (ok, got) = run(
        "msg-list",
        "struct List:\n    n: int\nfn f(a: List):\n    pass\nf([1])\n",
    );
    assert!(
        !ok && got.contains("expected main.List, found List[int]"),
        "got {got:?}"
    );
}
