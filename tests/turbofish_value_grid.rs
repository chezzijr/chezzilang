//! TICKET-204: the type-application and path-value grid. A type-applied generic fn `pair[str, int]`
//! is a value at every arity, in every position, through every head (same-module, `import F from`,
//! `lib.F`, a static method, a bound method), and a type path follows Rust's path-value rule: a
//! payload variant, a static method and an instance method named through its type are values; a
//! bound method, a type, a protocol method and a native method are not, each with its own message.
//! One generated program per cell, run through the built `chezzi` binary.
//!
//! Both carriers of `head[T…]` (a one-arg `Index`, a multi-arg `TypeApply`) are read through
//! `ast::type_application`; a red cell here means a second decider has come back.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LIB: &str = "fn idt[A](a: A) -> A:
    return a
fn pair[A, B](a: A, b: B) -> (A, B):
    return (a, b)
fn tri[A, B, C](a: A, b: B, c: C) -> (A, B, C):
    return (a, b, c)
struct Box[T]:
    v: T
    fn make1[U](x: U) -> U:
        return x
    fn make2[U, V](x: U, y: V) -> (U, V):
        return (x, y)
    fn make3[U, V, W](x: U, y: V, z: W) -> (U, V, W):
        return (x, y, z)
    fn m1[U](self, x: U) -> U:
        return x
    fn m2[U, V](self, x: U, y: V) -> (U, V):
        return (x, y)
    fn m3[U, V, W](self, x: U, y: V, z: W) -> (U, V, W):
        return (x, y, z)
";

const OLIB: &str = "struct Box[T]:
    v: T
    fn mk[U](x: U) -> U:
        return x
struct Pr[A, B]:
    a: A
    b: B
    fn mk(a: A, b: B) -> Pr[A, B]:
        return Pr(a=a, b=b)
enum R1[T]:
    L(T)
    N
enum R2[T, U]:
    L(T)
    R(U)
    N
";

const PLIB: &str = "enum Pair[T, U]:
    Both(T, U)
    Neither
";

const VLIB: &str = "fn pair[A, B](a: A, b: B) -> (A, B):
    return (a, b)
struct Bx[T]:
    v: T
    fn make(v: T) -> Bx[T]:
        return Bx(v=v)
    fn get(self) -> T:
        return self.v
    fn put[U](self, u: U) -> (T, U):
        return (self.v, u)
struct Pt:
    x: int
    fn origin() -> Pt:
        return Pt(x=0)
    fn getx(self) -> int:
        return self.x
enum R1[T]:
    L(T)
    N
enum R2[T, U]:
    L(T)
    R(U)
    N
enum Col:
    Red
    Rgb(int, int, int)
protocol Show:
    fn show(self) -> str
";

const ALIB: &str = "enum R1[T]:
    L(T)
    N
struct Bx[T]:
    v: T
    fn make(v: T) -> Bx[T]:
        return Bx(v=v)
    fn get(self) -> T:
        return self.v
    fn put[U](self, u: U) -> (T, U):
        return (self.v, u)
type A = R1[int]
type B = Bx[int]
";

const QLIB: &str = "enum E[T]:
    V(T)
    N
    fn mk() -> E[int]:
        return E.V(9)
";

fn prints(s: &str) -> Expect {
    Expect::Prints(s.to_string())
}

fn cell(name: String, files: Vec<(&str, String)>, expect: Expect) -> Cell {
    Cell {
        name,
        files: files.into_iter().map(|(p, s)| (p.to_string(), s)).collect(),
        expect,
    }
}

/// A program of `main.chz` alone.
fn main_only(name: &str, src: &str, expect: Expect) -> Cell {
    cell(
        name.to_string(),
        vec![("main.chz", format!("{src}\n"))],
        expect,
    )
}

/// A program of `main.chz` beside one library file.
fn with_lib(name: String, lib: (&str, &str), main: String, expect: Expect) -> Cell {
    cell(
        name,
        vec![
            (lib.0, lib.1.to_string()),
            ("main.chz", format!("{main}\n")),
        ],
        expect,
    )
}

/// The channel block that runs `send` inside a `spawn:` and prints what it sent.
fn spawn_block(send: &str) -> String {
    format!(
        "r := Channel[str](1)\nparallel:\n    spawn:\n        r.send('{{{send}}}')\nprint(r.recv())"
    )
}

