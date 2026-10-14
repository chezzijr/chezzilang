//! TICKET-242: the one-line `if … else` statement is the if-expression with its value discarded,
//! and the inline `recover: <expr>` is an ordinary expression (TICKET-260 lifted the TICKET-242 position limits). Form {calls,
//! `elif`, nested, literals, assignment, `return`, guards, `recover:`, block-expression bodies} x
//! position {top level, fn body, loop body, inline fn body, inline loop body, statement `match`
//! arm, match-expression arm, parentheses, call argument}. One generated program per cell, run
//! through the built `chezzi` binary; every accept cell RUNS.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LIB: &str = "fn g() -> int?:
    return None
fn k() -> int!str:
    return !\"bad\"
fn ok1() -> int!str:
    print(\"k\")
    return 1
t := true
f := false
x := 0
";

/// The six helper fns of the mixed-type table; each prints its name.
const FNS: &str = "fn pi() -> int:
    print(\"pi\")
    return 1
fn ps() -> str:
    print(\"ps\")
    return \"s\"
fn pn():
    print(\"pn\")
fn pl() -> List[int]:
    print(\"pl\")
    return [1]
fn pr() -> int!str:
    print(\"pr\")
    return 1
fn po() -> int?:
    print(\"po\")
    return None
";

/// A cell's verdict in table form: `=out` prints `out`, `!frag` rejects with `frag`.
fn verdict(v: &'static str) -> Expect {
    match v.strip_prefix('!') {
        Some(frag) => Expect::Rejects(frag),
        None => Expect::Prints(v.strip_prefix('=').expect("`=out` or `!frag`").to_string()),
    }
}

fn cell(name: &str, lib: &str, body: &str, v: &'static str) -> Cell {
    Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), format!("{lib}{body}\n"))],
        expect: verdict(v),
    }
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One single-line form and its verdict in each position. `shows` is true when the form has a
/// value worth printing: the value positions then print it.
struct Form {
    name: &'static str,
    stmt: &'static str,
    shows: bool,
    /// top level, indented fn body, indented loop body: one verdict, the form is a statement there.
    statement: &'static str,
    inline_fn: &'static str,
    inline_loop: &'static str,
    stmt_arm: &'static str,
    expr_arm: &'static str,
    paren: &'static str,
    arg: &'static str,
}

const FORMS: &[Form] = &[
    Form {
        name: "if/else calls",
        stmt: "if f: print(1) else: print(2)",
        shows: false,
        statement: "=2",
        inline_fn: "=2",
        inline_loop: "=2",
        stmt_arm: "=2",
        expr_arm: "!expression returns no value (None) and cannot be used as a value",
        paren: "=2",
        arg: "!expression returns no value (None) and cannot be used as a value",
    },
    Form {
        name: "if/elif/else calls",
        stmt: "if f: print(1) elif t: print(2) else: print(3)",
        shows: false,
        statement: "=2",
        inline_fn: "=2",
        inline_loop: "=2",
        stmt_arm: "=2",
        expr_arm: "!expression returns no value (None) and cannot be used as a value",
        paren: "=2",
        arg: "!expression returns no value (None) and cannot be used as a value",
    },
    Form {
        name: "nested ternary in the else branch",
        stmt: "if f: print(1) else: if f: print(2) else: print(3)",
        shows: false,
        statement: "=3",
        inline_fn: "=3",
        inline_loop: "=3",
        stmt_arm: "=3",
        expr_arm: "!expression returns no value (None) and cannot be used as a value",
        paren: "=3",
        arg: "!expression returns no value (None) and cannot be used as a value",
    },
    Form {
        name: "literal branches",
        stmt: "if f: 1 else: 2",
        shows: true,
        statement: "=",
        inline_fn: "=2",
        inline_loop: "=",
        stmt_arm: "=",
        expr_arm: "=2",
        paren: "=2",
        arg: "=2",
    },
    Form {
        name: "assignment branch",
        stmt: "if f: x = 1 else: x = 2",
        shows: false,
        statement: "!takes an expression in each branch",
        inline_fn: "!expected 'else', found '='",
        inline_loop: "!expected 'else', found '='",
        stmt_arm: "!expected 'else', found '='",
        expr_arm: "!expected 'else', found '='",
        paren: "!expected 'else', found '='",
        arg: "!expected 'else', found '='",
    },
    Form {
        name: "assignment in the else branch",
        stmt: "if f: print(1) else: x = 2",
        shows: false,
        statement: "!takes an expression in each branch",
        inline_fn: "!invalid assignment target",
        inline_loop: "!invalid assignment target",
        stmt_arm: "!invalid assignment target",
        // the arm ends after the else branch: the arm-end check rejects `= 2`
        expr_arm: "!expected end of line, found '='",
        paren: "!expected ')', found '='",
        arg: "!expected ')', found '='",
    },
    Form {
        name: "return branch",
        stmt: "if f: return 1 else: return 2",
        shows: false,
        statement: "!takes an expression in each branch",
        inline_fn: "!takes an expression in each branch",
        inline_loop: "!takes an expression in each branch",
        stmt_arm: "!takes an expression in each branch",
        expr_arm: "!takes an expression in each branch",
        paren: "!takes an expression in each branch",
        arg: "!takes an expression in each branch",
    },
    Form {
        name: "return in the else branch",
        stmt: "if f: print(1) else: return 2",
        shows: false,
        statement: "!takes an expression in each branch",
        inline_fn: "!takes an expression in each branch",
        inline_loop: "!takes an expression in each branch",
        stmt_arm: "!takes an expression in each branch",
        expr_arm: "!takes an expression in each branch",
        paren: "!takes an expression in each branch",
        arg: "!takes an expression in each branch",
    },
    Form {
        name: "bare-else guard",
        stmt: "if t: g() else: return",
        shows: false,
        statement: "!takes an expression in each branch",
        inline_fn: "!takes an expression in each branch",
        inline_loop: "!takes an expression in each branch",
        stmt_arm: "!takes an expression in each branch",
        expr_arm: "!takes an expression in each branch",
        paren: "!takes an expression in each branch",
        arg: "!takes an expression in each branch",
    },
    Form {
        name: "else e: guard",
        stmt: "if t: ok1() else e: panic(e)",
        shows: false,
        statement: "=k",
        // an if-expression has no guard: its `else` takes a ':'
        inline_fn: "!expected ':', found identifier 'e'",
        inline_loop: "!expected ':', found identifier 'e'",
        stmt_arm: "!expected ':', found identifier 'e'",
        expr_arm: "!expected ':', found identifier 'e'",
        paren: "!expected ':', found identifier 'e'",
        arg: "!expected ':', found identifier 'e'",
    },
    Form {
        name: "recover: <expr>",
        stmt: "recover: 1",
        shows: true,
        statement: "=",
        inline_fn: "=1",
        inline_loop: "=",
        stmt_arm: "=",
        expr_arm: "=1",
        paren: "=1",
        arg: "=1",
    },
];

#[test]
fn form_by_position_grid() {
    let mut cells = Vec::new();
    for form in FORMS {
        let s = form.stmt;
        let show = |call: &str| {
            if form.shows {
                format!("print({call})")
            } else {
                call.to_string()
            }
        };
        let programs = [
            ("top level", s.to_string(), form.statement),
            ("fn body", format!("fn h():\n    {s}\nh()"), form.statement),
            (
                "loop body",
                format!("for _i in 0..1:\n    {s}"),
                form.statement,
            ),
            (
                "inline fn body",
                format!("fn h(): {s}\n{}", show("h()")),
                form.inline_fn,
            ),
            (
                "inline loop body",
                format!("for _i in 0..1: {s}"),
                form.inline_loop,
            ),
            (
                "statement match arm",
                format!("match 1:\n    1: {s}\n    _: pass"),
                form.stmt_arm,
            ),
            (
                "match-expression arm",
                format!(
                    "v := match 1:\n    1: {s}\n    _: {s}\n{}",
                    if form.shows { "print(v)" } else { "" }
                ),
                form.expr_arm,
            ),
            ("parentheses", show(&format!("({s})")), form.paren),
            ("call argument", format!("print({s})"), form.arg),
        ];
        for (position, body, v) in programs {
            cells.push(cell(&format!("{} / {position}", form.name), LIB, &body, v));
        }
    }

    // The three block-expression bodies need layout, so they exist in the three indented-capable
    // positions only: top level, fn body, loop body. A dedented `elif` / `else:` is not on the
    // `if` line. After the block a dedented `else:` is the `if`'s own, never a guard on `y`
    // (TICKET-249), so it runs exactly when the condition is false.
    let arms = "    1: 10\n    _: 20";
    let opt_arms = "    1: g()\n    _: g()";
    let elif = format!("n := 1\nif f: y := match n:\n{arms}\nelif t:\n    print(2)");
    let rec = "if f: r := recover:\n    1 / 0\nelif true:\n    print(4)".to_string();
    for (name, body, v) in [
        ("top level", elif.clone(), "=2"),
        ("fn body", format!("fn h():\n{}\nh()", indent(&elif)), "=2"),
        (
            "loop body",
            format!("for i in 0..1:\n{}", indent(&elif)),
            "=2",
        ),
    ] {
        cells.push(cell(
            &format!("match body then dedented elif / {name}"),
            LIB,
            &body,
            v,
        ));
    }
    for (name, body, v) in [
        ("top level", rec.clone(), "=4"),
        ("fn body", format!("fn h():\n{}\nh()", indent(&rec)), "=4"),
        (
            "loop body",
            format!("for i in 0..1:\n{}", indent(&rec)),
            "=4",
        ),
    ] {
        cells.push(cell(
            &format!("recover body then dedented elif / {name}"),
            LIB,
            &body,
            v,
        ));
    }
    let guard = |leave: &str| {
        format!("n := 1\nif f: y := match n:\n{opt_arms}\nelse:\n    {leave}\nprint(\"after\")")
    };
    cells.push(cell(
        "match body then dedented else of the if / top level",
        LIB,
        &guard("print(\"else\")"),
        "=else\nafter",
    ));
    cells.push(cell(
        "match body then dedented else of the if / fn body",
        LIB,
        "fn h(c: bool, n: int) -> int:\n    if c: y := match n:\n        1: g()\n        _: g()\n    else:\n        return 7\n    return 8\nprint(h(true, 1), h(false, 1))",
        "=8 7",
    ));
    cells.push(cell(
        "match body then dedented else of the if / loop body",
        LIB,
        &format!("for i in 0..2:\n{}", indent(&guard("print(\"else\")"))),
        "=else\nafter\nelse\nafter",
    ));
    cells.push(cell(
        "match body of int arms then dedented else / top level",
        LIB,
        &format!("n := 1\nif f: y := match n:\n{arms}\nelse:\n    print(\"else\")"),
        "=else",
    ));
    run_grid("inline-if-form", &cells);
}

/// Row 5: `recover: <expr>` is an ordinary expression in every position; its body is one expression.
#[test]
fn recover_inline_grid() {
    const F: &str = "fn two(a: int!Error, b: int):\n    print(b)\nfn calls(g: fn() -> int!Error):\n    print(g())\n";
    let table: &[(&str, &'static str)] = &[
        ("print(recover: 1)", "=1"),
        ("if (recover: 1) == ?1:\n    print(\"y\")", "=y"),
        ("print((recover: 1), 2)", "=1 2"),
        ("print(recover: 1 / 0)", "=!division by zero"),
        ("two(recover: 1, 2)", "=2"),
        ("two(b=3, a=recover: 1)", "=3"),
        ("y := (recover: 1 / 0)\nprint(y)", "=!division by zero"),
        ("print(recover: 1 + 2, recover: 3)", "=3 3"),
        ("print(recover: (recover: 1))", "=1"),
        ("print(recover: 1,\n2)", "=1 2"),
        // not a whole value: an operand, a closure body, an if-expression branch, a tuple element
        ("print(1 + recover: 1)", "!cannot apply + to int and int!"),
        ("y := (?1 == recover: 1)", "="),
        (
            "print(not recover: true)",
            "!'not' expects bool, found bool!",
        ),
        ("calls(fn(): recover: 1 / 0)", "=!division by zero"),
        (
            "y := (if t: recover: 1 else: 2)",
            "!branches have incompatible types",
        ),
        ("y := (recover: 1, 2)", "="),
        ("print(recover: x = 1)", "!expected ')', found '='"),
        ("xs := [recover: 1]", "="),
        (
            "m := {recover: 1}",
            "!set element type must implement Hashable",
        ),
        (
            "xs := [1]\nprint(xs[recover: 0])",
            "!index must be int, found int!",
        ),
        ("print(recover: recover: 1)", "=1"),
        (
            "print(2, 1 + recover: 1)",
            "!cannot apply + to int and int!",
        ),
        ("y := (1, recover: 2)", "="),
        (
            "fn q():\n    y := (recover: return)",
            "!unexpected 'return' in expression",
        ),
    ];
    let lib = format!("{LIB}{F}");
    let cells: Vec<Cell> = table
        .iter()
        .map(|(body, v)| cell(body, &lib, body, v))
        .collect();
    run_grid("inline-if-recover", &cells);
}

/// Whose `else` is it: the `if` line, a nested block on it, a later line, a bare `return`.
#[test]
fn else_owner_grid() {
    // each body is the statement inside `fn h():`, followed by `print("after")`
    let in_h: &[(&str, &'static str)] = &[
        (
            "if f:\n    print(0)\nelif t: print(1) else: print(2)",
            "!an `elif` body ends its line",
        ),
        (
            "if f: print(0)\nelif t: print(1) else: print(2)",
            "!an `elif` body ends its line",
        ),
        (
            "if f: print(0)\nelif t: print(1) elif true: print(2)",
            "!an `elif` body ends its line",
        ),
        (
            "if false: print(0)\nelif true: g() else: return",
            "!an `elif` body ends its line",
        ),
        (
            "if f: print(1)\nelif f: return else: return",
            "!an `elif` body ends its line",
        ),
        // the inline `recover:` is an ordinary expression: these parse, and only the unused `y` warns
        (
            "if t: y := if f: recover: 1 else: 2",
            "!unused variable 'y'",
        ),
        ("y := if t: recover: 1 else: 2", "!unused variable 'y'"),
        (
            "if t: print(if f: recover: 1 else: 2)",
            "!branches have incompatible types",
        ),
        (
            "if t: y := (if f: recover: 1 else: 2) else: print(3)",
            "!takes an expression in each branch",
        ),
        (
            "if true: r := recover: g() else: return",
            "!takes an expression in each branch",
        ),
        (
            "if true: k() else e: g() else: return",
            "!else block must leave",
        ),
        (
            "if true: fv := fn(): g() else: return",
            "!takes an expression in each branch",
        ),
        (
            "if f: return else: print(2)",
            "!takes an expression in each branch",
        ),
        (
            "if f: return else: return",
            "!takes an expression in each branch",
        ),
        // a nested inline block on the `if` line keeps its own `else:` guard
        (
            "if true: _ := recover: g() else: panic(\"p\")",
            "!takes an expression in each branch",
        ),
        (
            "if false: print(1)\nelif true: r := recover: g() else: panic(\"p\")",
            "!an `elif` body ends its line",
        ),
        (
            "if true:\n    r := recover: g() else: panic(\"p\")\n    print(r)",
            "=None\nafter",
        ),
        (
            "if true: r := recover:\n    g() else: panic(\"p\")\nelif true:\n    print(1)",
            "=after",
        ),
        // the `if`'s own `else` after a bracket that holds an if-expression or a `recover:`
        ("if f: print((if f: 1 else: 2)) else: print(3)", "=3\nafter"),
        ("if f: print(if f: 1 else: 2) else: print(3)", "=3\nafter"),
        (
            "if f: print(recover: 1) else: print(recover: 2)",
            "=2\nafter",
        ),
        // a later line is not the `if` line; a bracket that spans lines is one line
        (
            "if f: print(0)\nelif t: print(1)\nelse: print(2)",
            "=1\nafter",
        ),
        ("if f: print(1,\n2) else: print(7)", "=7\nafter"),
        ("if f: return\nelse: print(2)", "=2\nafter"),
    ];
    let cells: Vec<Cell> = in_h
        .iter()
        .map(|(body, v)| {
            let program = format!("fn h():\n{}\n    print(\"after\")\nh()", indent(body));
            cell(body, LIB, &program, v)
        })
        .collect();
    run_grid("inline-if-else-owner", &cells);
}

/// A one-line `if … else` statement discards its value, so its branches need not join; every
/// position that uses the value keeps the join.
#[test]
fn mixed_branch_types_grid() {
    let table: &[(&str, &str, &'static str)] = &[
        ("top level", "c := false\nif c: pi() else: ps()", "=ps"),
        ("None and List", "c := false\nif c: pn() else: pl()", "=pl"),
        (
            "elif, second taken",
            "a := false\nb := true\nif a: pi() elif b: ps() else: pl()",
            "=ps",
        ),
        (
            "elif, else taken",
            "a := false\nb := false\nif a: pi() elif b: ps() else: pl()",
            "=pl",
        ),
        (
            "nested",
            "a := false\nb := false\nif a: pi() else: if b: ps() else: pn()",
            "=pn",
        ),
        (
            "fn body",
            "fn h(c: bool):\n    if c: pi() else: ps()\n    print(\"after\")\nh(false)\nh(true)",
            "=ps\nafter\npi\nafter",
        ),
        (
            "loop body",
            "for i in 0..3:\n    if i == 0: pi() elif i == 1: ps() else: pl()",
            "=pi\nps\npl",
        ),
        (
            "parentheses as a statement",
            "c := false\n(if c: pi() else: ps())",
            "=ps",
        ),
        ("T!E branch", "c := true\nif c: pr() else: pi()", "=pr"),
        ("T? branch", "c := false\nif c: pi() else: po()", "=po"),
        (
            "literals",
            "c := false\nif c: 1 else: \"a\"\nprint(\"end\")",
            "=end",
        ),
        (
            "None and int",
            "c := false\nxs := [1, 2]\nif c: print(1) else: xs.len()\nprint(\"end\")",
            "=end",
        ),
        (
            "parenthesized branch",
            "c := true\nd := false\nif c: (if d: 1 else: \"a\") else: pl()\nprint(\"end\")",
            "=end",
        ),
        (
            "`?` in a branch",
            "fn h(c: bool) -> int!str:\n    if c: pr()? else: ps()\n    return 2\nprint(h(true))\nprint(h(false))",
            "=pr\n2\nps\n2",
        ),
        (
            "statement in a recover block",
            "c := false\nr := recover:\n    if c: pi() else: ps()\n    3\nprint(r)",
            "=ps\nNone",
        ),
        (
            "inline loop body, parenthesized",
            "c := false\nfor i in 0..1: (if c: pi() else: ps())",
            "=ps",
        ),
        (
            "statement match arm, parenthesized",
            "c := false\nn := 1\nmatch n:\n    1: (if c: pi() else: ps())\n    _: pass",
            "=ps",
        ),
        // the value is used: the join stays
        (
            "let value",
            "c := false\ny := if c: 1 else: \"a\"",
            "!branches have incompatible types",
        ),
        (
            "call argument",
            "c := false\nprint(if c: 1 else: \"a\")",
            "!branches have incompatible types",
        ),
        (
            "inline fn body, declared return",
            "fn h(c: bool) -> int: if c: 1 else: \"a\"",
            "!branches have incompatible types",
        ),
        (
            "inline fn body",
            "fn h(c: bool): if c: pi() else: ps()\nh(false)",
            "!branches have incompatible types",
        ),
        (
            "return value",
            "fn h(c: bool) -> int:\n    return if c: 1 else: \"a\"",
            "!branches have incompatible types",
        ),
        (
            "match-expression arm",
            "c := false\nn := 1\ny := match n:\n    1: if c: 1 else: \"a\"\n    _: 2",
            "!branches have incompatible types",
        ),
        (
            "recover block tail",
            "c := false\nr := recover:\n    if c: 1 else: \"a\"\nprint(r)",
            "=None",
        ),
        (
            "closure body",
            "c := false\nfv := fn(): if c: pi() else: ps()",
            "!branches have incompatible types",
        ),
        (
            "discard binding",
            "c := false\n_ := if c: 1 else: \"a\"",
            "!branches have incompatible types",
        ),
        // an error inside a branch still reports
        (
            "condition type",
            "if 1: pi() else: ps()",
            "!if condition must be bool",
        ),
        (
            "unknown name in a branch",
            "c := false\nif c: pi() else: undefined_name()",
            "!unknown name 'undefined_name'",
        ),
    ];
    let cells: Vec<Cell> = table
        .iter()
        .map(|(name, body, v)| cell(name, FNS, body, v))
        .collect();
    run_grid("inline-if-mixed", &cells);
}
