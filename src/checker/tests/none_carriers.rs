//! Checker tests: the `none` word, imported constants in patterns, ret/err value grids, variant
//! patterns, later pins, present-value compares.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

#[cfg(test)]
mod ticket_231_none_word {
    use super::*;

    #[test]
    fn mismatch_message_prints_option_sugar() {
        rejects("fn main():\n    x: int? = \"a\"\n", "int?");
    }

    /// Every carrier shape, on the checker-message surface: the message prints the sugar the
    /// user writes, never the long form.
    #[test]
    fn display_grid_prints_the_sugar() {
        let pairs = [
            ("int?", "int?"),
            ("int!str", "int!str"),
            ("int!", "int!"),
            ("None!str", "!str"),
            ("List[int?]", "List[int?]"),
            ("Map[str, int!str]", "Map[str, int!str]"),
            ("int??", "int??"),
            ("(int!str)?", "(int!str)?"),
            ("(int!)?", "int!?"),
            ("int?!str", "int?!str"),
            ("(fn() -> int)?", "(fn() -> int)?"),
            ("(int!str)!int", "(int!str)!int"),
        ];
        let mut red = Vec::new();
        for (written, want) in pairs {
            let errs = check_src(&format!("x: {written} = true\n"));
            let needle = format!("variable of type {want}");
            let long = |m: &str| m.contains("Option[") || m.contains("Result[");
            if !errs.iter().any(|e| e.message.contains(&needle))
                || errs.iter().any(|e| long(&e.message))
            {
                red.push(format!("{written} -> want {needle:?}, got {errs:?}"));
            }
        }
        let errs = check_src("fn f() -> None:\n    pass\nx := f()\nprint(x)\n");
        if !errs
            .iter()
            .any(|e| e.message.contains("returns no value (None)"))
        {
            red.push(format!(
                "void use -> want \"returns no value (None)\", got {errs:?}"
            ));
        }
        assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
    }

    /// D1: `None` is the one "nothing" word. As a type it is a return annotation only; as a value
    /// it is the absent `T?`; `nil` is an ordinary identifier.
    #[test]
    fn none_word_grid() {
        const LOG: &str = "fn log(m: str) -> None:\n    print(m)\n";
        let log_call = format!("{LOG}log(\"a\")\n");
        let log_bound = format!("{LOG}x := log(\"a\")\n");
        let cells: [(&str, Option<&str>); 11] = [
            (&log_call, None),
            (&log_bound, Some("returns no value (None)")),
            ("x: None? = 5\n", Some("'None?' is not a type")),
            ("x: None? = 5\n", Some("'None?' is not a type")),
            ("type N = None\nx: N? = 5\n", Some("'None?' is not a type")),
            ("nil := 5\nprint(nil)\n", None),
            ("fn nil() -> int:\n    return 1\nprint(nil())\n", None),
            ("fn f() -> nil:\n    pass\n", Some("unknown type 'nil'")),
            ("fn f() -> None!str:\n    return\nprint(f())\n", None),
            ("fn f() -> None!str:\n    return\nprint(f())\n", None),
            ("x: int? = None\nprint(x)\n", None),
        ];
        // `None` is a keyword (TICKET-237): `struct None` no longer reaches the checker.
        parse_rejects("struct None:\n    a: int\n", "reserved keyword");
        let mut red = Vec::new();
        for (src, want) in cells {
            let errs = check_src(src);
            let pass = match want {
                None => errs.is_empty(),
                Some(n) => errs.iter().any(|e| e.message.contains(n)),
            };
            if !pass {
                red.push(format!("{src:?} -> want {want:?}, got {errs:?}"));
            }
        }
        // The void value prints `None`, as Python's does.
        let out = crate::compiler::run_for_tests::run_capture(
            "fn f() -> None!str:\n    return\nprint(f())\n",
        );
        if out.as_deref().ok() != Some("None\n") {
            red.push(format!("void print -> want \"None\", got {out:?}"));
        }
        assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
    }
}

#[test]
fn bare_constant_in_pattern_rejected() {
    rejects(
        "LIMIT: const int = 3\nfn main():\n    match 5:\n        LIMIT:\n            print(\"hit\")\n        _:\n            print(\"miss\")\n",
        "`LIMIT` is a constant",
    );
}

/// TICKET-228 (D5) — the whole grid of "a bare, payload-free name in a pattern": every scrutinee
/// kind x every thing the name can be. A constant is rejected at every position; a fresh name and
/// a local that shadows a constant bind (and the program RUNS); a bare unimported variant stays
/// rejected with its own text.
#[test]
fn pattern_bare_name_grid() {
    // (label, prelude, the match with `@` for the name, expected stdout when the name binds)
    let scrutinees: [(&str, &str, &str, &str); 6] = [
        (
            "int",
            "",
            "    match 5:\n        @:\n            print(1)\n",
            "1\n",
        ),
        (
            "tuple",
            "",
            "    match (1, 2):\n        (@, b):\n            print(b)\n",
            "2\n",
        ),
        (
            "enum payload",
            "enum E:\n    A(int)\n    B\n",
            "    match E.A(1):\n        E.A(@):\n            print(1)\n        E.B:\n            print(0)\n",
            "1\n",
        ),
        (
            "struct field",
            "struct P:\n    x: int\n    y: int\n",
            "    match P(1, 2):\n        P(@, y):\n            print(y)\n",
            "2\n",
        ),
        (
            "T?",
            "fn g() -> int?:\n    return 1\n",
            "    match g():\n        ?(@):\n            print(1)\n        None:\n            print(0)\n",
            "1\n",
        ),
        (
            "un-inferable",
            "fn u(x):\n    match x:\n        @:\n            print(1)\n",
            "    u(5)\n",
            "1\n",
        ),
    ];
    let consts = |errs: &[CheckError]| {
        errs.iter()
            .filter(|e| e.message.contains("is a constant"))
            .count()
    };
    for (label, prelude, body, out) in scrutinees {
        // A fresh name binds, and the program runs.
        let fresh = format!("{prelude}fn main():\n{body}main()\n").replace('@', "n");
        // An un-inferable scrutinee needs an untyped parameter, which is its own error: that row
        // counts the constant errors only.
        let inferable = label != "un-inferable";
        if inferable {
            ok(&fresh);
            assert_eq!(
                crate::compiler::run_for_tests::run_capture(&fresh).expect("runs"),
                out,
                "{label}: fresh name"
            );
        } else {
            assert_eq!(consts(&check_src(&fresh)), 0, "{label}: fresh name");
        }
        // A local variable that shadows a global constant is a new binder (un-inferable: the
        // shadow lives in `u`, where the match is).
        let shadow = format!(
            "LIMIT: const int = 3\n{}fn main():\n    LIMIT := 9\n    print(LIMIT)\n{body}main()\n",
            prelude.replace(
                "    match x:",
                "    LIMIT := 9\n    print(LIMIT)\n    match x:"
            )
        )
        .replace('@', "LIMIT");
        let errs = check_src(&shadow);
        assert_eq!(consts(&errs), 0, "{label}: shadowing local, got {errs:?}");
        // A global constant is rejected, once, and it is the only error.
        let global = format!("LIMIT: const int = 3\n{prelude}fn main():\n{body}main()\n")
            .replace('@', "LIMIT");
        let errs = check_src(&global);
        assert_eq!(consts(&errs), 1, "{label}: global const, got {errs:?}");
        if inferable {
            assert_eq!(errs.len(), 1, "{label}: global const, got {errs:?}");
        }
        assert!(
            errs.iter().any(|e| e.message.contains(
                "`LIMIT` is a constant; to compare write `x if x == LIMIT`, to bind use a new name"
            )),
            "{label}: {errs:?}"
        );
        // A local constant, same rule.
        let local = format!(
            "{}fn main():\n    L2: const int = 4\n{body}main()\n",
            prelude.replace("    match x:", "    L2: const int = 4\n    match x:")
        )
        .replace('@', "L2");
        let errs = check_src(&local);
        assert_eq!(consts(&errs), 1, "{label}: local const, got {errs:?}");
        // A bare unimported variant is never a binder and never "a constant".
        let variant = format!("enum C:\n    Red\n    Green\n{prelude}fn main():\n{body}main()\n")
            .replace('@', "Red");
        let errs = check_src(&variant);
        assert!(!errs.is_empty(), "{label}: bare variant must be rejected");
        assert_eq!(consts(&errs), 0, "{label}: bare variant, got {errs:?}");
    }
    // A constant inside an or-alternative, and the same constant twice in one tuple pattern.
    let errs = check_src(
        "LIMIT: const int = 3\nfn main():\n    match 5:\n        LIMIT | 4:\n            print(1)\n        _:\n            print(2)\n",
    );
    assert_eq!(consts(&errs), 1, "or-alternative, got {errs:?}");
    let errs = check_src(
        "LIMIT: const int = 3\nfn main():\n    match (1, 2):\n        (LIMIT, LIMIT):\n            print(1)\n",
    );
    assert!(consts(&errs) >= 1, "const twice, got {errs:?}");
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("bound more than once")),
        "a constant is not a binder: {errs:?}"
    );
}