/// Arity {1, 2, 3, inferred} x head x position.
fn arity_cells(out: &mut Vec<Cell>) {
    for ar in ["1", "2", "3", "inf"] {
        let (f, ta, args, ft, kw, want) = match ar {
            "1" => ("idt", "[str]", "\"a\"", "fn(str) -> str", "a=\"a\"", "a"),
            "2" | "inf" => (
                "pair",
                if ar == "2" { "[str, int]" } else { "" },
                "\"a\", 1",
                "fn(str, int) -> (str, int)",
                "b=1, a=\"a\"",
                "('a', 1)",
            ),
            _ => (
                "tri",
                "[str, int, bool]",
                "\"a\", 1, true",
                "fn(str, int, bool) -> (str, int, bool)",
                "c=true, b=1, a=\"a\"",
                "('a', 1, true)",
            ),
        };
        let n = if ar == "inf" { "2" } else { ar };
        let heads: &[&str] = if ar == "inf" {
            &["same", "imp", "mod", "static"]
        } else {
            &["same", "imp", "mod", "static", "method"]
        };
        for head in heads {
            let (pre, h) = match *head {
                "same" => (LIB.trim_end().to_string(), f.to_string()),
                "imp" => (format!("import {f} from lib"), f.to_string()),
                "mod" => ("import lib".to_string(), format!("lib.{f}")),
                "static" => ("import Box from lib".to_string(), format!("Box.make{n}")),
                _ => (
                    "import Box from lib\no := Box(v=0)".to_string(),
                    format!("o.m{n}"),
                ),
            };
            let v = format!("{h}{ta}");
            let mut pos: Vec<(&str, String)> = vec![
                ("let", format!("p := {v}\nprint(p({args}))")),
                ("typed", format!("p: {ft} = {v}\nprint(p({args}))")),
                (
                    "hof",
                    format!("fn ap(f: {ft}) -> str:\n    return '{{f({args})}}'\nprint(ap({v}))"),
                ),
                (
                    "ret",
                    format!("fn g() -> {ft}:\n    return {v}\nprint(g()({args}))"),
                ),
                (
                    "default",
                    format!("fn g(f: {ft} = {v}) -> str:\n    return '{{f({args})}}'\nprint(g())"),
                ),
                ("called", format!("print({v}({args}))")),
                (
                    "spawn",
                    format!("p := {v}\n{}", spawn_block(&format!("p({args})"))),
                ),
            ];
            if ar != "inf" {
                pos.push(("interp_call", format!("print('{{{v}({args})}}')")));
                if matches!(*head, "same" | "imp" | "mod") {
                    pos.push(("kw", format!("p := {v}\nprint(p({kw}))")));
                }
            }
            for (p, body) in pos {
                let expect = if ar == "inf" {
                    if matches!(p, "let" | "spawn") {
                        Expect::Rejects("not determined here")
                    } else {
                        prints(want)
                    }
                } else if *head == "method" && !matches!(p, "interp_call" | "called") {
                    Expect::Rejects("a bound method is not a value")
                } else {
                    prints(want)
                };
                out.push(with_lib(
                    format!("a{ar}/{head}/{p}"),
                    ("lib.chz", LIB),
                    format!("{pre}\n{body}"),
                    expect,
                ));
            }
        }
    }
}

/// The three heads of a library type: inlined, `import … from`, `module.`.
fn heads3<'a>(
    lib: &'a str,
    module: &'a str,
    imports: &'a str,
) -> [(&'static str, String, &'a str); 3] {
    [
        ("same", lib.trim_end().to_string(), ""),
        ("imp", format!("import {imports} from {module}"), ""),
        ("mod", format!("import {module}"), module),
    ]
}

/// Owner-note rows: constructors are not values; variants and static methods are; named
/// arguments combine with type arguments.
fn owner_cells(out: &mut Vec<Cell>) {
    for (h, pre, m) in heads3(OLIB, "olib", "Box, Pr, R1, R2") {
        let q = if m.is_empty() {
            String::new()
        } else {
            format!("{m}.")
        };
        let rows: Vec<(&str, String, Expect)> = vec![
            (
                "ctor1_val",
                format!("f := {q}Box[int]"),
                Expect::Rejects("is a type, not a value"),
            ),
            (
                "ctor2_val",
                format!("f := {q}Pr[int, str]"),
                Expect::Rejects("is a type, not a value"),
            ),
            (
                "ctor_bare_val",
                format!("f := {q}Box"),
                Expect::Rejects("is a type, not a value"),
            ),
            (
                "var1_val",
                format!("f := {q}R1[int].L\nprint(f(1))"),
                prints("L(1)"),
            ),
            (
                "var2_val",
                format!("f := {q}R2[int, str].L\nprint(f(1))"),
                prints("L(1)"),
            ),
            (
                "var_bare_let",
                format!("f := {q}R1.L"),
                Expect::Rejects("not determined here"),
            ),
            (
                "var2_nullary",
                format!("print({q}R2[int, str].N)"),
                prints("N"),
            ),
            (
                "var2_call",
                format!("print({q}R2[int, str].L(1))"),
                prints("L(1)"),
            ),
            (
                "static2_call",
                format!("print({q}Pr[int, str].mk(1, \"x\"))"),
                prints("Pr(a=1, b='x')"),
            ),
            // `mk[U]` keeps its own `U` free: a let with nothing to pin it is Go's
            // "cannot use generic function without instantiation".
            (
                "static1_val",
                format!("f := {q}Box[int].mk\nprint(f(1))"),
                Expect::Rejects("not determined here"),
            ),
            (
                "static1_turbo",
                format!("f := {q}Box[int].mk[int]\nprint(f(1))"),
                prints("1"),
            ),
            (
                "static2_val",
                format!("f := {q}Pr[int, str].mk\nprint(f(1, \"x\"))"),
                prints("Pr(a=1, b='x')"),
            ),
            (
                "ctor1_named",
                format!("print({q}Box[int](v=1))"),
                prints("Box(v=1)"),
            ),
            (
                "ctor1_pos",
                format!("print({q}Box[int](1))"),
                prints("Box(v=1)"),
            ),
            (
                "ctor2_named",
                format!("print({q}Pr[int, str](b=\"x\", a=1))"),
                prints("Pr(a=1, b='x')"),
            ),
        ];
        for (name, body, expect) in rows {
            out.push(with_lib(
                format!("own/{h}/{name}"),
                ("olib.chz", OLIB),
                format!("{pre}\n{body}"),
                expect,
            ));
        }
    }
    out.push(main_only(
        "own/fn_named",
        "fn pair[A, B](a: A, b: B) -> (A, B):\n    return (a, b)\nprint(pair[str, int](b=1, a=\"a\"))",
        prints("('a', 1)"),
    ));
}

