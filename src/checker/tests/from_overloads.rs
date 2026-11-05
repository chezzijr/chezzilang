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