/// TICKET-228 (D5) — a constant IMPORTED from another module is rejected in a pattern too.
#[test]
fn imported_constant_in_pattern_rejected() {
    let t = TmpDir::new();
    t.write("lib.chz", "LIMIT: const int = 3\n");
    let entry = t.write(
        "main.chz",
        "import LIMIT from lib\nfn main():\n    match 5:\n        LIMIT:\n            print(1)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = check_graph(&graph).err().unwrap_or_default();
    assert!(
        errs.iter()
            .any(|e| e.message.contains("`LIMIT` is a constant")),
        "got: {errs:?}"
    );
}

/// TICKET-228 — an exhaustiveness witness prints the pattern the user would write.
#[test]
fn witness_prints_pattern_syntax() {
    rejects_help(
        "fn o() -> int?:\n    return 1\nfn main():\n    match o():\n        ?v:\n            print(v)\n",
        "non-exhaustive match on int?: missing None",
        "pattern `None` is not covered",
    );
    rejects_help(
        "fn r() -> int!str:\n    return 1\nfn main():\n    match r():\n        ?v:\n            print(v)\n",
        "non-exhaustive match on int!str: missing !_",
        "pattern `!_` is not covered",
    );
    rejects_help(
        "fn r() -> int!str:\n    return 1\nfn main():\n    match r():\n        !e:\n            print(e)\n",
        "non-exhaustive match on int!str: missing ?_",
        "pattern `?_` is not covered",
    );
    rejects_help(
        "fn oo() -> int??:\n    return None\nfn main():\n    match oo():\n        ?(?v):\n            print(v)\n        None:\n            print(0)\n",
        "non-exhaustive match",
        "pattern `?None` is not covered",
    );
    rejects_help(
        "fn oo() -> int??:\n    return None\nfn main():\n    match oo():\n        ?None:\n            print(1)\n        None:\n            print(0)\n",
        "non-exhaustive match",
        "pattern `?(?_)` is not covered",
    );
}

/// A 1-tuple witness prints with its comma (S2-7): `(false,)`, never `(false)`, which reads as
/// `false`.
#[test]
fn a_one_tuple_witness_prints_with_its_comma() {
    rejects_help(
        "t := (true,)\nmatch t:\n    (true,):\n        print(1)\n",
        "non-exhaustive match",
        "pattern `(false,)` is not covered",
    );
}

/// TICKET-228 — the three `?`-outside-a-carrier texts name the types by their spelling.
#[test]
fn try_messages_name_no_removed_spelling() {
    let cells = [
        (
            "fn g() -> int?:\n    return 1\nstruct H:\n    n: int = g()?\nfn main():\n    print(H().n)\n",
            "use `??` or produce a `T?` value",
        ),
        (
            "fn g() -> int?:\n    return 1\nfn main():\n    f := fn(n: int): g()? + n\n    print(f(1))\n",
            "declare it to return a `T?` or `T!E` value (e.g. `-> int?`)",
        ),
        (
            "fn g() -> int?:\n    return 1\nfn f() -> int!:\n    x := g()?\n    return x\n",
            "'?' propagates a None, but the enclosing function returns a `T!E` value, not a `T?` value",
        ),
    ];
    for (i, (src, want)) in cells.into_iter().enumerate() {
        // Both funnels run `assert_no_removed_spelling` over every error. A default expression
        // is judged in its desugared provider.
        let errs = if i == 0 {
            check_desugared(src)
        } else {
            check_src(src)
        };
        assert!(
            errs.iter().any(|e| e.message.contains(want)),
            "expected {want:?}, got: {errs:?}"
        );
    }
}

/// TICKET-228 — a warning or an error about a carrier prints its type, never an enum key.
#[test]
fn carrier_messages_name_no_removed_spelling() {
    let (errs, warns) = warn_src("xs: List[int] = [1]\nxs.pop()\n");
    assert!(errs.is_empty(), "{errs:?}");
    assert!(
        warns.iter().any(|w| w
            .message
            .contains("the `int?` value returned by 'pop' is discarded")),
        "{warns:?}"
    );
    let (errs, _) = warn_src(
        "fn f(x: int?):\n    match x:\n        Foo(v):\n            print(v)\n        _:\n            print(0)\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("'Foo' is not a variant of int?")),
        "{errs:?}"
    );
    let (errs, _) = warn_src(
        "enum Color:\n    Red\n    Green\nfn f(x: int?):\n    match x:\n        Color.Red:\n            print(1)\n        _:\n            print(0)\n",
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("variant 'Color.Red' cannot match a value of type int?")),
        "{errs:?}"
    );
}

/// TICKET-228 — the prefix type `!E` is `None!E` (and `!` is `None!`): one type in every
/// position, printed in the short form.
#[test]
fn prefix_error_type_grid() {
    for (short, long) in [("!str", "None!str"), ("!", "None!")] {
        // One value flows between the two spellings in each position: equal types, no error.
        let positions = [
            format!("fn a() -> {short}:\n    return\nfn b() -> {long}:\n    return a()\n"),
            format!("fn a() -> {long}:\n    return\nfn b() -> {short}:\n    return a()\n"),
            format!("fn a(x: {short}):\n    print(1)\nfn b(y: {long}):\n    a(y)\n"),
            format!("fn a(x: List[{short}]):\n    print(1)\nfn b(y: List[{long}]):\n    a(y)\n"),
            format!(
                "fn a(f: fn() -> {short}):\n    print(1)\nfn b(g: fn() -> {long}):\n    a(g)\n"
            ),
            format!(
                "type R = {short}\nfn a() -> R:\n    return\nfn b() -> {long}:\n    return a()\n"
            ),
        ];
        for src in &positions {
            ok(src);
        }
        // A mismatch prints the short form, whichever spelling the source used.
        for spelled in [short, long] {
            let src = format!(
                "fn a() -> {spelled}:\n    return\nfn main():\n    x: int = a()\n    print(x)\n"
            );
            let errs = check_src(&src);
            assert!(
                errs.iter()
                    .any(|e| e.message.contains(&format!("cannot assign {short} to"))),
                "{spelled}: {errs:?}"
            );
            assert!(
                !errs.iter().any(|e| e.message.contains("None!")),
                "{spelled}: {errs:?}"
            );
        }
    }
}

/// TICKET-228 — `None` as a TYPE means "returns nothing": legal as a return type and as the
/// success side of `None!E`, rejected in every value position. The whole position grid.
#[test]
fn none_type_position_grid() {
    const MSG: &str = "'None' is not a value type";
    // Value positions: exactly one error, and it is this one.
    let reject_once = [
        ("List", "fn f(x: List[None]):\n    print(1)\n"),
        ("Map value", "x: Map[str, None] = {}\n"),
        ("Channel", "fn f(c: Channel[None]):\n    print(1)\n"),
        (
            "Iterator",
            "import Iterator from std.iter\nfn f(c: Iterator[None]):\n    print(1)\n",
        ),
        ("tuple element", "fn f(x: (int, None)):\n    print(1)\n"),
        (
            "fn-type parameter",
            "fn f(g: fn(None) -> int):\n    print(1)\n",
        ),
        ("variant payload", "enum E:\n    A(None)\n    B\n"),
        ("struct field", "struct S:\n    x: None\n"),
        ("parameter", "fn f(x: None):\n    print(1)\n"),
        (
            "error side of T!E",
            "fn f(g: fn() -> int!None):\n    print(1)\n",
        ),
        (
            "alias of None as an element",
            "type V = None\nfn f(x: List[V]):\n    print(1)\n",
        ),
        (
            "alias of None as a parameter",
            "type V = None\nfn f(x: V):\n    print(1)\n",
        ),
        (
            "alias body, at its use",
            "type V = List[None]\nfn f(x: V):\n    print(1)\n",
        ),
    ];
    for (label, src) in reject_once {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "{label}: {errs:?}");
        assert!(errs[0].message.contains(MSG), "{label}: {errs:?}");
    }
    // Value positions where the unresolved type may add a follow-on error: this one is among them.
    for (label, src) in [
        ("let annotation", "fn g():\n    print(1)\nx: None = g()\n"),
        (
            "turbofish",
            "fn id[T](x: T) -> T:\n    return x\nfn main():\n    print(id[None](1))\n",
        ),
        ("Set", "fn f(x: Set[None]):\n    print(1)\n"),
    ] {
        rejects(src, MSG);
        let _ = label;
    }
    // Every return-type holder, the success side of a carrier, and positions that spell no
    // value type.
    for (label, src) in [
        ("top-level fn", "fn f() -> None:\n    print(1)\n"),
        (
            "method",
            "struct S:\n    x: int\n    fn m(self) -> None:\n        print(self.x)\n",
        ),
        (
            "protocol requirement",
            "protocol P:\n    fn m(self) -> None\n",
        ),
        (
            "nested fn",
            "fn f():\n    fn g() -> None:\n        print(1)\n    g()\n",
        ),
        (
            "extern fn",
            "extern \"libc.so.6\":\n    fn srand(seed: int) -> None\n\nsrand(1)\n",
        ),
        ("lambda", "h := fn() -> None: print(1)\nh()\n"),
        (
            "lambda against an expected fn type",
            "fn run(g: fn() -> None):\n    g()\nrun(fn() -> None: print(1))\n",
        ),
        ("None!E return", "fn f() -> None!str:\n    return\n"),
        ("prefix !E return", "fn f() -> !str:\n    return\n"),
        ("fn-type return", "fn run(g: fn() -> None):\n    g()\n"),
        (
            "alias of None as a return",
            "type V = None\nfn f() -> V:\n    print(1)\n",
        ),
        ("unused alias body", "type V = List[None]\nprint(1)\n"),
        (
            "None!E as an element",
            "fn f(x: List[None!str]):\n    print(1)\n",
        ),
        ("None!E as a parameter", "fn f(x: !str):\n    print(1)\n"),
    ] {
        let errs = check_src(src);
        assert!(errs.is_empty(), "{label}: {errs:?}");
    }
    // DEC-231's own reject keeps its own text.
    rejects("x: None? = 5\n", "'None?' is not a type");
}

/// TICKET-228 — an unknown return type reports once.
#[test]
fn unknown_return_type_reports_once() {
    let errs = check_src("fn f() -> nil:\n    return\n");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains("unknown type 'nil'"), "{errs:?}");
    assert_eq!(errs[0].help, None, "{errs:?}");
    // A genuine near miss keeps the suggester's own help.
    let errs = check_src("fn f() -> Eror:\n    return\n");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(
        errs[0].help.as_deref(),
        Some("did you mean 'Error'?"),
        "{errs:?}"
    );
}

/// TICKET-228 (step 13) — where a grid fn lives. `@name(` in a body or a use line is the call of a
/// sibling grid fn, spelled for the holder.
#[derive(Clone, Copy, PartialEq, Debug)]
enum RetHolder {
    Top,
    Nested,
    Method,
    Static,
}

/// One grid fn: (name, params, body, inline). A block body is a list of lines split on `\n`.
type RetFn<'a> = (&'a str, &'a str, &'a str, bool);

fn ret_grid_program(h: RetHolder, fns: &[RetFn], uses: &str) -> String {
    let (pad, body_call, use_call) = match h {
        RetHolder::Top => ("", "", ""),
        RetHolder::Nested => ("    ", "", ""),
        RetHolder::Method => ("    ", "self.", "S(1)."),
        RetHolder::Static => ("    ", "S.", "S."),
    };
    let mut out = String::from("fn g() -> int!str:\n    return 1\n");
    match h {
        RetHolder::Top => {}
        RetHolder::Nested => out.push_str("fn outer():\n"),
        RetHolder::Method | RetHolder::Static => out.push_str("struct S:\n    v: int\n"),
    }
    for (name, params, body, inline) in fns {
        let params = match (h, params.is_empty()) {
            (RetHolder::Method, true) => "self".to_string(),
            (RetHolder::Method, false) => format!("self, {params}"),
            _ => params.to_string(),
        };
        let body = body.replace('@', body_call);
        if *inline {
            out.push_str(&format!("{pad}fn {name}({params}): {body}\n"));
        } else {
            out.push_str(&format!("{pad}fn {name}({params}):\n"));
            for line in body.split('\n') {
                out.push_str(&format!("{pad}    {line}\n"));
            }
        }
    }
    let use_pad = if h == RetHolder::Nested { "    " } else { "" };
    for line in uses.split('\n').filter(|l| !l.is_empty()) {
        out.push_str(&format!("{use_pad}{}\n", line.replace('@', use_call)));
    }
    if h == RetHolder::Nested {
        out.push_str("outer()\n");
    }
    out
}