/// `type_head` rows: every kind of type in value position gets the type-as-value message.
fn type_head_cells(out: &mut Vec<Cell>) {
    for (h, pre, m) in heads3(OLIB, "olib", "Box, Pr, R1, R2") {
        let q = if m.is_empty() {
            String::new()
        } else {
            format!("{m}.")
        };
        for (name, body) in [
            ("enum_bare", format!("f := {q}R2")),
            ("enum1", format!("f := {q}R1[int]")),
            ("enum2", format!("f := {q}R2[int, str]")),
        ] {
            out.push(with_lib(
                format!("thead/{h}/{name}"),
                ("olib.chz", OLIB),
                format!("{pre}\n{body}"),
                Expect::Rejects("is a type, not a value — use one of its variants"),
            ));
        }
    }
}

/// Fixed neighbour cells: subscripts keep working, and the multi-arg form rejects cleanly
/// wherever it is not a fn value.
fn neighbour_cells(out: &mut Vec<Cell>) {
    let pair = "fn pair[A, B](a: A, b: B) -> (A, B):\n    return (a, b)\n";
    out.push(main_only(
        "idx_list",
        "xs := [10, 20]\ni := 1\nprint(xs[i])",
        prints("20"),
    ));
    out.push(main_only(
        "idx_map",
        "m := {\"a\": 1}\nk := \"a\"\nprint(m[k])",
        prints("1"),
    ));
    out.push(main_only(
        "idx_nested",
        "a := [[1, 2], [3, 4]]\nprint(a[0][1])",
        prints("2"),
    ));
    out.push(main_only(
        "slice",
        "xs := [1, 2, 3, 4]\nprint(xs[1:3], xs[::2])",
        prints("[2, 3] [1, 3]"),
    ));
    out.push(main_only(
        "idx_two_names",
        "xs := [10, 20]\ni := 0\nj := 1\nprint(xs[i, j])",
        Expect::Rejects("a subscript takes one index, found 2"),
    ));
    out.push(main_only(
        "idx_two_lits",
        "xs := [10, 20]\nprint(xs[0, 1])",
        Expect::Rejects("expected ']', found ','"),
    ));
    out.push(main_only(
        "idx_assign_two",
        "xs := [10, 20]\ni := 0\nj := 1\nxs[i, j] = 5",
        Expect::Rejects("invalid assignment target"),
    ));
    out.push(with_lib(
        "qual_type_multi".to_string(),
        ("plib.chz", PLIB),
        "import plib\nprint(plib.Pair[int, str].Both(1, \"x\"))".to_string(),
        prints("Both(1, 'x')"),
    ));
    out.push(with_lib(
        "qual_type_multi_null".to_string(),
        ("plib.chz", PLIB),
        "import plib\nprint(plib.Pair[int, str].Neither)".to_string(),
        prints("Neither"),
    ));
    out.push(main_only(
        "type_as_value",
        &format!("{PLIB}p := Pair[int, str]"),
        Expect::Rejects("'Pair' is a type, not a value"),
    ));
    out.push(main_only(
        "arity_short",
        &format!("{pair}p := pair[int]"),
        Expect::Rejects("'pair' expects 2 type argument(s), found 1"),
    ));
    out.push(main_only(
        "arity_long",
        &format!("{pair}p := pair[int, str, bool]"),
        Expect::Rejects("'pair' expects 2 type argument(s), found 3"),
    ));
    out.push(main_only(
        "bound_viol",
        "fn add2[A: Add, B](a: A, b: B) -> A:\n    return a + a\np := add2[bool, int]",
        Expect::Rejects("type bool does not satisfy Add"),
    ));
    // A conditional method read as a value keeps its receiver `where` bound (Rust E0599 on
    // `Bx::<P>::total`), in every position that pins the head.
    let wbx = "struct Bx[T]:\n    v: T\n    fn total(self, o: Bx[T]) -> T where T: Add:\n        return self.v + o.v\nstruct P:\n    n: int\n";
    let ap2 = "fn ap2[X](f: fn(Bx[P], X) -> P, x: X) -> int:\n    return 0\n";
    for (name, tail) in [
        ("where_let", "f := Bx[P].total".to_string()),
        (
            "where_typed",
            "g: fn(Bx[P], Bx[P]) -> P = Bx.total".to_string(),
        ),
        ("where_alias", "type B = Bx[P]\nf := B.total".to_string()),
        (
            "where_ghof",
            format!("{ap2}print(ap2(Bx.total, Bx(v=P(n=1))))"),
        ),
    ] {
        out.push(main_only(
            name,
            &format!("{wbx}{tail}"),
            Expect::Rejects("type P does not satisfy Add"),
        ));
    }
    out.push(main_only(
        "where_ok",
        &format!("{wbx}f := Bx[int].total\nprint(f(Bx(v=1), Bx(v=2)))"),
        prints("3"),
    ));
    out.push(main_only(
        "type_mismatch",
        &format!("{pair}p := pair[str, int]\nprint(p(1, \"x\"))"),
        Expect::Rejects("expected str, found int"),
    ));
    out.push(main_only(
        "nongeneric",
        "fn f(a: int, b: int) -> int:\n    return a\np := f[int, str]",
        Expect::Rejects("a subscript takes one index, found 2"),
    ));
    out.push(with_lib(
        "meth_named".to_string(),
        ("lib.chz", LIB),
        "import Box from lib\nprint(Box.make2[str, int](y=1, x=\"a\"))".to_string(),
        prints("('a', 1)"),
    ));
    out.push(with_lib(
        "inst_named".to_string(),
        ("lib.chz", LIB),
        "import Box from lib\no := Box(v=0)\nprint(o.m2[str, int](y=1, x=\"a\"))".to_string(),
        prints("('a', 1)"),
    ));
    out.push(main_only(
        "native_qual",
        "import std.net\nf := net.Socket",
        Expect::Rejects("'net.Socket' is a type, not a value"),
    ));
    out.push(main_only(
        "type_member_miss",
        "struct S:\n    n: int\nh := S.nosuch",
        Expect::Rejects("'S' is a type, not a value"),
    ));
}

