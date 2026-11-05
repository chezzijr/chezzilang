//! Checker tests: declaring several static `from` / `try_from` overloads on one type (Rust's
//! several `impl From<S>` / `impl TryFrom<S>`), and the builtin `int` / `float` conversions. The
//! declaration rules: which repeats are admitted, which overlap, which shapes, one owner per source.

use super::*;

/// A struct `Rat` with `body` as its methods (each line already indented one level).
fn rat(body: &str) -> String {
    format!("struct Rat:\n    n: int\n{body}fn main():\n    print(1)\nmain()\n")
}

/// The whole declaration grid. Each accept cell type-checks clean; each reject cell gives its
/// message, and a cell the overload gate admits never also says "already defined".
#[test]
fn from_overload_declaration_grid() {
    let to_int = "    fn from(x: int) -> Rat:\n        return Rat(x)\n";
    let to_str = "    fn from(x: str) -> Rat:\n        return Rat(x.len())\n";
    let shape = "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s\n";
    let named = "protocol Named:\n    fn name(self) -> str\n";
    let accept: Vec<(&str, String)> = vec![
        (
            "int + str + Rat",
            rat(&format!(
                "{to_int}{to_str}    fn from(x: Rat) -> Rat:\n        return Rat(x.n)\n"
            )),
        ),
        (
            "int + float",
            rat(&format!(
                "{to_int}    fn from(x: float) -> Rat:\n        return Rat(0)\n"
            )),
        ),
        (
            "int + str with the From import",
            format!("import From from std.convert\n{}", rat(&format!("{to_int}{to_str}"))),
        ),
        (
            "try_from overloads returning Rat!str",
            rat("    fn try_from(x: int) -> Rat!str:\n        return Rat(x)\n    fn try_from(x: str) -> Rat!str:\n        return !\"no\"\n"),
        ),
        (
            "a from and a try_from of different sources",
            rat(&format!("{to_int}    fn try_from(x: str) -> Rat!:\n        return Rat(x.to_int()?)\n")),
        ),
        (
            "int + str on an enum",
            "enum E:\n    A\n    B\n    fn from(x: int) -> E:\n        return E.A\n    fn from(x: str) -> E:\n        return E.B\nfn main():\n    print(1)\nmain()\n".to_string(),
        ),
    ];
    let reject: Vec<(&str, String, &str)> = vec![
        (
            "same type twice",
            rat(&format!("{to_int}{to_int}")),
            "conflicting from for int: from(x: int) at 3:8",
        ),
        (
            "an alias of the same type",
            format!(
                "type C = int\n{}",
                rat(&format!(
                    "{to_int}    fn from(x: C) -> Rat:\n        return Rat(x)\n"
                ))
            ),
            "conflicting from for int",
        ),
        (
            "int + int?",
            rat(&format!(
                "{to_int}    fn from(x: int?) -> Rat:\n        return Rat(0)\n"
            )),
            "conflicting from for int",
        ),
        (
            "int? + str?",
            rat("    fn from(x: int?) -> Rat:\n        return Rat(0)\n    fn from(x: str?) -> Rat:\n        return Rat(0)\n"),
            "conflicting from for None: from(x: int?) at 3:8",
        ),
        (
            "a protocol param + a struct that satisfies it",
            format!(
                "{shape}{}",
                rat("    fn from(x: Shape) -> Rat:\n        return Rat(x.area())\n    fn from(x: Sq) -> Rat:\n        return Rat(x.s)\n")
            ),
            "conflicting from for Sq",
        ),
        (
            "two protocol params",
            format!(
                "{shape}{named}{}",
                rat("    fn from(x: Shape) -> Rat:\n        return Rat(0)\n    fn from(x: Named) -> Rat:\n        return Rat(0)\n")
            ),
            "conflicting from for a type that satisfies both Shape and Named",
        ),
        (
            "int!str + int!int (3 wraps into both)",
            rat("    fn from(x: int!str) -> Rat:\n        return Rat(0)\n    fn from(x: int!int) -> Rat:\n        return Rat(1)\n"),
            "conflicting from for int: from(x: int!str) at 3:8",
        ),
        (
            "int? + int!str (3 wraps into both)",
            rat("    fn from(x: int?) -> Rat:\n        return Rat(0)\n    fn from(x: int!str) -> Rat:\n        return Rat(1)\n"),
            "conflicting from for int",
        ),
        (
            "a protocol param + an optional struct that satisfies it",
            format!(
                "{shape}{}",
                rat("    fn from(x: Shape) -> Rat:\n        return Rat(x.area())\n    fn from(x: Sq?) -> Rat:\n        return Rat(0)\n")
            ),
            "conflicting from for Sq",
        ),
        (
            "an overload returning another type",
            rat(&format!("{to_int}    fn from(x: str) -> int:\n        return 5\n")),
            "an overloaded from returns Self, found int",
        ),
        (
            "an overload returning a result of another type",
            rat(&format!("{to_int}    fn from(x: str) -> str!int:\n        return \"a\"\n")),
            "an overloaded from returns Self, found str!int",
        ),
        (
            "a fallible from",
            rat(&format!("{to_int}    fn from(x: str) -> Rat!str:\n        return Rat(0)\n")),
            "an overloaded from returns Self, found Rat!str; a fallible conversion is try_from",
        ),
        (
            "a try_from returning Self",
            rat("    fn try_from(x: int) -> Rat:\n        return Rat(x)\n    fn try_from(x: str) -> Rat!str:\n        return Rat(0)\n"),
            "an overloaded try_from returns Self!E, found Rat",
        ),
        (
            "a from and a try_from of one source",
            rat(&format!("{to_int}    fn try_from(x: int) -> Rat!:\n        return Rat(x)\n")),
            "conflicting conversion int -> Rat: from at 3:8 and try_from at 5:8",
        ),
        (
            "a from and a try_from of overlapping sources",
            rat("    fn from(x: int?) -> Rat:\n        return Rat(0)\n    fn try_from(x: str?) -> Rat!:\n        return Rat(1)\n"),
            "conflicting conversion None -> Rat: from at 3:8 and try_from at 5:8",
        ),
        (
            "a renamed param",
            rat(&format!(
                "{to_int}    fn from(y: str) -> Rat:\n        return Rat(0)\n"
            )),
            "method 'from' parameter 1 is named 'y', but From declares 'x'",
        ),
        (
            "two params",
            rat(&format!(
                "{to_int}    fn from(x: str, k: int) -> Rat:\n        return Rat(k)\n"
            )),
            "an overloaded from takes 1 parameter, as From[S] declares",
        ),
        (
            "an inferred return type",
            rat(&format!("{to_int}    fn from(x: str): Rat(x.len())\n")),
            "an overloaded from declares its return type, as From[S] declares",
        ),
        (
            "a generic param",
            "struct Box[T]:\n    v: int\n    fn from(x: T) -> Box[T]:\n        return Box[T](0)\n    fn from(x: int) -> Box[T]:\n        return Box[T](x)\nfn main():\n    print(1)\nmain()\n".to_string(),
            "an overloaded from takes a concrete type, found T",
        ),
        (
            "each body checks against its own signature",
            rat(&format!(
                "{to_int}    fn from(x: str) -> Rat:\n        return Rat(x)\n"
            )),
            "argument 1 of 'Rat': expected int, found str",
        ),
        (
            "the same type twice on an enum",
            "enum E:\n    A\n    fn from(x: int) -> E:\n        return E.A\n    fn from(x: int) -> E:\n        return E.A\nfn main():\n    print(1)\nmain()\n".to_string(),
            "conflicting from for int",
        ),
        (
            "an instance from twice",
            rat("    fn from(self, x: int) -> Rat:\n        return Rat(x)\n    fn from(self, x: str) -> Rat:\n        return Rat(0)\n"),
            "method 'from' is already defined",
        ),
        (
            "any other method twice",
            rat("    fn show(self) -> int:\n        return 1\n    fn show(self) -> int:\n        return 2\n"),
            "method 'show' is already defined",
        ),
    ];
    let mut failures = Vec::new();
    for (cell, src) in &accept {
        let errs = check_entry(src);
        if !errs.is_empty() {
            failures.push(format!("accept `{cell}`: {errs:?}"));
        }
    }
    for (cell, src, needle) in &reject {
        let errs = check_entry(src);
        if !errs.iter().any(|e| e.message.contains(needle)) {
            failures.push(format!("reject `{cell}`: wanted {needle:?}, got {errs:?}"));
        } else if !needle.contains("already defined")
            && errs.iter().any(|e| e.message.contains("already defined"))
        {
            failures.push(format!(
                "reject `{cell}`: an admitted overload says already defined: {errs:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The direct call `Rat.from(arg)` on an overloaded name: the argument picks the one overload
/// it fits. No overload fits (no widening, Rust `E0277`), or the argument fits more than one
/// (Rust `E0283`), is an error that lists the overloads. Each accept cell type-checks clean.
#[test]
fn from_overload_call_grid() {
    let three = "    fn from(x: int) -> Rat:\n        return Rat(x)\n    fn from(x: str) -> Rat:\n        return Rat(x.len())\n    fn from(x: Rat) -> Rat:\n        return Rat(x.n)\n";
    let lists = "    fn from(x: List[int]) -> Rat:\n        return Rat(0)\n    fn from(x: List[str]) -> Rat:\n        return Rat(1)\n";
    let call = |body: &str, stmt: &str| {
        format!("struct Rat:\n    n: int\n{body}fn main():\n    {stmt}\nmain()\n")
    };
    let accept: Vec<(&str, String)> = vec![
        ("an int", call(three, "r := Rat.from(3)")),
        ("a str", call(three, "r := Rat.from(\"ab\")")),
        ("a Rat", call(three, "r := Rat.from(Rat(1))")),
        ("an annotated result", call(three, "r: Rat = Rat.from(3)")),
        ("a List[int]", call(lists, "r := Rat.from([1])")),
        (
            "an annotated value",
            call(three, "f: fn(str) -> Rat = Rat.from"),
        ),
    ];
    let builtin = |stmt: &str| format!("fn main():\n    {stmt}\nmain()\n");
    let accept_builtin: Vec<(&str, String)> = vec![
        ("int.from a bool", builtin("n: int = int.from(true)")),
        ("float.from a bool", builtin("x: float = float.from(false)")),
        (
            "int.try_from a str",
            builtin("n: int! = int.try_from(\"5\")"),
        ),
        (
            "float.try_from a str",
            builtin("x: float! = float.try_from(\"1.5\")"),
        ),
        (
            "int.from as a value",
            builtin("f: fn(bool) -> int = int.from"),
        ),
    ];
    let reject: Vec<(&str, String, &str)> = vec![
        (
            "float.from an int (lossy past 2^53, rustc E0277)",
            builtin("x := float.from(3)"),
            "argument 1 of 'from': expected bool, found int",
        ),
        (
            "int.from a str (a parse is try_from)",
            builtin("n := int.from(\"5\")"),
            "argument 1 of 'from': expected bool, found str",
        ),
        (
            "a float (no widening)",
            call(three, "r := Rat.from(1.5)"),
            "type Rat has no from for float (it has: int, str, Rat)",
        ),
        (
            "an argument that fits two overloads",
            call(lists, "r := Rat.from([])"),
            "type annotations needed: Rat.from has 2 overloads that take List[?] (List[int], List[str])",
        ),
        (
            "no argument",
            call(three, "r := Rat.from()"),
            "an overloaded from takes 1 argument, found 0",
        ),
        (
            "an unknown keyword",
            call(three, "r := Rat.from(y=3)"),
            "unknown named argument 'y' (its parameters are: x)",
        ),
        (
            "a value with no annotation",
            call(three, "g := Rat.from"),
            "type annotations needed: Rat.from has 3 overloads (int, str, Rat)",
        ),
        (
            "a value annotated with a source no overload takes",
            call(three, "g: fn(float) -> Rat = Rat.from"),
            "type Rat has no from for float (it has: int, str, Rat)",
        ),
    ];
    let mut failures = Vec::new();
    for (cell, src) in accept.iter().chain(&accept_builtin) {
        let errs = check_entry(src);
        if !errs.is_empty() {
            failures.push(format!("accept `{cell}`: {errs:?}"));
        }
    }
    for (cell, src, needle) in &reject {
        let errs = check_entry(src);
        if !errs.iter().any(|e| e.message.contains(needle)) {
            failures.push(format!("reject `{cell}`: wanted {needle:?}, got {errs:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A bound call `make(src)` on an overloaded `T` (`fn make[S, T: From[S]](x: S) -> T`) whose
/// source still has holes (`[]`, `None`): conformance and the witness read the one pick, the
/// direct call's rule, so the verdict never depends on the declaration order. Each cell checks
/// both orders of its two overloads.
#[test]
fn from_overload_bound_pick_ignores_declaration_order() {
    let src = |a: &str, b: &str, arg: &str| {
        format!(
            "import From from std.convert\nstruct Rat:\n    n: int\n    fn from(x: {a}) -> Rat:\n        return Rat(0)\n    fn from(x: {b}) -> Rat:\n        return Rat(1)\nfn make[S, T: From[S]](x: S) -> T:\n    return T.from(x)\nfn main():\n    r: Rat = make({arg})\n    print(r.n)\nmain()\n"
        )
    };
    let accept = [("str", "List[int]", "[]"), ("str", "int?", "None")];
    let reject = [(
        "List[int]",
        "List[str]",
        "[]",
        "type annotations needed: Rat.from has 2 overloads that take List[?]",
    )];
    let mut failures = Vec::new();
    for (a, b, arg) in accept {
        for (x, y) in [(a, b), (b, a)] {
            let errs = check_entry(&src(x, y, arg));
            if !errs.is_empty() {
                failures.push(format!("accept `{x} + {y}` make({arg}): {errs:?}"));
            }
        }
    }
    for (a, b, arg, needle) in reject {
        for (x, y) in [(a, b), (b, a)] {
            let errs = check_entry(&src(x, y, arg));
            if !errs.iter().any(|e| e.message.contains(needle)) {
                failures.push(format!(
                    "reject `{x} + {y}` make({arg}): wanted {needle:?}, got {errs:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The instance conversion `fn into(self) -> T` / `fn try_into(self) -> T!` (Rust `impl Into<T>`,
/// `impl TryInto<T>`), overloaded by the return type and picked by `r.into[T]()`, and the one
/// owner per conversion pair (Rust `E0119`): at most one of `B.from(A)`, `B.try_from(A)`,
/// `A.into -> B`, `A.try_into -> B!`. The program is `Rat` (methods `rat`), then `Big` (methods
/// `big`), then `main` running `stmt`. Each accept cell type-checks clean; each reject cell gives
/// its message, and a cell the overload gate admits never also says "already defined".
#[test]
fn into_overload_grid() {
    let src = |rat: &str, big: &str, stmt: &str| {
        format!(
            "struct Rat:\n    n: int\n{rat}struct Big:\n    n: int\n{big}fn main():\n    r := Rat(1)\n    {stmt}\nmain()\n"
        )
    };
    let to_float = "    fn into(self) -> float:\n        return 0.5\n";
    let to_str = "    fn into(self) -> str:\n        return \"r\"\n";
    let to_int = "    fn try_into(self) -> int!:\n        return self.n\n";
    let to_big = "    fn into(self) -> Big:\n        return Big(self.n)\n";
    let try_to_big = "    fn try_into(self) -> Big!:\n        return Big(self.n)\n";
    let big_from = "    fn from(x: Rat) -> Big:\n        return Big(x.n)\n";
    let big_try_from = "    fn try_from(x: Rat) -> Big!:\n        return Big(x.n)\n";
    let three = format!("{to_float}{to_str}{to_int}");
    let accept: Vec<(&str, String)> = vec![
        (
            "into float + into str + try_into int",
            src(
                &three,
                "",
                "f: float = r.into[float]()\n    s: str = r.into[str]()\n    n: int = r.try_into[int]() ?? 0",
            ),
        ),
        ("one into", src(to_float, "", "f: float = r.into[float]()")),
        // A single `into` is a plain method (2+ decls make an overload set), as a single `from`.
        ("one into, no [T]", src(to_float, "", "f: float = r.into()")),
        (
            "one into through the type",
            src(to_float, "", "f: float = Rat.into[float](r)"),
        ),
        ("one into as a value", src(to_float, "", "xs := [r].map(Rat.into)")),
        (
            "one into takes a parameter",
            src("    fn into(self, k: int) -> int:\n        return self.n + k\n", "", "n: int = r.into(2)"),
        ),
        (
            "one into returns T!",
            src("    fn into(self) -> float!:\n        return 0.5\n", "", "f: float = r.into() ?? 0.0"),
        ),
        (
            "one into satisfies a user protocol",
            "protocol Conv:\n    fn into(self) -> int\nstruct A:\n    n: int\n    fn into(self) -> int:\n        return self.n\nfn use(c: Conv) -> int:\n    return c.into()\nfn main():\n    print(use(A(4)))\nmain()\n".to_string(),
        ),
        (
            "an enum into",
            "enum E:\n    A\n    fn into(self) -> int:\n        return 1\nfn main():\n    n: int = E.A.into[int]()\nmain()\n".to_string(),
        ),
        (
            "a static into is a plain method",
            src("    fn into(x: int) -> Rat:\n        return Rat(x)\n", "", "q := Rat.into(3)"),
        ),
        (
            "Rat into Big beside Big from int (two pairs)",
            src(to_big, "    fn from(x: int) -> Big:\n        return Big(x)\n", "b: Big = r.into[Big]()"),
        ),
        (
            "Rat into float beside Rat from float (two directions)",
            src(&format!("{to_float}    fn from(x: float) -> Rat:\n        return Rat(0)\n"), "", "f: float = r.into[float]()"),
        ),
    ];
    let reject: Vec<(&str, String, &str)> = vec![
        (
            "no target type",
            src(&three, "", "f := r.into()"),
            "into on Rat needs a target type: r.into[T]()",
        ),
        (
            "no overload for the target",
            src(&three, "", "n := r.into[int]()"),
            "type Rat has no into for int (it has: float, str)",
        ),
        (
            "an argument",
            src(&three, "", "f := r.into[float](3)"),
            "an overloaded into takes no argument, found 1",
        ),
        (
            "the pair is Big's from",
            src(to_float, big_from, "b := r.into[Big]()"),
            "type Rat has no into for Big (it has: float); Big defines it: use Big.from(r)",
        ),
        (
            "the pair is Big's try_from, and Rat has no into",
            src("", big_try_from, "b := r.try_into[Big]()"),
            "type Rat has no try_into for Big; Big defines it: use Big.try_from(r)",
        ),
        (
            "two into -> float",
            src(&format!("{to_float}{to_float}"), "", "f := 1"),
            "conflicting into for float: into(self) -> float at 3:8",
        ),
        (
            "into takes a parameter",
            src(&format!("{to_float}    fn into(self, y: int) -> str:\n        return \"\"\n"), "", "f := 1"),
            "an overloaded into takes no parameter but self, as Into[T] declares",
        ),
        (
            "into returns T!",
            src(&format!("{to_str}    fn into(self) -> float!:\n        return 0.5\n"), "", "f := 1"),
            "an overloaded into returns T, found float!; a fallible conversion is try_into",
        ),
        (
            "try_into returns T",
            src("    fn try_into(self) -> int:\n        return 1\n    fn try_into(self) -> str!:\n        return \"\"\n", "", "f := 1"),
            "an overloaded try_into returns T!E, found int",
        ),
        (
            "into returns the struct's own type param",
            "struct Box[T]:\n    v: T\n    fn into(self) -> T:\n        return self.v\n    fn into(self) -> str:\n        return \"\"\nfn main():\n    print(1)\nmain()\n".to_string(),
            "an overloaded into returns a concrete type, found T",
        ),
        (
            "Rat.into -> Big and Big.from(Rat)",
            src(to_big, big_from, "f := 1"),
            "conflicting conversion Rat -> Big: into at 3:8 and from at 7:8",
        ),
        (
            "Rat.into -> Big and Big.try_from(Rat)",
            src(to_big, big_try_from, "f := 1"),
            "conflicting conversion Rat -> Big: into at 3:8 and try_from at 7:8",
        ),
        (
            "Rat.try_into -> Big! and Big.from(Rat)",
            src(try_to_big, big_from, "f := 1"),
            "conflicting conversion Rat -> Big: try_into at 3:8 and from at 7:8",
        ),
        (
            "into -> float and try_into -> float! on one type",
            src(&format!("{to_float}    fn try_into(self) -> float!:\n        return 0.5\n"), "", "f := 1"),
            "conflicting conversion Rat -> float: into at 3:8 and try_into at 5:8",
        ),
        (
            "one into, another target",
            src(to_float, "", "n := r.into[int]()"),
            "type Rat has no into for int (it has: float)",
        ),
        (
            "a type param target",
            format!("struct Rat:\n    n: int\n{three}fn conv[T](r: Rat) -> T:\n    return r.into[T]()\n"),
            "type Rat has no into for T (it has: float, str)",
        ),
        (
            "two static into are no overloads",
            src("    fn into(x: int) -> Rat:\n        return Rat(x)\n    fn into(x: str) -> Rat:\n        return Rat(0)\n", "", "f := 1"),
            "method 'into' is already defined",
        ),
    ];
    let mut failures = Vec::new();
    for (cell, src) in &accept {
        let errs = check_entry(src);
        if !errs.is_empty() {
            failures.push(format!("accept `{cell}`: {errs:?}"));
        }
    }
    for (cell, src, needle) in &reject {
        let errs = check_entry(src);
        if !errs.iter().any(|e| e.message.contains(needle)) {
            failures.push(format!("reject `{cell}`: wanted {needle:?}, got {errs:?}"));
        } else if errs.len() != 1 {
            failures.push(format!("reject `{cell}`: wanted one error, got {errs:?}"));
        } else if !needle.contains("already defined")
            && errs.iter().any(|e| e.message.contains("already defined"))
        {
            failures.push(format!(
                "reject `{cell}`: an admitted overload says already defined: {errs:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One owner per conversion pair across modules (Rust `E0119`): `B.from(x: P)` in an imported
/// module owns the pair `A -> B` for an `A` that satisfies `P`, so `A`'s `into -> B` conflicts.
/// The importing module, which names both types, reports it.
#[test]
fn into_conflicts_with_an_imported_from() {
    let zb = "protocol P:\n    fn n(self) -> int\nstruct B:\n    m: int\n    fn from(x: P) -> B:\n        return B(x.n())\n";
    let a = |into: &str| {
        format!(
            "import B, P from zb\nstruct A:\n    k: int\n    fn n(self) -> int:\n        return self.k\n{into}a := A(5)\nprint(B.from(a))\n"
        )
    };
    let to_b = "    fn into(self) -> B:\n        return B(-1)\n";
    let try_to_b = "    fn try_into(self) -> B!:\n        return B(-1)\n";
    for (into, name) in [(to_b, "into"), (try_to_b, "try_into")] {
        let errs = check_files(&[("zb.chz", zb), ("main.chz", &a(into))]);
        let needle = format!("conflicting conversion A -> B: {name} at 6:8 and B.from");
        assert!(
            errs.len() == 1 && errs[0].message.contains(&needle),
            "{name}: wanted one {needle:?}, got {errs:?}"
        );
    }
    // A type that does not satisfy `P` converts by its own `into`.
    let other = "import B from zb\nstruct C:\n    k: int\n    fn into(self) -> B:\n        return B(1)\nprint(C(1).into[B]())\n";
    files_ok(&[("zb.chz", zb), ("main.chz", other)]);
}