/// TICKET-228 (owner, 2026-10-09) — a named fn's return type is written. A BLOCK body with no `->`
/// returns nothing, and a value `return` in it is an error that prints the `->` to add. An INLINE
/// expression body takes the type of its one expression, as a lambda does, and only from a walk in
/// which every call had a type: a recursive inline fn asks for `->`. Nothing is joined over
/// `return` statements. Every accept cell also RUNS.
#[test]
fn named_fn_return_grid() {
    const NO_RET: &str = "declares no return type, so it returns nothing";
    const CYCLE: &str = "a call in its body has no known type yet";
    let msgs =
        |src: &str| -> Vec<String> { check_src(src).into_iter().map(|e| e.message).collect() };
    let one = |src: &str, needle: &str| {
        let m = msgs(src);
        assert!(
            m.len() == 1 && m[0].contains(needle),
            "want one error holding {needle:?}, got {m:?}\n{src}"
        );
    };
    let runs = |src: &str, want: &str| {
        ok(src);
        assert_eq!(
            crate::compiler::run_for_tests::run_capture(src).expect("accept cell runs"),
            want,
            "{src}"
        );
    };
    for h in [
        RetHolder::Top,
        RetHolder::Nested,
        RetHolder::Method,
        RetHolder::Static,
    ] {
        let p = |fns: &[RetFn], uses: &str| ret_grid_program(h, fns, uses);
        // ---- block bodies ----
        one(
            &p(&[("f", "n: int", "return n * 2", false)], ""),
            &format!("{NO_RET}; add '-> int' to 'f'"),
        );
        runs(&p(&[("f", "n: int", "print(n)", false)], "@f(3)"), "3\n");
        one(
            &p(&[("f", "n: int", "return g()", false)], ""),
            &format!("{NO_RET}; add '-> int!str' to 'f'"),
        );
        let m = msgs(&p(&[("f", "n: int", "return !\"bad\"", false)], ""));
        assert_eq!(m, vec![ERR_VALUE_NEEDS_TYPE_TEXT.to_string()], "{h:?}");
        let rec = p(
            &[(
                "f",
                "n: int",
                "if n < 1:\n    return 0\nreturn @f(n - 1)",
                false,
            )],
            "",
        );
        let m = msgs(&rec);
        assert!(
            !m.is_empty() && m.iter().all(|e| e.contains(NO_RET)),
            "{m:?}\n{rec}"
        );
        if h != RetHolder::Nested {
            // A nested fn does not see a nested fn declared below it, so it has no mutual cell.
            let mutual = p(
                &[
                    (
                        "a",
                        "n: int",
                        "if n < 1:\n    return 0\nreturn @b(n - 1)",
                        false,
                    ),
                    ("b", "n: int", "return @a(n)", false),
                ],
                "",
            );
            let m = msgs(&mutual);
            assert!(
                !m.is_empty() && m.iter().all(|e| e.contains(NO_RET)),
                "{m:?}\n{mutual}"
            );
        }
        // ---- inline expression bodies ----
        runs(&p(&[("f", "n: int", "n * 2", true)], "print(@f(2))"), "4\n");
        one(
            &p(
                &[("f", "n: int", "n * 2", true)],
                "x: str = @f(1)\nprint(x)",
            ),
            "cannot assign int to variable of type str",
        );
        runs(&p(&[("f", "n: int", "print(n)", true)], "@f(3)"), "3\n");
        one(
            &p(&[("f", "n: int", "g()", true)], "x: str = @f(1)\nprint(x)"),
            "cannot assign int!str to variable of type str",
        );
        let m = msgs(&p(&[("f", "n: int", "!\"bad\"", true)], ""));
        assert_eq!(m, vec![ERR_VALUE_NEEDS_TYPE_TEXT.to_string()], "{h:?}");
        one(
            &p(
                &[("f", "n: int", "if n < 2: 1 else: n * @f(n - 1)", true)],
                "",
            ),
            CYCLE,
        );
        if h != RetHolder::Nested {
            let mutual = p(
                &[
                    ("a", "n: int", "if n < 1: 0 else: @b(n - 1)", true),
                    ("b", "n: int", "@a(n)", true),
                ],
                "",
            );
            let m = msgs(&mutual);
            assert!(
                m.len() == 2 && m.iter().all(|e| e.contains(CYCLE)),
                "{m:?}\n{mutual}"
            );
            assert!(
                m.iter().any(|e| e.contains("'a'")) && m.iter().any(|e| e.contains("'b'")),
                "{m:?}"
            );
        }
    }
    // A sole diverging call is `None`; an empty list has no element type to give.
    runs("fn boom(): panic(\"x\")\nprint(1)\n", "1\n");
    one(
        "fn f(): []\n",
        "cannot infer return type of 'f'; add a -> annotation",
    );
    // A forward reference with no cycle only orders the walks.
    runs("fn c(): d() + 1\nfn d(): 2\nprint(c())\n", "3\n");
    // ---- generators: the element type is written ----
    const GEN: &str = "must declare a return type of `Iterator[T]`";
    one("fn count(n: int):\n    yield n\n", GEN);
    one(
        "struct S:\n    v: int\n    fn each(self):\n        yield self.v\n",
        GEN,
    );
    runs(
        "import Iterator from std.iter\nfn count(n: int) -> Iterator[int]:\n    yield n\nfor x in count(4):\n    print(x)\n",
        "4\n",
    );
    // ---- lambdas: unchanged, and typed by the same `inline_body_ty` ----
    // (A block-body lambda is a parse error, so it has no cell.)
    runs("g := fn(x: int): x * 2\nprint(g(2))\n", "4\n");
    one(
        "fn g() -> int!str:\n    return 1\nh := fn(): g()\nx: str = h()\nprint(x)\n",
        "cannot assign int!str to variable of type str",
    );
    ok("g := fn(): panic(\"x\")\n");
    one("g := fn(n: int): g(n)\n", "unknown name 'g'");
}

/// TICKET-228 (owner, 2026-10-09) — a `T!E` type is never inferred from returns or branches. An
/// error value `!e` takes its type from an annotation or an expected type; with neither it reports
/// the one `!` text, exactly once, and no site adds a message of its own beside it.
#[test]
fn err_value_type_grid() {
    let typed = [
        "fn f(c: bool) -> int!str:\n    if c:\n        return 1\n    return !\"bad\"\n",
        "g := fn(c: bool) -> int!str: if c: 1 else: !\"bad\"\n",
        "fn f(c: bool):\n    x: int!str = if c: 1 else: !\"bad\"\n    print(x)\n",
        "fn f(c: bool):\n    x: int!str = match c:\n        true: 1\n        false: !\"bad\"\n    print(x)\n",
        "x: int!str = !\"e\"\nprint(x)\n",
        // expected type
        "fn take(r: int!str):\n    print(r)\ntake(!\"bad\")\n",
        "fn take(r: int!str):\n    print(r)\ntake(if true: 1 else: !\"bad\")\n",
        "fn run(f: fn(bool) -> int!str):\n    print(f(true))\nrun(fn(c: bool): if c: 1 else: !\"bad\")\n",
    ];
    for src in typed {
        ok(src);
    }
    let untyped = [
        "g := fn(): !\"bad\"\n",
        "g := fn(c: bool): if c: 1 else: !\"bad\"\n",
        "fn f(c: bool):\n    x := if c: 1 else: !\"bad\"\n    print(x)\n",
        "c := true\nx := if c: 1 else: !\"bad\"\nprint(x)\n",
        "fn f(c: bool):\n    x := match c:\n        true: 1\n        false: !\"bad\"\n    print(x)\n",
        "c := true\nx := match c:\n    true: 1\n    false: !\"bad\"\nprint(x)\n",
        "fn f():\n    x := !\"e\"\n    print(x)\n",
        "x := !\"e\"\nprint(x)\n",
        "fn f(c: bool): if c: 1 else: !\"bad\"\n",
        "fn f(c: bool):\n    if c:\n        return 1\n    return !\"bad\"\n",
    ];
    for src in untyped {
        let m: Vec<String> = check_src(src).into_iter().map(|e| e.message).collect();
        let hits = m.iter().filter(|e| *e == ERR_VALUE_NEEDS_TYPE_TEXT).count();
        assert_eq!(hits, 1, "want the `!` text exactly once, got {m:?}\n{src}");
        assert!(
            !m.iter().any(|e| e.contains("?!") || e.contains("_!")),
            "{m:?}\n{src}"
        );
    }
}