/// Path-value rows (Rust's rule): variants, static methods and methods through their type are
/// values; bound methods, types and protocol methods are not.
fn path_value_cells(out: &mut Vec<Cell>) {
    for (h, pre, m) in heads3(VLIB, "vlib", "Bx, Pt, R1, R2, Col, Show") {
        let q = if m.is_empty() {
            String::new()
        } else {
            format!("{m}.")
        };
        let rows: Vec<(&str, String, Expect)> = vec![
            (
                "var1",
                format!("f := {q}R1[int].L\nprint(f(1))"),
                prints("L(1)"),
            ),
            (
                "var2",
                format!("f := {q}R2[int, str].L\nprint(f(1))"),
                prints("L(1)"),
            ),
            (
                "var_typed",
                format!("f: fn(int) -> {q}R1[int] = {q}R1.L\nprint(f(2))"),
                prints("L(2)"),
            ),
            (
                "var_hof",
                format!(
                    "fn ap(f: fn(int) -> {q}R1[int]) -> str:\n    return '{{f(3)}}'\nprint(ap({q}R1.L))"
                ),
                prints("L(3)"),
            ),
            (
                "var_ret",
                format!(
                    "fn g() -> fn(int) -> {q}R2[int, str]:\n    return {q}R2[int, str].L\nprint(g()(4))"
                ),
                prints("L(4)"),
            ),
            (
                "var_default",
                format!(
                    "fn g(f: fn(int) -> {q}R1[int] = {q}R1[int].L) -> str:\n    return '{{f(5)}}'\nprint(g())"
                ),
                prints("L(5)"),
            ),
            (
                "var_nongen",
                format!("f := {q}Col.Rgb\nprint(f(1, 2, 3))"),
                prints("Rgb(1, 2, 3)"),
            ),
            (
                "var_spawn",
                format!("f := {q}R1[int].L\n{}", spawn_block("f(7)")),
                prints("L(7)"),
            ),
            (
                "var_let_inf",
                format!("f := {q}R1.L"),
                Expect::Rejects("not determined here"),
            ),
            (
                "var_kw",
                format!("f := {q}R1[int].L\nprint(f(x=1))"),
                Expect::Rejects("unknown named argument 'x'"),
            ),
            (
                "static1",
                format!("f := {q}Bx[int].make\nprint(f(1).get())"),
                prints("1"),
            ),
            (
                "static_typed",
                format!("f: fn(int) -> {q}Bx[int] = {q}Bx.make\nprint(f(2).get())"),
                prints("2"),
            ),
            (
                "static_hof",
                format!(
                    "fn ap(f: fn(int) -> {q}Bx[int]) -> int:\n    return f(3).get()\nprint(ap({q}Bx.make))"
                ),
                prints("3"),
            ),
            (
                "static_nongen",
                format!("f := {q}Pt.origin\nprint(f().x)"),
                prints("0"),
            ),
            (
                "static_kw",
                format!("f := {q}Bx[int].make\nprint(f(v=5).get())"),
                prints("5"),
            ),
            (
                "tmeth1",
                format!("g := {q}Bx[int].get\nprint(g({q}Bx(v=3)))"),
                prints("3"),
            ),
            (
                "tmeth_turbo",
                format!("g := {q}Bx[int].put[str]\nprint(g({q}Bx(v=1), \"a\"))"),
                prints("(1, 'a')"),
            ),
            (
                "tmeth_kw",
                format!("g := {q}Bx[int].put[str]\nprint(g({q}Bx(v=1), u=\"a\"))"),
                prints("(1, 'a')"),
            ),
            (
                "tmeth_self_kw",
                format!("g := {q}Pt.getx\nprint(g(self={q}Pt(x=6)))"),
                prints("6"),
            ),
            (
                "tmeth_ret",
                format!(
                    "fn g() -> fn({q}Pt) -> int:\n    return {q}Pt.getx\nprint(g()({q}Pt(x=4)))"
                ),
                prints("4"),
            ),
            (
                "tmeth_spawn",
                format!(
                    "g := {q}Bx[int].get\nb := {q}Bx(v=8)\n{}",
                    spawn_block("g(b)")
                ),
                prints("8"),
            ),
            (
                "bound",
                format!("b := {q}Bx(v=1)\nh := b.get"),
                Expect::Rejects("a bound method is not a value"),
            ),
            (
                "struct_val",
                format!("f := {q}Bx[int]"),
                Expect::Rejects("is a type, not a value — constructors are not values"),
            ),
            (
                "struct_bare",
                format!("f := {q}Pt"),
                Expect::Rejects("is a type, not a value — constructors are not values"),
            ),
            (
                "enum_bare",
                format!("f := {q}R2"),
                Expect::Rejects("use one of its variants"),
            ),
            (
                "proto",
                format!("f := {q}Show.show"),
                Expect::Rejects("a protocol method is not a value"),
            ),
            (
                "proto_bare",
                format!("f := {q}Show"),
                Expect::Rejects("is a protocol, not a value"),
            ),
        ];
        for (name, body, expect) in rows {
            out.push(with_lib(
                format!("path/{h}/{name}"),
                ("vlib.chz", VLIB),
                format!("{pre}\n{body}"),
                expect,
            ));
        }
    }
}

