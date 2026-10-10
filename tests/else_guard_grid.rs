//! TICKET-228 (D4): the `else` guard, `v := f() else e: <block>`. It binds `v` on success; on
//! failure the block runs and must leave. Carrier {`T?`, `T!E`, `None!E`, `int?!E`} x position
//! {fn, nested fn, top level, loop, generator, `spawn:`, `defer:`} x block {each leave statement,
//! falls through, produces a value}. One generated program per cell, run through the built
//! `chezzi` binary; every accept cell RUNS.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LIB: &str = "import std.os
fn o(n: int) -> int?:
    if n > 0:
        return n
    return None
fn r(n: int) -> int!str:
    if n > 0:
        return n
    return !\"bad\"
fn save(n: int) -> None!str:
    if n > 0:
        return
    return !\"full\"
fn deep(n: int) -> int?!str:
    if n > 1:
        x: int? = n
        return ?x
    if n == 1:
        y: int? = None
        return ?y
    return !\"deep\"
";

const MUST_LEAVE: &str = "else block must leave (return, break, continue, panic)";

fn cell(name: &str, body: &str, expect: Expect) -> Cell {
    Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), format!("{LIB}{body}\n"))],
        expect,
    }
}

fn prints(name: &str, body: &str, want: &str) -> Cell {
    cell(name, body, Expect::Prints(want.to_string()))
}

fn rejects(name: &str, body: &str, frag: &'static str) -> Cell {
    cell(name, body, Expect::Rejects(frag))
}