/// TICKET-228 — a value that wraps into a carrier slot meets the payload's checks, whichever way
/// the wrap is spelled (`?x`, implicit wrap): constant width, payload pin, the int-into-float note,
/// comparability and the `Error` bound of `!e`. Rust, for the width cell: a literal `300` in an
/// `i8` payload is `literal out of range for `i8``.
#[test]
fn carrier_wrap_check_grid() {
    enum Want {
        Clean,
        /// Exactly one error, and it contains the text.
        One(String),
        /// Some error contains the text.
        Has(String),
    }
    use Want::*;
    let one = |s: &str| One(s.to_string());
    let mut cells: Vec<(String, Want)> = Vec::new();
    let body = |lines: &[&str]| -> String {
        let mut s = String::from("fn f():\n");
        for l in lines {
            s.push_str("    ");
            s.push_str(l);
            s.push('\n');
        }
        s
    };

    // Width: the reference slot and every wrap site.
    const W: &str = "import std.ffi\nimport int8 from std.ffi\nimport float32 from std.ffi\n";
    // TICKET-262: `int8?` puts the width under a type argument, which is no C position, so every
    // cell built on `W` is a rule-1 error whatever the constant.
    let r1 = || Has("describes the C boundary only".to_string());
    for prog in [
        "o: int8 = 300\n",
        "o: int8? = ?300\n",
        "o: int8? = 300\n",
        "o: int8?? = ?300\n",
        "o: int8!str = 300\n",
        "o: int8!str = ?300\n",
        "fn r() -> int8?:\n    return 300\n",
        "fn r() -> int8?:\n    return ?300\n",
        "fn g(x: int8? = 300) -> None:\n    pass\n",
        "fn g(x: int8? = ?300) -> None:\n    pass\n",
        "l: List[int8?] = [300]\n",
        "l: List[int8?] = [?300]\n",
        "m: Map[str, int8?] = {\"a\": 300}\n",
        "m: Map[str, int8?] = {\"a\": ?300}\n",
        "fn g(x: int8?) -> None:\n    pass\ng(300)\n",
        "fn g(x: int8?) -> None:\n    pass\ng(?300)\n",
        "o: int8? = None\no = 300\n",
        "o: int8? = None\no = ?300\n",
        "struct S:\n    v: int8?\ns := S(300)\n",
        "struct S:\n    v: int8?\ns := S(?300)\n",
        "o: int8? = 100 + 200\n",
    ] {
        cells.push((format!("{W}{prog}"), r1()));
    }
    cells.push((format!("{W}o: int8? = -129\n"), r1()));
    cells.push((format!("{W}o: int8? = ?(-129)\n"), r1()));
    cells.push((format!("{W}o: float32? = ?1e39\n"), r1()));
    cells.push((format!("{W}o: int8?? = 300\n"), r1()));
    cells.push((format!("{W}o: int8? = 127\nprint(o)\n"), r1()));
    cells.push((format!("{W}o: int8? = ?127\nprint(o)\n"), r1()));

    // TICKET-238 -- inference never reads a later statement, so the payload-pin rows were deleted:
    // their subject was a later use fixing an open payload (first-use pin, pin-miss texts, `??`
    // and generic-sibling readers, alias and branch-join pins). What stays: an untyped open binder
    // is rejected on its own line whatever writes follow, and the same writes into a TYPED binder
    // are judged against its annotation.
    let hole = "cannot infer the";
    let slot = "'?' value: expected int, found str";
    // Slot matrix: writer x wrap position x {untyped binder, typed conflict, typed agree}.
    type Tpl<'a> = &'a dyn Fn(&str) -> String;
    let positions: [(&str, &str, Tpl); 5] = [
        ("None", "int?", &|p| format!("?{p}")),
        ("[None]", "List[int?]", &|p| format!("[?{p}]")),
        ("{\"k\": None}", "Map[str, int?]", &|p| {
            format!("{{\"k\": ?{p}}}")
        }),
        ("None", "int??", &|p| format!("?(?{p})")),
        ("(1, None)", "(int, int?)", &|p| format!("(1, ?{p})")),
    ];
    let writers: [(bool, Tpl); 4] = [
        (false, &|v| format!("xs.push({v})")),
        (false, &|v| format!("xs.extend([{v}])")),
        (true, &|v| format!("xs[\"b\"] = {v}")),
        (false, &|v| format!("xs[0] = {v}")),
    ];
    for (is_map, write) in writers {
        for (seed, ty, wrap) in positions {
            let (open, typed) = if is_map {
                (
                    format!("xs := {{\"a\": {seed}}}"),
                    format!("xs: Map[str, {ty}] = {{\"a\": {seed}}}"),
                )
            } else {
                (
                    format!("xs := [{seed}]"),
                    format!("xs: List[{ty}] = [{seed}]"),
                )
            };
            let a = write(&wrap("5"));
            cells.push((body(&[&open, &a]), one(hole)));
            for (second, want) in [("\"a\"", one(slot)), ("6", Clean)] {
                let b = write(&wrap(second));
                cells.push((body(&[&typed, &a, &b]), want));
            }
        }
    }
    for (lines, want) in [
        (&["z := None", "z = ?5"][..], hole),
        (&["xs := []", "xs.push(?5)"], hole),
        (&["m := {}", "m[\"a\"] = ?5"], hole),
        (&["s := Set()", "s.add(5)"], hole),
        (&["z: int? = None", "z = ?5", "z = ?\"ab\""], slot),
        (&["z: int?? = None", "z = ?(?5)", "z = ?(?\"a\")"], slot),
        (
            &[
                "a: int? = None",
                "c: int? = None",
                "a, c = ?5, ?6",
                "a, c = ?\"x\", ?7",
            ],
            slot,
        ),
        (
            &[
                "xs: List[int?] = [None]",
                "xs.push(?5)",
                "y: List[str?] = xs",
            ],
            "cannot assign List[int?] to variable of type List[str?]",
        ),
    ] {
        cells.push((body(lines), one(want)));
    }
    cells.push(("z := None\nz = ?5\n".to_string(), one(hole)));
    cells.push((
        "z: int? = None\nz = ?5\nz = ?\"ab\"\n".to_string(),
        one(slot),
    ));
    // A plain value wraps into a typed carrier slot.
    cells.push((body(&["xs: List[int?] = [None]", "xs.push(5)"]), Clean));
    cells.push((body(&["xs: List[int?] = [None]", "xs.extend([5])"]), Clean));
    // A `?x` bound to a name is `T?` on its own line; a later `int!str` slot does not re-read it.
    cells.push((
        "fn f() -> int!str:\n    x := ?5\n    return x\n".to_string(),
        Has("expected return type int!str, found int?".to_string()),
    ));
    // A generic sibling argument: the typed side decides on the call's own statement.
    const G: &str = "fn mv[T](a: List[T], b: List[T]) -> None:\n    pass\nfn same[T](a: T, b: T) -> None:\n    pass\n";
    for (lines, want) in [
        (&["xs := []", "mv([?5], xs)"][..], one(hole)),
        (
            &["xs: List[int?] = []", "mv([?5], xs)", "xs.push(?\"a\")"],
            one(slot),
        ),
        (
            &["xs: List[int?] = []", "mv([?5], xs)", "xs.push(?6)"],
            Clean,
        ),
        (&["r: int!str = 1", "same(?5, r)"], Clean),
        (&["ys: List[int!str] = []", "mv([?5], ys)"], Clean),
    ] {
        cells.push((format!("{G}{}", body(lines)), want));
    }

    // Int where a float is expected: every sink message carries the fix note.
    let note = " — write 1.0 (or float(x))";
    for prog in [
        "x: float? = ?1\n",
        "x: float?? = ?1\n",
        "fn r() -> float?:\n    return ?1\n",
        "fn g(x: float? = ?1) -> None:\n    pass\n",
        "l: List[float?] = [?1]\n",
        "m: Map[str, float?] = {\"a\": ?1}\n",
        "x: float!str = ?1\n",
    ] {
        let slot = if prog.contains("float??") {
            "float?"
        } else {
            "float"
        };
        cells.push((
            prog.to_string(),
            Has(format!("'?' value: expected {slot}, found int{note}")),
        ));
        if !prog.contains("float??") {
            cells.push((prog.replace("?1", "1"), Has(note.to_string())));
        }
    }
    cells.push(("x: float? = ?1.0\nprint(x)\n".to_string(), Clean));

    // Comparability: `T?` never orders (owner decision 4 of TICKET-265); `T!E` does not order.
    cells.push((
        "a: int? = 1\nb: int? = 2\nprint(a < b)\n".to_string(),
        Has("cannot compare int? and int?".to_string()),
    ));
    cells.push((
        "a: bool? = true\nb: bool? = false\nprint(a < b)\n".to_string(),
        Has("cannot compare bool? and bool?".to_string()),
    ));
    cells.push((
        "a: int!str = 1\nb: int!str = 2\nprint(a < b)\n".to_string(),
        Has("cannot compare int!str and int!str".to_string()),
    ));
    // The `Error` bound of `!e`.
    const S: &str = "struct S:\n    n: int\n";
    for prog in [
        "fn r() -> int!S:\n    return !S(1)\n",
        "fn g(x: int!S = !S(1)) -> None:\n    pass\n",
        "l: List[int!S] = [!S(1)]\n",
    ] {
        cells.push((
            format!("{S}{prog}"),
            Has("S does not satisfy Error".to_string()),
        ));
    }

    let mut red: Vec<String> = Vec::new();
    for (src, want) in &cells {
        let m: Vec<String> = check_entry(src).into_iter().map(|e| e.message).collect();
        let good = match want {
            Clean => m.is_empty(),
            One(t) => m.len() == 1 && m[0].contains(t.as_str()),
            Has(t) => m.iter().any(|e| e.contains(t.as_str())),
        };
        if !good {
            let w = match want {
                Clean => "clean".to_string(),
                One(t) => format!("exactly one error holding {t:?}"),
                Has(t) => format!("an error holding {t:?}"),
            };
            red.push(format!("{src}  want {w}, got {m:?}"));
        }
    }
    assert!(
        red.is_empty(),
        "{} of {} carrier-wrap cells failed:\n{}",
        red.len(),
        cells.len(),
        red.join("\n")
    );

    // Every accept program also RUNS. `run_capture` compiles a module the checker rejected, so
    // each one is checked clean first.
    for (src, out) in [
        (
            "fn f():\n    xs: List[int?] = [None]\n    xs.push(?5)\n    xs.push(?6)\n    z: int? = None\n    z = ?5\n    z = ?7\n    print(xs)\n    print(z)\nf()\n",
            "[None, 5, 6]\n7\n",
        ),
        (
            "fn f():\n    xs: List[int?] = [None]\n    xs.extend([?5])\n    xs.extend([?6])\n    z: List[int?] = [None]\n    z = [?7]\n    m: Map[str, List[int?]] = {\"a\": [None]}\n    m[\"b\"] = [?8]\n    print(xs)\n    print(z)\n    print(m[\"b\"])\nf()\n",
            "[None, 5, 6]\n[7]\n[8]\n",
        ),
        ("fn f() -> int!str:\n    return ?5\nprint(f())\n", "5\n"),
        ("x: float? = ?1.0\nprint(x)\n", "1.0\n"),
    ] {
        ok(src);
        assert_eq!(
            crate::compiler::run_for_tests::run_capture(src).unwrap(),
            out,
            "{src}"
        );
    }
}

/// TICKET-228 — a carrier or variant pattern on a non-enum scrutinee is a checker error. (The VM
/// fault an older test asserted was reachable only through a helper that ignores checker errors.)
#[test]
fn variant_pattern_on_a_non_enum_scrutinee_is_rejected() {
    rejects(
        "fn main():\n    match 5:\n        ?x: print(x)\nmain()\n",
        "`?v` matches a present `T?` or a successful `T!E`, found int",
    );
    rejects(
        "enum E:\n    A(int)\n    B\nfn main():\n    match 5:\n        E.A(x): print(x)\n        _: print(0)\nmain()\n",
        "cannot match a variant against int",
    );
}