/// Alias rows: an alias head pins its args and is a path head like any type.
fn alias_cells(out: &mut Vec<Cell>) {
    for (h, pre, m) in heads3(ALIB, "alib", "A, B, Bx, R1") {
        let q = if m.is_empty() {
            String::new()
        } else {
            format!("{m}.")
        };
        let ap2 = format!(
            "fn ap2[X](f: fn({q}Bx[int], X) -> (int, X), x: X) -> (int, X):\n    return f({q}Bx(v=1), x)"
        );
        let rows: Vec<(&str, String, Expect)> = vec![
            (
                "alias_var",
                format!("f := {q}A.L\nprint(f(1))"),
                prints("L(1)"),
            ),
            (
                "alias_var_typed",
                format!("f: fn(int) -> {q}R1[int] = {q}A.L\nprint(f(2))"),
                prints("L(2)"),
            ),
            ("alias_null", format!("print({q}A.N)"), prints("N")),
            ("alias_call", format!("print({q}A.L(1))"), prints("L(1)")),
            (
                "alias_miss",
                format!("f := {q}A.Q"),
                Expect::Rejects("has no variant 'Q'"),
            ),
            (
                "alias_static",
                format!("m := {q}B.make\nprint(m(2).get())"),
                prints("2"),
            ),
            (
                "alias_tmeth",
                format!("g := {q}B.get\nprint(g({q}Bx(v=3)))"),
                prints("3"),
            ),
            (
                "alias_val",
                format!("f := {q}A"),
                Expect::Rejects("is a type, not a value — use one of its variants"),
            ),
            (
                "alias_struct_val",
                format!("f := {q}B"),
                Expect::Rejects("is a type, not a value — constructors are not values"),
            ),
            (
                "alias_applied",
                format!("f := {q}A[int].L"),
                Expect::Rejects("error"),
            ),
            (
                "alias_hof",
                format!(
                    "fn ap(f: fn({q}Bx[int], str) -> (int, str)) -> str:\n    return '{{f({q}Bx(v=1), \"a\")}}'\nprint(ap({q}B.put))"
                ),
                prints("(1, 'a')"),
            ),
            (
                "ghof_applied",
                format!("{ap2}\nprint(ap2({q}Bx[int].put, \"a\"))"),
                prints("(1, 'a')"),
            ),
            (
                "ghof_bare",
                format!("{ap2}\nprint(ap2({q}Bx.put, \"a\"))"),
                prints("(1, 'a')"),
            ),
            (
                "ghof_alias",
                format!("{ap2}\nprint(ap2({q}B.put, \"a\"))"),
                prints("(1, 'a')"),
            ),
        ];
        for (name, body, expect) in rows {
            out.push(with_lib(
                format!("alias/{h}/{name}"),
                ("alib.chz", ALIB),
                format!("{pre}\n{body}"),
                expect,
            ));
        }
    }
}