#[test]
fn else_guard_grid() {
    let cells = vec![
        // ---- fn position, each carrier, `return`.
        prints(
            "fn T!E return",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        print(e)\n        return -1\n    return v\nprint(f(1))\nprint(f(0))",
            "1\nbad\n-1",
        ),
        prints(
            "fn T? return",
            "fn f(n: int) -> int:\n    v := o(n) else:\n        return -1\n    return v\nprint(f(4))\nprint(f(0))",
            "4\n-1",
        ),
        prints(
            "fn T!E else without a name ignores the error",
            "fn f(n: int) -> int:\n    v := r(n) else:\n        return -1\n    return v\nprint(f(4))\nprint(f(0))",
            "4\n-1",
        ),
        prints(
            "fn T!E return !e, inline block",
            "fn f(n: int) -> int!str:\n    v := r(n) else e: return !e\n    return v + 1\nmatch f(0):\n    ?v:\n        print(v)\n    !e:\n        print(e)\nmatch f(1):\n    ?v:\n        print(v)\n    !e:\n        print(e)",
            "bad\n2",
        ),
        prints(
            "fn typed let",
            "fn f(n: int) -> int:\n    v: int = r(n) else e:\n        return 0\n    return v\nprint(f(3))\nprint(f(0))",
            "3\n0",
        ),
        prints(
            "fn None!E statement form",
            "fn f(n: int) -> str:\n    save(n) else e:\n        return e\n    return \"saved\"\nprint(f(1))\nprint(f(0))",
            "saved\nfull",
        ),
        prints(
            "fn None!E discard form",
            "fn f(n: int) -> str:\n    _ := save(n) else e:\n        return e\n    return \"saved\"\nprint(f(1))\nprint(f(0))",
            "saved\nfull",
        ),
        prints(
            "fn int?!E binds int?",
            "fn f(n: int) -> str:\n    v := deep(n) else e:\n        return e\n    match v:\n        ?x:\n            return \"some\"\n        None:\n            return \"none\"\nprint(f(2))\nprint(f(1))\nprint(f(0))",
            "some\nnone\ndeep",
        ),
        // ---- the other leave statements.
        prints(
            "panic leaves",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        panic(e)\n    return v\nprint(f(2))\nres := recover: f(0)\nmatch res:\n    ?v:\n        print(v)\n    !e:\n        print(e.message())",
            "2\nbad",
        ),
        prints(
            "os.exit leaves",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        os.exit(3)\n    return v\nprint(f(2))",
            "2",
        ),
        rejects(
            "os.exit runs on failure",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        os.exit(3)\n    return v\nprint(f(0))",
            "",
        ),
        prints(
            "if/else whose every branch leaves",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        if e == \"bad\":\n            return -1\n        else:\n            panic(e)\n    return v\nprint(f(0))",
            "-1",
        ),
        prints(
            "match whose every arm leaves",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        match e:\n            \"bad\":\n                return -1\n            _:\n                return -2\n    return v\nprint(f(0))",
            "-1",
        ),
        // ---- loop position: break, continue, and a capture of `e` and the loop variable.
        prints(
            "loop continue and break",
            "fn main():\n    for i in [1, 0, 2, -1, 3]:\n        if i < 0:\n            v := o(i) else:\n                break\n            print(v)\n        v := r(i) else e:\n            print(e)\n            continue\n        print(v)\nmain()",
            "1\nbad\n2",
        ),
        prints(
            "loop capture",
            "fn main():\n    for i in [0, 1]:\n        v := r(i) else e:\n            g := fn() -> str: \"{e}{i}\"\n            print(g())\n            continue\n        h := fn() -> int: v + i\n        print(h())\nmain()",
            "bad0\n2",
        ),
        // ---- nested fn position: `return` leaves the nested fn only.
        prints(
            "nested fn",
            "fn main():\n    base := 10\n    fn inner(n: int) -> int:\n        v := r(n) else e:\n            return base\n        return v\n    print(inner(0))\n    print(inner(5))\nmain()",
            "10\n5",
        ),
        // ---- top level: no `return` there.
        prints(
            "top level panic and binding",
            "v := r(7) else e:\n    panic(e)\nprint(v)",
            "7",
        ),
        prints(
            "top level loop",
            "for i in [0, 3]:\n    v := o(i) else:\n        continue\n    print(v)",
            "3",
        ),
        rejects(
            "top level return",
            "v := r(7) else e:\n    return\nprint(v)",
            "'return' outside a function",
        ),
        // ---- generator: `return` ends it.
        prints(
            "generator",
            "fn gen() -> Iterator[int]:\n    for i in [1, 2, 0, 4]:\n        v := r(i) else e:\n            return\n        yield v\nfor x in gen():\n    print(x)",
            "1\n2",
        ),
        // ---- spawn and defer blocks: rejected.
        rejects(
            "spawn block",
            "fn main():\n    parallel:\n        spawn:\n            v := r(1) else e:\n                return\n            print(v)\nmain()",
            "'else' guard is not allowed inside a spawn block: a spawned task has no caller to leave to",
        ),
        rejects(
            "defer block",
            "fn main():\n    defer:\n        v := r(1) else e:\n            panic(e)\n        print(v)\nmain()",
            "'else' guard is not allowed inside a defer block: a defer cannot leave",
        ),
        // ---- the block must leave, and never produces a value.
        rejects(
            "falls through",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        print(e)\n    return v\nprint(f(1))",
            MUST_LEAVE,
        ),
        rejects(
            "produces a value",
            "fn f(n: int) -> int:\n    v := r(n) else e: 5\n    return v\nprint(f(1))",
            MUST_LEAVE,
        ),
        rejects(
            "if without else falls through",
            "fn f(n: int) -> int:\n    v := r(n) else e:\n        if n == 0:\n            return 0\n    return v\nprint(f(1))",
            MUST_LEAVE,
        ),
        rejects(
            "None!E call still must leave",
            "fn f(n: int):\n    save(n) else e:\n        print(e)\nf(1)",
            MUST_LEAVE,
        ),
        rejects(
            "top level falls through",
            "v := o(1) else:\n    print(0)\nprint(v)",
            MUST_LEAVE,
        ),
        // ---- the operand and the binder must fit.
        rejects(
            "None!E success bound to a name",
            "fn f(n: int) -> int:\n    v := save(n) else e:\n        return 0\n    return 1\nprint(f(1))",
            "expression returns no value (None) and cannot be used as a value",
        ),
        rejects(
            "else e on T?",
            "fn f(n: int) -> int:\n    v := o(n) else e:\n        return 0\n    return v\nprint(f(1))",
            "`else e:` needs an error to bind, and int? has none; write `else:`",
        ),
        rejects(
            "else on int",
            "fn f(n: int) -> int:\n    v := n else:\n        return 0\n    return v\nprint(f(1))",
            "`else` needs a `T?` or `T!E` value, found int",
        ),
    ];
    run_grid("else-guard", &cells);
}

/// The tuple carriers of the destructuring rows, appended to [`LIB`].
const LIB2: &str = "fn o2(n: int) -> (int, int)?:
    if n > 0:
        return (n, n + 1)
    return None