/// TICKET-228 (D6) — no near-miss help offers a name the language does not have: a near miss of
/// `Option` or `Result` gets no help.
#[test]
fn near_miss_offers_no_removed_spelling() {
    let help_of = |src: &str, msg: &str| -> Option<String> {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "{src}: {errs:?}");
        assert!(errs[0].message.contains(msg), "{src}: {errs:?}");
        errs[0].help.clone()
    };
    assert_eq!(help_of("x: option = 1\n", "unknown type 'option'"), None);
    assert_eq!(help_of("x: Optoin = 1\n", "unknown type 'Optoin'"), None);
    assert_eq!(
        help_of(
            "fn f(x: Resul) -> int:\n    return 1\n",
            "unknown type 'Resul'"
        ),
        None
    );
    assert_eq!(
        help_of("x: Eror = 3\n", "unknown type 'Eror'"),
        Some("did you mean 'Error'?".to_string())
    );
    for src in ["x := some(1)\n", "x := Som(1)\n", "y := Er(\"x\")\n"] {
        assert_eq!(help_of(src, "unknown name"), None, "{src}");
    }
    assert_eq!(
        help_of(
            "type O = int?\nfn f(x: int?) -> int:\n    match x:\n        O.Som(v): return v\n        _: return 0\n",
            "enum 'O' has no variant 'Som'"
        ),
        None
    );
}

/// TICKET-228 (D6) — a carrier alias is a type spelling, never a variant head.
#[test]
fn carrier_alias_is_not_a_variant_head() {
    let o = "type O = int?\n";
    let r = "type R = int!str\n";
    let (some, okv, errv) = ("Some", "Ok", "Err");
    rejects(
        &format!("{o}x := O.{some}(1)\nprint(x)\n"),
        "'O' is a type, not a value",
    );
    parse_rejects(&format!("{o}x: O = O.None\nprint(x)\n"), "reserved keyword");
    rejects(
        &format!("{r}x := R.{errv}(\"a\")\nprint(x)\n"),
        "'R' is a type, not a value",
    );
    rejects(
        &format!(
            "{o}fn f(x: int?) -> int:\n    match x:\n        O.{some}(v): return v\n        _: return 0\n"
        ),
        "enum 'O' has no variant 'Some'",
    );
    rejects(
        &format!(
            "{r}fn f(x: int!str) -> int:\n    match x:\n        R.{okv}(v): return v\n        _: return 0\n"
        ),
        "enum 'R' has no variant 'Ok'",
    );
    ok(&format!("{o}x: O = 1\nprint(x)\n"));
    ok(&format!("{r}fn f() -> R:\n    return 1\n"));
    // A user-enum alias is still a head.
    ok(
        "enum E:\n    A(int)\n    B\ntype F = E\nx := F.A(1)\nmatch x:\n    F.A(v): print(v)\n    F.B: print(0)\n",
    );
}

/// TICKET-228 (D6) — hover prints these docs; none names a removed spelling.
#[test]
fn module_fn_docs_name_no_removed_spelling() {
    for (module, docs) in crate::checker::graph_pass::MODULE_FN_DOCS {
        for (name, doc) in *docs {
            let hit = doc
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .find(|w| ["Option", "Result", "Some", "Ok", "Err"].contains(w));
            assert_eq!(hit, None, "{module}.{name}: {doc}");
        }
    }
}

#[test]
fn plain_value_meets_open_none_carrier_slot() {
    let cells = [
        (
            "assign",
            "fn main():\n    z: int? = None\n    z = 7\n    print(z)\n",
        ),
        (
            "push",
            "fn main():\n    xs: List[int?] = [None]\n    xs.push(7)\n",
        ),
        (
            "map set",
            "fn main():\n    m: Map[str, int?] = {\"a\": None}\n    m[\"b\"] = 7\n",
        ),
        (
            "list literal",
            "fn main():\n    xs := [None, 7]\n    print(xs)\n",
        ),
        (
            "if expression",
            "fn main():\n    c := true\n    z := if c: None else: 7\n    print(z)\n",
        ),
        (
            "after ??",
            "fn main():\n    z: int? = None\n    a := z ?? 5\n    z = 7\n    print(a)\n",
        ),
        (
            "module global",
            "z: int? = None\nfn f():\n    z = 7\nfn main():\n    f()\n",
        ),
    ];
    let failed: Vec<String> = cells
        .iter()
        .filter_map(|(name, src)| {
            let errs = check_src(src);
            (!errs.is_empty()).then(|| {
                format!(
                    "{name}: {:?}",
                    errs.iter().map(|e| &e.message).collect::<Vec<_>>()
                )
            })
        })
        .collect();
    assert!(
        failed.is_empty(),
        "plain value must meet an open None carrier; rejected cells: {failed:?}"
    );
}

#[test]
fn none_cannot_be_rebound_by_a_binder() {
    for src in [
        "fn find() -> int?:\n    return None\nNone := 5\nprint(find())\n",
        "fn f(None: int) -> int?:\n    return None\n",
        "for None in [1, 2]:\n    print(None)\n",
        "None: int = 4\n",
    ] {
        parse_rejects(src, "keyword");
    }
}