/// Native-handle rows: a reserved native handle's method has no proto, so it is not a value.
fn native_cells(out: &mut Vec<Cell>) {
    let not_value = || Expect::Rejects("a native method is not a value");
    out.push(main_only(
        "native_io",
        "import std.io\ng := io.Reader.close\nprint(\"x\")",
        not_value(),
    ));
    out.push(main_only(
        "native_net",
        "import std.net\ng := net.Socket.close\nprint(\"x\")",
        not_value(),
    ));
    out.push(main_only(
        "native_accept",
        "import std.net\ng := net.Listener.accept\nprint(\"x\")",
        not_value(),
    ));
    out.push(main_only(
        "native_bodied",
        "import std.concurrency\ng := concurrency.Executor.submit_result\nprint(\"x\")",
        not_value(),
    ));
    out.push(main_only(
        "native_hof",
        "import std.io\nfn ap(f: fn(io.Reader) -> Result[None]) -> int:\n    return 0\nprint(ap(io.Reader.close))",
        not_value(),
    ));
    out.push(main_only(
        "native_bare",
        "import Reader from std.io\ng := Reader.close",
        Expect::Rejects("unknown name 'Reader'"),
    ));
    out.push(main_only(
        "native_alias",
        "import std.io\ntype W = io.Writer\nf := W.close",
        Expect::Rejects("unknown name 'W'"),
    ));
    out.push(main_only(
        "native_call",
        "import std.io\nprint(io.Reader.close())",
        Expect::Rejects("call it on a value"),
    ));
}

/// Qualified-enum rows: green on base, they pin the `qualified_type_head` folds.
fn qenum_cells(out: &mut Vec<Cell>) {
    let clash = "enum E:\n    V(str)\n";
    let rows: Vec<(&str, String, Expect)> = vec![
        (
            "qenum_call",
            "print(qlib.E.V(1))\nprint(qlib.E[int].V(2))\nprint(qlib.E.mk())\nprint(qlib.E.N)"
                .to_string(),
            prints("V(1)\nV(2)\nV(9)\nN"),
        ),
        (
            "qenum_clash",
            format!("{clash}print(qlib.E.V(1))\nprint(E.V(\"x\"))"),
            prints("V(1)\nV('x')"),
        ),
        (
            "qenum_defer",
            "fn f():\n    defer qlib.E.V(1)\n    print(\"a\")\nf()".to_string(),
            Expect::Rejects("defer requires a function or method call"),
        ),
        (
            "qenum_defer_applied",
            "fn f():\n    defer qlib.E[int].V(1)\n    print(\"a\")\nf()".to_string(),
            Expect::Rejects("defer requires a function or method call"),
        ),
        (
            "qenum_defer_clash",
            format!("{clash}fn f():\n    defer qlib.E.V(1)\n    print(\"a\")\nf()"),
            Expect::Rejects("defer requires a function or method call"),
        ),
        (
            "qenum_defer_static",
            "fn f():\n    defer qlib.E.mk()\n    print(\"a\")\nf()".to_string(),
            prints("a"),
        ),
        (
            "qenum_miss",
            "print(qlib.E.Q(1))".to_string(),
            Expect::Rejects("type 'qlib.E' has no static method 'Q'"),
        ),
        (
            "qenum_variant_targs",
            "print(qlib.E.V[int](1))".to_string(),
            Expect::Rejects("put the type arguments on the type: qlib.E[int].V(...)"),
        ),
    ];
    for (name, body, expect) in rows {
        out.push(with_lib(
            name.to_string(),
            ("qlib.chz", QLIB),
            format!("import qlib\n{body}"),
            expect,
        ));
    }
}

const BLIB: &str = "fn idt[T](x: T) -> T:
    return x
struct P:
    x: int
struct Bx[T]:
    v: T
    fn make(v: T) -> Bx[T]:
        return Bx(v=v)
    fn get(self) -> T:
        return self.v
enum E[T]:
    A(T)
    N
";

const BPRE: &str = "import std.json
import std.math
import lib
import Bx, E from lib
fn okv[T](r: Result[T]) -> T:
    match r:
        Ok(v):
            return v
        Err(e):
            panic(e.message())
fn un[T](e: E[T]) -> T:
    match e:
        E.A(v):
            return v
        E.N:
            panic('N')
fn yes[A](a: A) -> bool:
    return true
fn inc(x: int) -> int:
    return x + 1
";