fn r2(n: int) -> (int, int)!str:
    if n > 0:
        return (n, n + 1)
    return !\"bad\"
";

const NO_VALUE: &str = "expression returns no value (None) and cannot be used as a value";
const WALRUS_NAME: &str = "left side of ':=' must be a name";
const GUARD_PLACE: &str =
    "an `else` guard belongs on a `:=` binding or a bare call, not on an assignment or `return`";

/// TICKET-241: binding form x carrier. The guard exists on a let (one name or a destructuring)
/// and on a bare call; an assignment and `return` refuse it with one text. Every accept cell
/// runs the success path and the failure path.
#[test]
fn else_guard_binding_form_grid() {
    // What a cell does: run and print `ok` then `-1`, or reject with a fragment.
    enum Want {
        Runs(&'static str),
        Rejects(&'static str),
    }
    use Want::{Rejects, Runs};
    // (carrier, one-value call, pair call, guard head)
    let carriers = [
        ("T?", "o(n)", "o2(n)", "else:"),
        ("T!E", "r(n)", "r2(n)", "else e:"),
        ("None!E", "save(n)", "save(n)", "else e:"),
    ];
    // (form, setup line, statement head, takes the pair call, value returned on success,
    //  verdict per carrier in the order above)
    #[allow(clippy::type_complexity)]
    let forms: [(&str, &str, &str, bool, &str, [Want; 3]); 13] = [
        (
            "x :=",
            "",
            "x := ",
            false,
            "x",
            [Runs("3"), Runs("3"), Rejects(NO_VALUE)],
        ),
        (
            "x: T =",
            "",
            "x: int = ",
            false,
            "x",
            [Runs("3"), Runs("3"), Rejects(NO_VALUE)],
        ),
        (
            "x: const T =",
            "",
            "x: const int = ",
            false,
            "x",
            [Runs("3"), Runs("3"), Rejects(NO_VALUE)],
        ),
        (
            "_ :=",
            "",
            "_ := ",
            false,
            "1",
            [Runs("1"), Runs("1"), Runs("1")],
        ),
        (
            "bare call",
            "",
            "",
            false,
            "1",
            [Runs("1"), Runs("1"), Runs("1")],
        ),
        (
            "a, b :=",
            "",
            "a, b := ",
            true,
            "a + b",
            [Runs("7"), Runs("7"), Rejects(NO_VALUE)],
        ),
        (
            "(a, b) :=",
            "",
            "(a, b) := ",
            true,
            "1",
            [
                Rejects(WALRUS_NAME),
                Rejects(WALRUS_NAME),
                Rejects(WALRUS_NAME),
            ],
        ),
        (
            "x =",
            "    x := 0\n",
            "x = ",
            false,
            "x",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
        (
            "x +=",
            "    x := 0\n",
            "x += ",
            false,
            "x",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
        (
            "a, b =",
            "    a := 0\n    b := 0\n",
            "a, b = ",
            true,
            "a + b",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
        (
            "self.f =",
            "",
            "self.f = ",
            false,
            "1",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
        (
            "xs[i] =",
            "    xs := [0]\n",
            "xs[0] = ",
            false,
            "xs[0]",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
        (
            "return",
            "",
            "return ",
            false,
            "1",
            [
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
                Rejects(GUARD_PLACE),
            ],
        ),
    ];
    let mut cells = Vec::new();
    for (form, setup, head, pair, result, wants) in forms {
        for ((carrier, one, two, guard), want) in carriers.iter().zip(wants) {
            let call = if pair { two } else { one };
            // The typed-let rows declare the carrier's own success type.
            let head = if *carrier == "None!E" {
                head.replace("int", "None")
            } else {
                head.to_string()
            };
            let body = format!(
                "{LIB2}fn f(n: int) -> int:\n{setup}    {head}{call} {guard}\n        return -1\n    return {result}\nprint(f(3))\nprint(f(0))"
            );
            let name = format!("{form} x {carrier}");
            cells.push(match want {
                Runs(ok) => prints(&name, &body, &format!("{ok}\n-1")),
                Rejects(frag) => rejects(&name, &body, frag),
            });
        }
    }
    assert_eq!(cells.len(), 39);
    run_grid("else-guard-binding-form", &cells);
}