/// TICKET-237 — must still pass: `None` as a value, a default, a pattern, a return type and the
/// success side of `None!E`. Red if the keyword stops parsing in any of those positions.
#[test]
fn none_keyword_stays_legal_where_it_was() {
    let src = "x: int? = None
print(x)
print(x == None)
print(x ?? 4)
fn f(a: int, b: int? = None) -> int:
    return a + (b ?? 10)
print(f(1))
fn v() -> None:
    print(\"v\")
v()
fn e() -> None!str:
    return !\"bad\"
fn e2() -> !str:
    return
match e():
    !m: print(m)
    ?_: print(\"ok\")
print(e2())
match x:
    None: print(\"none arm\")
    ?k: print(k)
z: int?? = ?x
match z:
    ?None: print(\"some none\")
    _: print(\"other\")
print(\"{None}\")
print([None, ?1])
";
    ok(src);
    let out = crate::compiler::run_for_tests::run_capture(src);
    assert_eq!(
        out.as_deref().ok(),
        Some("None\ntrue\n4\n11\nv\nbad\nNone\nnone arm\nsome none\nNone\n[None, 1]\n")
    );
}

/// TICKET-237 — an ASSIGNMENT to a keyword is not a binding and keeps its own text. Red on
/// `None = 5` if `None` is an assignable name again.
#[test]
fn assigning_to_a_keyword_is_an_invalid_target() {
    for src in ["None = 5\n", "a := 1\nNone, a = 1, 2\n", "true = 5\n"] {
        parse_rejects(src, "invalid assignment target");
    }
}

/// TICKET-237 — boundary: only the exact word is the keyword. Red on `Nonee := 5` if the lexer
/// matches the keyword by prefix or without case.
#[test]
fn a_name_that_only_resembles_none_is_an_identifier() {
    ok("Nonee := 5\nnone := 1\n_None := 2\nNONE := 3\nprint(Nonee + none + _None + NONE)\n");
}

/// TICKET-237 — the parser reads the keyword as a NAME only where the prelude declares the
/// variant. Red on `enum E: None` if a user enum, a user import or a member access takes it.
#[test]
fn a_user_file_cannot_name_none() {
    for src in [
        "enum E:\n    None\n    B\n",
        "enum E:\n    A\n    B\nimport None from E\n",
        "enum E:\n    A\n    B\nimport A as None from E\n",
        "struct S:\n    x: int\ns := S(1)\nprint(s.None)\n",
    ] {
        parse_rejects(src, "reserved keyword 'None'");
    }
}

/// `Some` is no name a user module sees (the carriers are reached only through `?v` / `!e`), so a
/// user may bind it: `import A as Some from E` is an ordinary import.
#[test]
fn a_user_import_may_bind_the_removed_name_some() {
    let errs = check_entry("enum E:\n    A\n    B\nimport A as Some from E\nprint(1)\n");
    assert!(errs.is_empty(), "{errs:?}");
}

#[test]
fn open_binding_is_rejected_on_its_own_line() {
    // TICKET-238: the type of a value is known on the statement that creates it.
    rejects(
        "fn main():\n    xs := [None]\n    ys := [xs]\n    ys[0].push(\"s\")\n    xs.push(7)\nmain()\n",
        "cannot infer the element type",
    );
}

#[test]
fn nested_statement_in_a_rolled_back_walk_keeps_the_statement_mark() {
    // TICKET-238: the `recover:` arm is walked speculatively for its `None` sibling, then for
    // real. The rolled-back walk's nested `print(xs)` must leave the `match` statement's mark as
    // it found it: no underflow, and the statement's own error is still reported.
    let errs = check_src(
        "fn main():\n    xs := []\n    c := true\n    r := match c:\n        true: None\n        false: recover:\n            print(xs)\n            1\n    print(r)\nmain()\n",
    );
    let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(
        msgs[0].contains("cannot infer the element type of `xs`"),
        "{msgs:?}"
    );
    assert!(
        msgs[1].contains("branches have incompatible types"),
        "{msgs:?}"
    );
}

/// TICKET-238 -- inference never reads a later statement. The tests of the pin machinery (alias
/// pin groups, carrier pins, refine-on-first-use, the pin-miss texts) were deleted with it. Each
/// program they accepted through a later pin is rejected on the line that creates the open
/// binding, and its typed spelling is accepted. The whole grid is `tests/open_binding_grid.rs`.
#[test]
fn later_pin_programs_are_rejected_on_the_creation_line() {
    for src in [
        "b := []\nc := b\nc.push(1)\n",
        "x := None\nx = ?1\n",
        "fn f(xs: List[int]):\n    print(xs)\nxs := []\nf(xs)\n",
        "enum Box[T]:\n    Full(T)\n    Empty\ne := Box.Empty\ne = Box.Full(1)\n",
        "m := {}\nm[\"k\"] = 1\n",
        "fn main():\n    xs := (if true: [] else: [])\n    xs.push(1)\nmain()\n",
        "fn tag[U](xs: List[U]) -> List[U]:\n    return xs\nx := tag([])\nx.push(1)\n",
    ] {
        rejects(src, "cannot infer the");
    }
    ok("b: List[int] = []\nc := b\nc.push(1)\n");
    ok("x: int? = None\nx = ?1\n");
    ok("m: Map[str, int] = {}\nm[\"k\"] = 1\n");
    // A write that misses the annotation is an ordinary mismatch, not a pin text.
    rejects("x: int? = None\nx = ?\"s\"\n", "expected int, found str");
    // A return-only type parameter has its own owner, at the call.
    const EMPTY: &str = "fn empty[T]() -> List[T]:\n    return []\n";
    rejects(
        &format!("{EMPTY}xs := empty()\nxs.push(1)\n"),
        "cannot infer type parameter T for 'empty'",
    );
    ok(&format!("{EMPTY}xs: List[int] = empty()\nxs.push(1)\n"));
    ok(&format!("{EMPTY}xs := empty[int]()\n"));
    rejects(
        "fn make[U]() -> U:\n    return make()\nz := make()\n",
        "cannot infer type parameter U for 'make'",
    );
}

/// TICKET-238 (`## Decisions`: "No generic call leaks the callee's own `Ty::Param`") -- a type
/// parameter in a PARAMETER position that an empty argument leaves unbound is the bare `Unknown`
/// sentinel on every call path. `x := first([])` is accepted: no value of that type can exist,
/// because the call faults first. Before, the free-fn path leaked `U`, and the use `x + 1` was
/// `cannot apply + to U and int`.
#[test]
fn an_unbound_parameter_position_type_param_is_the_uninhabited_sentinel() {
    entry_ok(
        "fn first[U](xs: List[U]) -> U:\n    return xs[0]\nfn main():\n    x := first([])\n    print(x + 1)\n",
    );
}

#[test]
fn closure_literal_wraps_into_an_optional_fn_slot() {
    ok(
        "fn take(cb: (fn(int) -> int)?) -> int:\n    f := cb else: return 0\n    return f(4)\nprint(take(fn(a: int) -> int: a + 1))\n",
    );
}

/// TICKET-239: a closure literal reads its expected type in the `Closure` arm of `infer`, through
/// one carrier layer, so every slot kind gives it the same verdict.
#[test]
fn closure_literal_reads_its_slot_through_one_carrier_layer() {
    for c in ["fn(a): a + 1", "fn(a: int) -> int: a + 1"] {
        ok(&format!("f: (fn(int) -> int)? = {c}\n"));
        ok(&format!(
            "fn take(cb: (fn(int) -> int)?) -> int:\n    f := cb else: return 0\n    return f(4)\nprint(take({c}))\n"
        ));
        ok(&format!(
            "struct S:\n    f: (fn(int) -> int)?\ns := S(None)\ns.f = {c}\n"
        ));
        ok(&format!("fn mk() -> (fn(int) -> int)?:\n    return {c}\n"));
    }
    ok("xs: List[fn(int) -> int] = [fn(a): a + 1]\n");
    rejects(
        "f: (fn(int) -> int)? = fn(a: str) -> int: 1\n",
        "cannot assign",
    );
    // Boundary: ONE layer is looked through. A slot two layers deep does not type the closure
    // (red if the strip loops over every layer, or if a wrap is applied twice).
    rejects(
        "f: (fn(int) -> int)?? = fn(a): a + 1\n",
        "cannot infer type of parameter 'a'",
    );
    rejects(
        "f: (fn(int) -> int)?? = fn(a: int) -> int: a + 1\n",
        "cannot assign",
    );
}

/// TICKET-239: a generic call substitutes its explicit type arguments before it reads an
/// argument, so the argument owns the substituted slot and wraps like at a non-generic slot.
#[test]
fn explicit_type_args_give_each_generic_argument_its_owned_slot() {
    let opt = "fn opt[T](x: T) -> T:\n    return x\n";
    ok("struct Box[T]:\n    v: T\nb := Box[int?](5)\n");
    ok(&format!("{opt}c := opt[int?](5)\n"));
    ok(&format!("{opt}c := opt[List[int]?]([])\n"));
    ok(&format!("{opt}c := opt[(fn(int) -> int)?](fn(a): a + 1)\n"));
    ok(
        "struct B[T]:\n    v: T\n    fn mk(x: T) -> B[T]:\n        return B(x)\nb := B[int?].mk(5)\n",
    );
    ok(
        "struct S:\n    n: int\n    fn gm[U](self, x: U) -> U:\n        return x\ns := S(0)\ny := s.gm[int?](5)\n",
    );
    ok(&format!("{opt}x := opt[int!str](!\"e\")\n"));
}

/// TICKET-239: with no type arguments the annotation gives the slot: the return type is unified
/// with the expected type before the arguments are read.
#[test]
fn annotation_only_generic_call_meets_the_width_and_wraps() {
    let id = "fn id[T](x: T) -> T:\n    return x\n";
    // `entry_rejects`: only the graph wire resolves a `std.ffi` import.
    entry_rejects(
        &format!("import int8 from std.ffi\n{id}b: int8? = id(300)\n"),
        "describes the C boundary only",
    );
    ok("struct Box[T]:\n    v: T\nc: Box[int?] = Box(5)\n");
    ok(&format!("{id}r: int? = id(?5)\n"));
    ok(&format!("{id}r: int? = id(None)\n"));
    ok(
        "fn first[T](xs: List[T]) -> T:\n    return xs[0]\nxs: List[int] = [1]\nn: int? = first(xs)\n",
    );
}

/// TICKET-239: a closure at a slot the type arguments fix is fully inferred, so its return type
/// is real and a mismatch is reported; only a prepassed closure has a masked return.
#[test]
fn an_owned_slot_closure_is_not_return_masked() {
    let ap = "fn ap[T](f: fn(int) -> T) -> T:\n    return f(1)\n";
    let two = "fn two[T, U](x: T, f: fn(T) -> U) -> U:\n    return f(x)\n";
    rejects(
        &format!("{ap}x := ap[int](fn(a): \"s\")\n"),
        "has type fn(int) -> str, expected fn(int) -> int",
    );
    ok(&format!("{ap}x := ap[int](fn(a): a + 1)\n"));
    ok(&format!("{ap}x := ap(fn(a): a + 2)\n"));
    ok(&format!("{two}x := two[int, str](1, fn(a): \"s{{a}}\")\n"));
    ok(&format!("{two}x := two(1, fn(a) -> str: \"t{{a}}\")\n"));
}

/// TICKET-239: a carrier annotation over a bare-`T` return is a guess at `T`, so it never types
/// a closure argument's parameters; the closure is prepassed and re-inferred against the `T` its
/// sibling arguments solve, and the result wraps at the binding.
#[test]
fn a_seed_only_slot_never_types_a_closure_argument() {
    let apply = "fn apply[T](x: T, f: fn(T) -> T) -> T:\n    return f(x)\n";
    let ap = "fn ap[T](f: fn(T) -> T, x: T) -> T:\n    return f(x)\n";
    let reduce = "fn reduce[T](xs: List[T], f: fn(T, T) -> T) -> T:\n    return f(xs[0], xs[1])\n";
    ok(&format!("{apply}r: int? = apply(5, fn(x): x + 1)\n"));
    ok(&format!("{apply}r: int!str = apply(5, fn(x): x + 1)\n"));
    ok(&format!(
        "{apply}r: int? = apply(5, fn(x: int) -> int: x + 1)\n"
    ));
    ok(&format!("{ap}r: int? = ap(fn(x): x + 1, 5)\n"));
    ok(&format!(
        "{reduce}fn total(xs: List[int]) -> int?:\n    return reduce(xs, fn(a, b): a + b)\n"
    ));
    // The seed still reaches a non-closure sibling: `T` is `int?` here, so the closure's `x` is too.
    ok(&format!("{ap}r: int? = ap(fn(x): x, None)\n"));
    ok(&format!("{ap}r: int? = ap(fn(x): x, ?5)\n"));
    rejects(
        &format!("{ap}r: int? = ap(fn(x): x + 1, ?5)\n"),
        "cannot apply + to int? and int",
    );
    // A closure that IS the `T` has no sibling to wait for: the seed is its only source.
    let g = "fn g[T](x: T) -> T:\n    return x\n";
    ok(&format!("{g}r: (fn(int) -> int)? = g(fn(a): a + 1)\n"));
    rejects(
        &format!("{g}r: int? = g(fn(a): a + 1)\n"),
        "cannot infer type of parameter 'a'",
    );
    // A prepassed closure still reports its body errors against the solved `T`.
    rejects(
        &format!("{apply}r: int? = apply(5, fn(x): x.nope())\n"),
        "nope",
    );
}

/// TICKET-239: a carrier-annotation seed reaches an argument only at the bare `T` slot it guesses.
/// It is never substituted into a compound slot (`List[T]`, `Map[str, T]`, `fn(T) -> T`), where it
/// would be an owned slot for the elements and solve `T` to the carrier. Whatever the argument's
/// form, `T` is solved from the arguments and the result wraps at the binding.
#[test]
fn a_carrier_seed_reaches_only_the_bare_parameter_slot() {
    let reduce = "fn reduce[T](xs: List[T], f: fn(T, T) -> T) -> T:\n    return f(xs[0], xs[1])\n";
    let find = "fn find[T](xs: List[T], x: T) -> T:\n    return x\n";
    let val = "fn val[T](m: Map[str, T], x: T) -> T:\n    return x\n";
    ok(&format!(
        "{reduce}r: int? = reduce([1, 2], fn(a, b): a + b)\n"
    ));
    ok(&format!(
        "{reduce}r: int!str = reduce([1, 2], fn(a, b): a + b)\n"
    ));
    ok(&format!(
        "{reduce}r: int? = reduce([i for i in [1, 2]], fn(a, b): a + b)\n"
    ));
    ok(&format!("{find}r: int? = find([1, 2], 2)\n"));
    ok(&format!("{find}r: int? = find([], 2)\n"));
    ok(&format!("{val}r: int? = val({{\"k\": 1}}, 2)\n"));
    // `T` is `int`, not the seed's `int?`: the closure's parameters are plain.
    rejects(
        &format!("{reduce}r: int? = reduce([1, 2], fn(a, b): a ?? b)\n"),
        "??",
    );
    // The bare `T` slot still reads the seed: a `None` or `?v` there is typed by it.
    ok(&format!("{find}r: int? = find([], None)\n"));
    ok(&format!("{find}r: int? = find([?1], ?2)\n"));
    // A sibling that solves `T = int` leaves no room for a carrier at the bare slot.
    rejects(
        &format!("{find}r: int? = find([1, 2], None)\n"),
        "argument to 'find'",
    );
}

/// TICKET-239: a `?.` call that returns nothing reports a bad argument ONCE, and nothing else: the
/// void lowering is kept on its type alone, so no value-form error or discard warning follows.
#[test]
fn optional_chain_void_call_reports_an_argument_error_once() {
    let c = "struct C:\n    n: int\n    fn add(self, k: int):\n        self.n += k\n    fn get(self, k: int) -> int:\n        return self.n + k\n";
    let (errs, warns) = warn_src(&format!("{c}x: C? = C(0)\nx?.add(undefined_name)\n"));
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains("unknown name 'undefined_name'"));
    assert!(warns.is_empty(), "{warns:?}");
    // The value form re-infers after a missed void attempt: still one error.
    let (errs, _) = warn_src(&format!("{c}x: C? = C(0)\nv := x?.get(undefined_name)\n"));
    assert_eq!(errs.len(), 1, "{errs:?}");
    let (errs, _) = warn_src(&format!("{c}x: C? = C(0)\nv := x?.get(1 + \"s\")\n"));
    assert_eq!(errs.len(), 1, "{errs:?}");
}

/// TICKET-239: an un-annotated `if` / `match` / `??` join of two `T!E` values keeps `T!E`; no
/// default rewrites the error type.
#[test]
fn an_unannotated_join_of_two_results_keeps_the_error_type() {
    let ab = "fn a() -> int!str: !\"bad\"\nfn b() -> int!str: 5\n";
    let tail = "    v := x?\n    return v + 1\n";
    ok(&format!(
        "{ab}fn pick(c: bool) -> int!str:\n    x := if c: a() else: b()\n{tail}"
    ));
    ok(&format!(
        "{ab}fn pick(c: bool) -> int!str:\n    x := match c:\n        true: a()\n        false: b()\n{tail}"
    ));
    ok(&format!(
        "{ab}fn pick(o: (int!str)?) -> int!str:\n    x := o ?? b()\n{tail}"
    ));
}

/// TICKET-239: a `T!E` sibling types a bare `!e` like a written `None` sibling types a plain
/// value, with one verdict at module scope and in a fn body.
#[test]
fn an_err_value_beside_a_result_sibling_takes_its_type_in_both_scopes() {
    let a = "fn a() -> int!str: !\"bad\"\n";
    for stmt in [
        "x := if c: a() else: !\"e\"",
        "x := match c:\n    true: a()\n    false: !\"e\"",
        "x := [a(), !\"e\"]",
        "x := {\"k\": a(), \"j\": !\"e\"}",
    ] {
        ok(&format!("{a}c := true\n{stmt}\n"));
        let body = stmt.replace('\n', "\n    ");
        ok(&format!("{a}fn main(c: bool):\n    {body}\n"));
    }
    rejects(
        "c := true\nx := if c: 1 else: !\"bad\"\n",
        "needs its type from an annotation",
    );
    rejects(
        "fn main(c: bool):\n    x := if c: 1 else: !\"bad\"\n",
        "needs its type from an annotation",
    );
}

/// TICKET-239: `?x` operands compare in a fn body as they do at module scope; arithmetic on one
/// is still an error about the optional.
#[test]
fn a_present_value_operand_compares_in_a_fn_body() {
    rejects(
        "fn main(): print(?1 < ?2)\n",
        "cannot compare int? and int?",
    );
    rejects("print(?1 < ?2)\n", "cannot compare int? and int?");
    rejects(
        "fn main(): print(1 + ?5)\n",
        "cannot apply + to int and int?",
    );
    rejects("print(1 + ?5)\n", "cannot apply + to int and int?");
}

/// TICKET-239 (owner, 2026-10-10): a carrier beside its plain payload does not join by itself;
/// the error names the annotation that makes it legal.
#[test]
fn optional_beside_plain_join_names_the_annotation_fix() {
    let note = "only under a written type";
    let w = "w: int? = 5\nc := true\n";
    let a = "fn a() -> int!str: !\"bad\"\nc := true\n";
    rejects(&format!("{w}x := [w, 3]\n"), note);
    rejects(&format!("{w}x := if c: w else: 5\n"), note);
    rejects(&format!("{w}x := if c: 5 else: w\n"), note);
    rejects(
        &format!("{w}x := match c:\n    true: w\n    false: 5\n"),
        note,
    );
    rejects(&format!("{w}x := {{\"k\": w, \"j\": 3}}\n"), note);
    rejects(&format!("{w}x := {{w, 3}}\n"), note);
    rejects(&format!("{a}x := if c: a() else: 5\n"), note);
    rejects(&format!("{a}x := [a(), 5]\n"), note);
    // A mismatch that no annotation of the carrier fixes carries no note.
    for src in [
        "c := true\nx := if c: 1 else: \"s\"\n".to_string(),
        format!("{w}x := if c: w else: \"s\"\n"),
    ] {
        let errs = check_src(&src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("branches have incompatible types")),
            "{errs:?}"
        );
        assert!(
            errs.iter().all(|e| !e.message.contains("written type")),
            "{errs:?}"
        );
    }
}

/// TICKET-239: a `??` mismatch names `??`; a `match` mismatch still names branches.
#[test]
fn coalesce_mismatch_names_the_operator() {
    rejects(
        "fn f(o: int?) -> int?:\n    return o ?? None\n",
        "'??' sides have incompatible types",
    );
    rejects(
        "c := true\nx := match c:\n    true: 1\n    false: \"s\"\n",
        "branches have incompatible types: int and str",
    );
}

/// TICKET-239: `x?.m()` on a method that returns nothing has no value. It is legal wherever a
/// plain call that returns nothing is legal (`infer` against `infer_value` decides, no statement
/// flag), and an error wherever a value is needed.
#[test]
fn optional_chain_void_call_has_no_value() {
    let c = "struct C:\n    n: int\n    fn bump(self):\n        self.n += 1\n    fn get(self) -> int:\n        return self.n\n";
    let each = "fn each[T](xs: List[T], f: fn(T) -> None):\n    for x in xs:\n        f(x)\n";
    for prog in [
        "c: C? = C(0)\nc?.bump()\n",
        "fn main():\n    c: C? = C(0)\n    c?.bump()\n",
        "fn poke(c: C?): c?.bump()\n",
        "c: C? = C(0)\nf := fn(): c?.bump()\nf()\n",
        "c: C? = C(0)\nif true: c?.bump()\n",
        "c: C? = C(0)\nmatch 1:\n    1: c?.bump()\n    _: pass\n",
    ] {
        ok(&format!("{c}{prog}"));
        no_warn(&format!("{c}{prog}"));
    }
    let gen_closure = format!("{c}{each}xs: List[C?] = [C(0)]\neach(xs, fn(a): a?.bump())\n");
    ok(&gen_closure);
    no_warn(&gen_closure);
    for value_use in ["x := c?.bump()", "print(c?.bump())", "xs := [c?.bump()]"] {
        rejects(
            &format!("{c}c: C? = C(0)\n{value_use}\n"),
            "expression returns no value",
        );
    }
    // A call that returns a value keeps the value lowering and its discarded-value warning.
    warns(
        &format!("{c}c: C? = C(0)\nc?.get()\n"),
        "value here is discarded",
    );
    ok(&format!("{c}c: C? = C(0)\nv := c?.get()\nw: int? = v\n"));
    ok(&format!(
        "{c}xs: List[C?] = [C(0)]\nys: List[int?] = xs.map(fn(a): a?.get())\n"
    ));
}

/// The `CallCrossing` of the one spawn in `src`.
fn spawn_crossing(src: &str) -> crate::checker::CallCrossing {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let mut m = parser::parse(tokens).unwrap_or_else(|e| panic!("parse failed: {e:?}\n{src}"));
    crate::desugar::run_standalone(&mut m).unwrap_or_else(|e| panic!("desugar failed: {e:?}"));
    assert_eq!(format!("{:?}", check_diags(&m)), "(Ok(()), [])", "{src}");
    let table = crate::checker::resolve_call_tables_standalone(&m.stmts).6;
    assert_eq!(table.len(), 1, "one spawn per program\n{src}");
    table.into_values().next().unwrap()
}

/// TICKET-240 Rule S: `Checker::fresh_shape` decides, per node of a spawn operand, whether the
/// parent can still reach the object. One cell per rule line, as `(param, operand, slot shapes)`.
#[test]
fn fresh_shape_grid() {
    let head = "struct S:\n    a: List[int]\n    b: List[int]\n    fn go(self):\n        pass\nstruct Key:\n    id: int\n    log: List[int]\n    fn hash(self) -> int:\n        return self.id\n    fn eq(self, other: Key) -> bool:\n        return self.id == other.id\nenum E:\n    V(List[int])\nfn mk() -> List[int]:\n    return [0]\n";
    let main = "fn main():\n    xs := [0]\n    n := 1\n    s := S([], [])\n    k := Key(1, [])\n    ks := [k]\n    msg := str(n)\n    _ := (xs, n, s, k, ks, msg)\n    parallel:\n";
    let deep = |n: usize| format!("{}0{}", "[".repeat(n), "]".repeat(n));
    let deep_ty = |n: usize| format!("{}int{}", "List[".repeat(n), "]".repeat(n));
    let deep17 = format!("[{}Marked{}]", "Node(At([".repeat(16), "]))".repeat(16));
    let cells: Vec<(String, String, String)> = [
        // Scalars and literals.
        ("x: int", "1", "[All]"),
        ("x: str", "'a'", "[All]"),
        ("x: List[List[int]]", "[[0], [1]]", "[All]"),
        ("x: List[List[int]]", "[xs]", "[Node(At([Marked]))]"),
        (
            "x: List[List[int]]",
            "[[], xs]",
            "[Node(At([All, Marked]))]",
        ),
        (
            "x: (List[int], int)",
            "(xs, 1)",
            "[Node(At([Marked, All]))]",
        ),
        ("x: Set[int]", "{1, 2}", "[All]"),
        ("x: Set[int]", "{n}", "[Node(Marked)]"),
        // A map: a key that is not fresh keeps the map from `All`.
        ("x: Map[int, List[int]]", "{1: []}", "[All]"),
        ("x: Map[int, List[int]]", "{1: xs}", "[Node(Marked)]"),
        ("x: Map[int, List[int]]", "{1: [], 2: xs}", "[Node(Marked)]"),
        (
            "x: Map[int, List[List[int]]]",
            "{1: [[]], 2: [xs]}",
            "[Node(Each(Node(Marked)))]",
        ),
        ("x: Map[Key, List[int]]", "{k: []}", "[Node(Each(All))]"),
        (
            "x: Map[Key, List[int]]",
            "{x: [] for x in ks}",
            "[Node(Each(All))]",
        ),
        (
            "x: Map[Key, List[int]]",
            "{x: xs for x in ks}",
            "[Node(Marked)]",
        ),
        // Comprehensions.
        ("x: List[List[int]]", "[[0] for _i in 0..1]", "[All]"),
        (
            "x: List[List[int]]",
            "[[i] for i in 0..1]",
            "[Node(Each(Node(At([Marked]))))]",
        ),
        (
            "x: List[List[int]]",
            "[xs for _i in 0..1]",
            "[Node(Marked)]",
        ),
        ("x: Set[int]", "{1 for _i in 0..1}", "[All]"),
        ("x: Set[int]", "{i for i in 0..1}", "[Node(Marked)]"),
        // Carriers: explicit, error value, implicit.
        ("x: List[int]?", "?[]", "[All]"),
        ("x: List[int]?", "?xs", "[Node(At([Marked]))]"),
        ("x: int!str", "!'e'", "[All]"),
        ("x: int!str", "!msg", "[Node(At([Marked]))]"),
        ("x: List[int]?", "[]", "[All]"),
        ("x: List[int]?", "xs", "[Node(At([Marked]))]"),
        ("x: List[int]!str", "xs", "[Node(At([Marked]))]"),
        (
            "x: List[List[int]]?",
            "[xs]",
            "[Node(At([Node(At([Marked]))]))]",
        ),
        ("x: List[int]??", "??xs", "[Node(At([Node(At([Marked]))]))]"),
        // `copy()` is shallow, and only a container's.
        ("x: List[int]", "xs.copy()", "[Node(Marked)]"),
        ("x: List[int]?", "xs.copy()", "[Node(At([Node(Marked)]))]"),
        ("x: S", "s.copy()", "[Marked]"),
        // Constructors: a keyword argument sits at its field's position.
        ("x: S", "S([], [])", "[All]"),
        ("x: S", "S([], xs)", "[Node(At([All, Marked]))]"),
        ("x: S", "S(b=[], a=xs)", "[Node(At([Marked, All]))]"),
        ("x: E", "E.V([])", "[All]"),
        ("x: E", "E.V(xs)", "[Node(At([Marked]))]"),
        // Everything else stays marked.
        ("x: List[int]", "xs", "[Marked]"),
        ("x: List[int]", "mk()", "[Marked]"),
        ("x: List[int]", "s.a", "[Marked]"),
        ("x: List[int]", "ks[0].log", "[Marked]"),
        ("x: List[int]", "xs + xs", "[Node(Marked)]"),
        // Slots: a pack is a list built at the call, a default is a literal or a provider call.
        ("...x: List[int]", "[], xs", "[Node(At([All, Marked]))]"),
        ("...x: List[int]", "[], []", "[All]"),
        ("x: List[int]? = []", "", "[All]"),
        ("x: List[int] = mk()", "", "[Marked]"),
    ]
    .iter()
    .map(|(p, a, w)| (p.to_string(), a.to_string(), w.to_string()))
    // The depth limit: 16 levels are described. A leaf or a list below them stays `Marked`.
    .chain([
        (format!("x: {}", deep_ty(15)), deep(15), "[All]".to_string()),
        (format!("x: {}", deep_ty(16)), deep(16), deep17.clone()),
        (format!("x: {}", deep_ty(17)), deep(17), deep17),
    ])
    .collect();
    for (param, arg, want) in cells {
        let src = format!("{head}fn w({param}):\n    pass\n{main}        spawn w({arg})\nmain()\n");
        let c = spawn_crossing(&src);
        assert_eq!(c.recv, None, "{param} <- {arg}");
        assert_eq!(format!("{:?}", c.args), want, "{param} <- {arg}");
    }
    // The deferred wrap: `?x` into a generic param records its carrier at frame end, after the
    // crossing is computed, so the level comes from the node itself.
    let c = spawn_crossing(&format!(
        "{head}fn show[T](x: T):\n    pass\n{main}        spawn show(?[xs])\nmain()\n"
    ));
    assert_eq!(format!("{:?}", c.args), "[Node(At([Node(At([Marked]))]))]");
    // A default that mentions a type parameter is callee-filled (`desugar::dflt_for`): the task
    // builds it, so it is no operand of the crossing and has no shape.
    let c = spawn_crossing(&format!(
        "{head}fn anyl[U]() -> List[U]:\n    return []\nfn gw[T](a: T, x: List[T] = anyl()):\n    pass\n{main}        spawn gw(1)\nmain()\n"
    ));
    assert_eq!(format!("{:?}", c.args), "[All]");
    // The receiver is entry 0.
    for (recv, want) in [
        ("S([], [])", "Some(All)"),
        ("S(xs, [])", "Some(Node(At([Marked, All])))"),
        ("s", "Some(Marked)"),
    ] {
        let c = spawn_crossing(&format!("{head}{main}        spawn {recv}.go()\nmain()\n"));
        assert_eq!(format!("{:?}", c.recv), want, "{recv}");
    }
}

#[test]
fn inline_if_else_statement_is_accepted() {
    ok("fn main():\n    c := true\n    if c: print(1) else: print(2)\n");
}

#[test]
fn inline_if_elif_else_statement_is_accepted() {
    ok(
        "fn main():\n    a := false\n    b := true\n    if a: print(1) elif b: print(2) else: print(3)\n",
    );
}

#[test]
fn inline_recover_in_call_argument_is_accepted() {
    ok("fn main():\n    print(recover: 1)\n");
}

/// TICKET-242 -- a one-line `if … else` statement discards its value, so its branches need not
/// join; each branch is checked as a discarded value of its own. Every position that USES the
/// value keeps the join.
#[test]
fn one_line_if_statement_branches_need_not_join() {
    let fns = "fn pi() -> int:\n    return 1\nfn ps() -> str:\n    return \"s\"\nfn pr() -> int!str:\n    return 1\nfn po() -> int?:\n    return None\n";
    for src in [
        "c := false\nif c: 1 else: \"a\"\nprint(\"end\")\n",
        "c := false\nxs := [1, 2]\nif c: print(1) else: xs.len()\nprint(\"end\")\n",
        "a := false\nb := true\nif a: 1 elif b: \"s\" else: [1]\n",
        "a := false\nb := false\nif a: 1 else: if b: \"s\" else: print(0)\n",
        "c := false\n(if c: print(1) else: 2)\n",
        // a `recover:` block has no tail value: its last statement is a discarded one-line `if`
        "c := false\nr := recover:\n    if c: 1 else: \"a\"\n",
        // an `Any` argument (print's `...args: Any`) joins both branches, as `fn oa(x: Any)` does
        "c := false\nprint(if c: 1 else: \"a\")\n",
    ] {
        ok(src);
    }
    for src in [
        "c := false\nx := if c: 1 else: \"a\"\n",
        "fn ti(x: int) -> int:\n    return x\nc := false\nprint(ti(if c: 1 else: \"a\"))\n",
        "fn f(c: bool) -> int: if c: 1 else: \"a\"\n",
        "fn f(c: bool) -> int:\n    return if c: 1 else: \"a\"\n",
        "c := false\nn := 1\nx := match n:\n    1: if c: 1 else: \"a\"\n    _: 2\n",
        "c := false\nf := fn(): if c: pi() else: ps()\n",
        "fn f(c: bool): if c: 1 else: \"a\"\n",
        "c := false\n_ := if c: 1 else: \"a\"\n",
    ] {
        rejects(&format!("{fns}{src}"), "branches have incompatible types");
    }
    rejects("if 1: 2 else: \"a\"\n", "if condition must be bool");
    rejects(
        "c := false\nif c: 1 else: undefined_name()\n",
        "unknown name",
    );
    warns(
        &format!("{fns}c := true\nif c: pr() else: pi()\n"),
        "returned by 'pr' is discarded",
    );
    warns(
        &format!("{fns}c := false\nif c: pi() else: po()\n"),
        "returned by 'po' is discarded",
    );
    entry_warns(
        "import Shared from std.concurrency\nfn main():\n    c := true\n    s := Shared[List[int]]([])\n    if c: s.get().push(1) else: print(0)\nmain()\n",
        "update",
    );
}

/// TICKET-243: the container methods leave `RwShared`; the collections in `std.concurrency.collection`
/// own them. Every removed method is a `no method` error whose help names the collection type.
#[test]
fn rwshared_container_methods_are_not_public() {
    let cases: &[(&str, &str, &str, &str)] = &[
        (
            "[1, 2]",
            "len()",
            "ConcurrentList",
            "use ConcurrentList from std.concurrency.collection",
        ),
        (
            "[1, 2]",
            "at(0)",
            "ConcurrentList",
            "use ConcurrentList from std.concurrency.collection",
        ),
        (
            "[1, 2]",
            "slice(0, 1)",
            "ConcurrentList",
            "use ConcurrentList from std.concurrency.collection",
        ),
        (
            "[1, 2]",
            "for_each(fn(x): print(x))",
            "ConcurrentList",
            "use ConcurrentList from std.concurrency.collection",
        ),
        (
            "[1, 2]",
            "fold(0, fn(a, x): a + x)",
            "ConcurrentList",
            "use ConcurrentList from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "len()",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "get_key(\"a\")",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "has(\"a\")",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "set_key(\"a\", 2)",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "remove_key(\"a\")",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "for_each_entry(fn(k, v): print(k))",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "fold_entries(0, fn(a, k, v): a + v)",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{\"a\": 1}",
            "get_or_insert(\"b\", 2)",
            "ConcurrentMap",
            "use ConcurrentMap from std.concurrency.collection",
        ),
        (
            "{1, 2}",
            "len()",
            "ConcurrentSet",
            "use ConcurrentSet from std.concurrency.collection",
        ),
        (
            "{1, 2}",
            "contains(1)",
            "ConcurrentSet",
            "use ConcurrentSet from std.concurrency.collection",
        ),
        (
            "{1, 2}",
            "for_each(fn(x): print(x))",
            "ConcurrentSet",
            "use ConcurrentSet from std.concurrency.collection",
        ),
        (
            "{1, 2}",
            "fold(0, fn(a, x): a + x)",
            "ConcurrentSet",
            "use ConcurrentSet from std.concurrency.collection",
        ),
    ];
    for (init, call, _ty, help) in cases {
        let src = format!(
            "import RwShared from std.concurrency\nfn main():\n    r := RwShared({init})\n    r.{call}\nmain()\n"
        );
        let errs = check_entry(&src);
        assert!(
            errs.iter().any(|e| e.message.contains("has no method")
                && e.help.as_deref().is_some_and(|h| h.contains(help))),
            "r.{call}: expected `has no method` with help containing {help:?}, got: {errs:?}"
        );
    }
}