/// TICKET-222: type-argument shape x head x position. `head[X]` is one bracket whose `X` is read
/// by the type grammar whatever its shape; the head picks type application over indexing. Each
/// accept cell prints what the Rust reference printed (`idt::<(i64, String)>`, `(i64::abs)(-6)`).
fn bracket_grid_cells(out: &mut Vec<Cell>) {
    // (key, type, value, JSON text of the value, show of a result `{}`, printed)
    let shapes = [
        ("int", "int", "6", "6", "{}", "6"),
        ("list", "List[int]", "[6]", "[6]", "{}", "[6]"),
        (
            "map",
            "Map[str, int]",
            "{'a': 6}",
            r#"{"a": 6}"#,
            "{}",
            "{'a': 6}",
        ),
        (
            "tuple",
            "(int, str)",
            "(6, 'a')",
            r#"[6, "a"]"#,
            "{}",
            "(6, 'a')",
        ),
        ("fn", "fn(int) -> int", "inc", "", "{}(2)", "3"),
        ("ltype", "lib.P", "lib.P(x=6)", r#"{"x": 6}"#, "{}.x", "6"),
        (
            "lgen",
            "lib.Bx[int]",
            "lib.Bx[int].make(6)",
            r#"{"v": 6}"#,
            "{}.v",
            "6",
        ),
        ("opt", "int?", "None", "null", "{}", "None"),
    ];
    let heads = [
        "local", "from", "mod", "full", "json", "abs", "make", "get", "variant", "nullary", "alias",
    ];
    let idt = "fn idt[T](x: T) -> T:\n    return x\n";
    for (sk, x, v, jv, show, want) in shapes {
        for head in heads {
            // (prelude, path value, its type, argument, wrapper around a result before `show`)
            let (pre, f, ft, arg, wrap) = match head {
                "local" => (
                    idt.to_string(),
                    format!("idt[{x}]"),
                    format!("fn({x}) -> {x}"),
                    v.to_string(),
                    "",
                ),
                "from" => (
                    "import idt from lib\n".to_string(),
                    format!("idt[{x}]"),
                    format!("fn({x}) -> {x}"),
                    v.to_string(),
                    "",
                ),
                "mod" => (
                    String::new(),
                    format!("lib.idt[{x}]"),
                    format!("fn({x}) -> {x}"),
                    v.to_string(),
                    "",
                ),
                "full" => (
                    "import a.b\n".to_string(),
                    format!("a.b.idt[{x}]"),
                    format!("fn({x}) -> {x}"),
                    v.to_string(),
                    "",
                ),
                "json" => (
                    String::new(),
                    format!("json.decode[{x}]"),
                    format!("fn(str) -> Result[{x}]"),
                    format!("r'{jv}'"),
                    "okv",
                ),
                "abs" => (
                    String::new(),
                    format!("math.abs[{x}]"),
                    format!("fn({x}) -> {x}"),
                    if sk == "int" {
                        "-6".to_string()
                    } else {
                        v.to_string()
                    },
                    "",
                ),
                "make" => (
                    String::new(),
                    format!("Bx[{x}].make"),
                    format!("fn({x}) -> Bx[{x}]"),
                    v.to_string(),
                    ".v",
                ),
                "alias" => (
                    format!("type BX = Bx[{x}]\n"),
                    "BX.make".to_string(),
                    format!("fn({x}) -> Bx[{x}]"),
                    v.to_string(),
                    ".v",
                ),
                "get" => (
                    String::new(),
                    format!("Bx[{x}].get"),
                    format!("fn(Bx[{x}]) -> {x}"),
                    format!("Bx[{x}](v={v})"),
                    "",
                ),
                "variant" => (
                    String::new(),
                    format!("E[{x}].A"),
                    format!("fn({x}) -> E[{x}]"),
                    v.to_string(),
                    "un",
                ),
                _ => (
                    String::new(),
                    format!("E[{x}].N"),
                    format!("E[{x}]"),
                    String::new(),
                    "",
                ),
            };
            let shr = |r: &str| {
                let w = match wrap {
                    "" => r.to_string(),
                    ".v" => format!("{r}.v"),
                    fun => format!("{fun}({r})"),
                };
                show.replace("{}", &w)
            };
            let name = |p: &str| format!("bracket/{sk}/{head}/{p}");
            let program = |body: String| {
                vec![
                    ("lib.chz", BLIB.to_string()),
                    ("a/b.chz", idt.to_string()),
                    ("main.chz", format!("{BPRE}{pre}{body}\n")),
                ]
            };
            let reject = match (head, sk) {
                ("json", "fn") => Some("decode: cannot decode into fn(int) -> int"),
                ("json", "lgen") => Some("decode: cannot decode into generic struct Bx[int]"),
                ("abs", s) if s != "int" => Some("does not satisfy Num"),
                _ => None,
            };
            if let Some(frag) = reject {
                for (p, body) in [
                    ("call", format!("print({f}({arg}))")),
                    ("let", format!("g := {f}")),
                ] {
                    out.push(cell(name(p), program(body), Expect::Rejects(frag)));
                }
                continue;
            }
            if head == "nullary" {
                let rows = [
                    ("let", format!("g := {f}\nprint(g)")),
                    ("typed", format!("g: {ft} = {f}\nprint(g)")),
                    ("paren", format!("print(({f}))")),
                    (
                        "default",
                        format!("fn k(e: {ft} = {f}) -> str:\n    return '{{e}}'\nprint(k())"),
                    ),
                    (
                        "guard",
                        format!(
                            "match 1:\n    1 if yes({f}):\n        print({f})\n    _:\n        print('no')"
                        ),
                    ),
                ];
                for (p, body) in rows {
                    out.push(cell(name(p), program(body), prints("N")));
                }
                continue;
            }
            let pr = |r: &str| format!("print({})", shr(r));
            let rows = [
                ("call", format!("r := {f}({arg})\n{}", pr("r"))),
                ("let", format!("g := {f}\nr := g({arg})\n{}", pr("r"))),
                (
                    "typed",
                    format!("g: {ft} = {f}\nr := g({arg})\n{}", pr("r")),
                ),
                (
                    "hof",
                    format!(
                        "fn ap[A, B](h: fn(A) -> B, a: A) -> B:\n    return h(a)\nr := ap({f}, {arg})\n{}",
                        pr("r")
                    ),
                ),
                ("paren", format!("r := ({f})({arg})\n{}", pr("r"))),
                ("eq", format!("g := {f}\nprint(g == g)")),
                (
                    "spawn_target",
                    format!(
                        "parallel:\n    spawn {f}({arg})\nr := {f}({arg})\n{}",
                        pr("r")
                    ),
                ),
                (
                    "spawn_capture",
                    format!(
                        "c := Channel[str](1)\nparallel:\n    spawn:\n        y := {f}({arg})\n        c.send('{{{}}}')\nprint(c.recv())",
                        shr("y")
                    ),
                ),
                (
                    "default",
                    format!(
                        "fn k(h: {ft} = {f}) -> str:\n    y := h({arg})\n    return '{{{}}}'\nprint(k())",
                        shr("y")
                    ),
                ),
                (
                    "guard",
                    format!(
                        "match 1:\n    1 if yes({f}):\n        r := {f}({arg})\n        {}\n    _:\n        print('no')",
                        pr("r")
                    ),
                ),
            ];
            for (p, body) in rows {
                // A variant is a constructor, which `spawn` never takes as its target.
                let expect = match p {
                    "spawn_target" if head == "variant" => {
                        Expect::Rejects("spawn requires a function or method call")
                    }
                    "eq" => prints("true"),
                    _ => prints(want),
                };
                out.push(cell(name(p), program(body), expect));
            }
        }
    }
    // A value head keeps the index reading (TICKET-210), a parameter named like a type indexes,
    // and a type-only or multi-type bracket on a value is rejected by name.
    let fs = "fn a(x: int) -> int:\n    return x + 1\nfn b(x: int) -> int:\n    return x * 2\nfs := [a, b]\n";
    let rows: Vec<(&str, String, Expect)> = vec![
        (
            "shadow/local_k",
            format!("{fs}K := 1\nprint(fs[K](10))"),
            prints("20"),
        ),
        (
            "shadow/param_int",
            "fn q(int: List[int]) -> int:\n    return int[0]\nprint(q([4, 5]))".to_string(),
            prints("4"),
        ),
        (
            "shadow/index_forms",
            "xs := [1, 2, 3]\ni := 1\nxs[i] = 9\nprint(xs[i], xs[i + 1], xs[0:2])".to_string(),
            prints("9 3 [1, 9]"),
        ),
        (
            "wrong/type_only_on_value",
            "xs := [1, 2]\nprint(xs[fn(int) -> int])".to_string(),
            Expect::Rejects("a subscript takes an expression, found the type 'fn(int) -> int'"),
        ),
        (
            "wrong/two_types_on_value",
            "xs := [1, 2]\nprint(xs[int, str])".to_string(),
            Expect::Rejects("a subscript takes one index, found 2"),
        ),
        (
            "wrong/value_on_generic",
            format!("{idt}print(idt[1])"),
            Expect::Rejects("'idt' is generic and T is not determined here"),
        ),
    ];
    for (n, src, e) in rows {
        out.push(main_only(n, &src, e));
    }
}

fn cells() -> Vec<Cell> {
    let mut out = Vec::new();
    arity_cells(&mut out);
    owner_cells(&mut out);
    type_head_cells(&mut out);
    neighbour_cells(&mut out);
    path_value_cells(&mut out);
    alias_cells(&mut out);
    native_cells(&mut out);
    qenum_cells(&mut out);
    out
}

#[test]
fn turbofish_value_grid() {
    run_grid("turbofish-value", &cells());
}

#[test]
fn type_arg_bracket_grid() {
    let mut out = Vec::new();
    bracket_grid_cells(&mut out);
    run_grid("type-arg-bracket", &out);
}
