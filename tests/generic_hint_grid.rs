//! TICKET-264 — an outer hint never fills a type parameter with a protocol (owner rule 2026-10-17).
//! One program per (generic expression, position). `T` comes from the call's own arguments; an
//! outer hint fills only a `T` they leave open, and only with a concrete type. `Any` is an
//! ordinary empty protocol, so the `print` pack, an `Any` param, a `Stringable` param and
//! `y: Any = ...` all give the same answer, and a concrete param may fill `T`.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const DECLS: &str = "protocol Default:
    fn default() -> Self
struct Counter:
    n: int
    fn default() -> Counter:
        return Counter(7)
fn pick[T](a: T, b: T) -> T:
    return a
fn un[T](o: T?, d: T) -> T:
    return o ?? d
fn ident[T](x: T) -> T:
    return x
fn apply[T](x: T, f: fn(T) -> T) -> T:
    return f(x)
fn empty[T: Default]() -> T:
    return T.default()
struct Holder:
    k: int
    fn build[T: Default]() -> T:
        return T.default()
fn tag[U](xs: List[U]) -> List[U]:
    return xs
fn oa(v: Any) -> str:
    return \"a:{v}\"
fn os(v: Stringable) -> str:
    return \"s:{v}\"
fn oo(v: Any?) -> str:
    return \"o:{v}\"
fn oi(v: int) -> int:
    return v
fn oc(v: Counter) -> int:
    return v.n
fn of(f: fn(int) -> int) -> int:
    return f(4)
fn ol(v: List[int]) -> int:
    return v.len()
";

/// The protocol positions; `X` is the generic expression. A nullable protocol (`Any?`,
/// `Stringable?`) is a protocol position too.
const PROTOCOL_POSITIONS: [(&str, &str); 6] = [
    ("print-pack", "print(X)"),
    ("any-param", "print(oa(X))"),
    ("stringable-param", "print(os(X))"),
    ("any-let", "y: Any = X\n    print(y)"),
    ("nullable-any-param", "print(oo(X))"),
    (
        "nullable-stringable-let",
        "y: Stringable? = X\n    print(y)",
    ),
];

/// The answer of one expression in a protocol position, the `X` prefix of the printed text
/// (`print` prints it bare, `oa` adds `a:`, `os` adds `s:`), or the reject fragment.
enum Ans {
    Prints(&'static str),
    Rejects(&'static str),
}

/// `(name, expression, answer in every protocol position, concrete position, its answer)`.
fn rows() -> Vec<(&'static str, &'static str, Ans, &'static str, Ans)> {
    use Ans::{Prints, Rejects};
    const CONFLICT_PICK: &str = "argument to 'pick' has type float, expected int";
    const CONFLICT_UN: &str = "argument to 'un' has type float, expected int";
    const OPEN: &str = "not determined here";
    vec![
        // Rule 1: a conflict among the arguments is an error, whatever the outer slot is.
        (
            "conflict-pick",
            "pick(5, 1.5)",
            Rejects(CONFLICT_PICK),
            "print(oi(X))",
            Rejects(CONFLICT_PICK),
        ),
        (
            "conflict-un",
            "un(?5, 1.5)",
            Rejects(CONFLICT_UN),
            "print(oi(X))",
            Rejects(CONFLICT_UN),
        ),
        // Rule 2: a protocol hint never fills an open `T`; a concrete one does.
        (
            "open-fn-value",
            "ident",
            Rejects(OPEN),
            "print(of(X))",
            Prints("4"),
        ),
        (
            "open-static-member",
            "Holder.build()",
            Rejects(OPEN),
            "print(oc(X))",
            Prints("7"),
        ),
        (
            "open-return",
            "empty()",
            Rejects(OPEN),
            "print(oc(X))",
            Prints("7"),
        ),
        // Rule 3: inside an open `T`, a closure param takes `T` from the arguments.
        (
            "closure-param",
            "apply(1, fn(a): a + 1)",
            Prints("2"),
            "print(oi(X))",
            Prints("2"),
        ),
        // Must still pass: the arguments agree.
        (
            "agree-pick",
            "pick(5, 6)",
            Prints("5"),
            "print(oi(X))",
            Prints("5"),
        ),
        (
            "agree-un",
            "un(?5, 6)",
            Prints("5"),
            "print(oi(X))",
            Prints("5"),
        ),
        // Must still pass: `[]` leaves `U` open; with no fill the value is the empty list, and a
        // concrete hint fills `U`.
        (
            "open-concrete-list",
            "tag([])",
            Prints("[]"),
            "print(ol(X))",
            Prints("0"),
        ),
    ]
}

fn program(body: &str) -> String {
    format!("{DECLS}fn main():\n    {body}\nmain()\n")
}

fn expect(ans: &Ans, prefix: &str) -> Expect {
    match ans {
        Ans::Prints(v) => Expect::Prints(format!("{prefix}{v}")),
        Ans::Rejects(f) => Expect::Rejects(f),
    }
}

fn cell(name: String, body: String, expect: Expect) -> Cell {
    Cell {
        name,
        files: vec![("main.chz".to_string(), program(&body))],
        expect,
    }
}

#[test]
fn generic_hint_grid() {
    let mut cells = Vec::new();
    for (name, x, ans, concrete, concrete_ans) in rows() {
        for (pos, tmpl) in PROTOCOL_POSITIONS {
            let prefix = match pos {
                "any-param" => "a:",
                "stringable-param" => "s:",
                "nullable-any-param" => "o:",
                _ => "",
            };
            cells.push(cell(
                format!("{name}/{pos}"),
                tmpl.replace('X', x),
                expect(&ans, prefix),
            ));
        }
        cells.push(cell(
            format!("{name}/concrete-param"),
            concrete.replace('X', x),
            expect(&concrete_ans, ""),
        ));
    }
    // Must still pass: an annotated concrete binding fills an open `U` (`ys: List[int] = tag([])`).
    cells.push(cell(
        "open-concrete-list/int-let".to_string(),
        "ys: List[int] = tag([])\n    print(ys)".to_string(),
        Expect::Prints("[]".to_string()),
    ));
    run_grid("generic-hint", &cells, &[]);
}
