//! TICKET-228 (D5): the carrier patterns `?v` (present / success), `!e` (error) and `None`
//! (absent). Scrutinee {`T?`, `T!E`, `T??`, tuple payload, error struct, int} x pattern {`?v`,
//! `!e`, `None`, bare name, `_`, nested forms} x exhaustiveness. One generated program per cell,
//! run through the built `chezzi` binary; every accept cell RUNS.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

/// The functions every cell may call.
const LIB: &str = "struct IoErr:
    code: int
    fn message(self) -> str:
        return \"io\"
enum C:
    Red
    Green
LIMIT: const int = 3
fn o(n: int) -> int?:
    if n > 0:
        return n
    return None
fn r(n: int) -> int!str:
    if n > 0:
        return n
    return !\"bad\"
fn oo(n: int) -> int??:
    if n > 1:
        x: int? = n
        return ?x
    if n == 1:
        y: int? = None
        return ?y
    return None
fn t(n: int) -> (int, int)?:
    if n > 0:
        return (n, n + 1)
    return None
fn io(n: int) -> int!IoErr:
    if n > 0:
        return n
    return !IoErr(7)
";

fn cell(name: &str, body: &str, expect: Expect) -> Cell {
    let mut src = String::from(LIB);
    src.push_str("fn main():\n");
    for line in body.lines() {
        src.push_str("    ");
        src.push_str(line);
        src.push('\n');
    }
    src.push_str("main()\n");
    Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), src)],
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
fn carrier_pattern_grid() {
    let cells = vec![
        // T? : `?v` + `None` is exhaustive.
        prints(
            "T? present",
            "match o(1):\n    ?v:\n        print(v)\n    None:\n        print(\"none\")",
            "1",
        ),
        prints(
            "T? absent",
            "match o(0):\n    ?v:\n        print(v)\n    None:\n        print(\"none\")",
            "none",
        ),
        // T!E : `?v` + `!e` is exhaustive.
        prints(
            "T!E success",
            "match r(1):\n    ?v:\n        print(v)\n    !e:\n        print(e)",
            "1",
        ),
        prints(
            "T!E error",
            "match r(0):\n    ?v:\n        print(v)\n    !e:\n        print(e)",
            "bad",
        ),
        // T?? : nesting, in both spellings.
        prints(
            "T?? ?(?v)",
            "for n in [2, 1, 0]:\n    match oo(n):\n        ?(?v):\n            print(v)\n        ?None:\n            print(\"inner none\")\n        None:\n            print(\"none\")",
            "2\ninner none\nnone",
        ),
        prints(
            "T?? ??v",
            "match oo(5):\n    ??v:\n        print(v)\n    _:\n        print(\"other\")",
            "5",
        ),
        // Tuple payload and error struct.
        prints(
            "?(a, b)",
            "match t(1):\n    ?(a, b):\n        print(a + b)\n    None:\n        print(\"none\")",
            "3",
        ),
        prints(
            "!IoErr(code)",
            "match io(0):\n    ?v:\n        print(v)\n    !IoErr(code):\n        print(code)",
            "7",
        ),
        // The default arm: a bare name binds the whole value, `_` binds nothing.
        prints(
            "bare name default",
            "match o(0):\n    ?v:\n        print(v)\n    other:\n        print(other == None)",
            "true",
        ),
        prints(
            "wildcard default",
            "match r(0):\n    ?v:\n        print(v)\n    _:\n        print(\"other\")",
            "other",
        ),
        // Literal payload, or-pattern, guard, and a closure capturing the binding.
        prints(
            "literal payload and or",
            "for n in [1, 2, 9]:\n    match o(n):\n        ?1 | ?2:\n            print(\"small\")\n        ?v if v > 5:\n            print(\"big\")\n        _:\n            print(\"other\")",
            "small\nsmall\nbig",
        ),
        prints(
            "captured binding",
            "match r(4):\n    ?v:\n        f := fn() -> int: v + 1\n        print(f())\n    !e:\n        print(e)",
            "5",
        ),
        // Exhaustiveness.
        rejects(
            "T? only ?v",
            "match o(1):\n    ?v:\n        print(v)",
            "non-exhaustive",
        ),
        rejects(
            "T!E only ?v",
            "match r(1):\n    ?v:\n        print(v)",
            "non-exhaustive",
        ),
        rejects(
            "T!E only !e",
            "match r(1):\n    !e:\n        print(e)",
            "non-exhaustive",
        ),
        // The tag must fit the scrutinee.
        rejects(
            "!e on T?",
            "match o(1):\n    ?v:\n        print(v)\n    !e:\n        print(e)\n    None:\n        print(0)",
            "`!e` matches an error, and int? has none; write `None`",
        ),
        rejects(
            "?v on int",
            "match 5:\n    ?v:\n        print(v)\n    _:\n        print(0)",
            "`?v` matches a present `T?` or a successful `T!E`, found int",
        ),
        rejects(
            "nested ?v on int payload",
            "match o(1):\n    ?(?v):\n        print(v)\n    _:\n        print(0)",
            "`?v` matches a present `T?` or a successful `T!E`, found int",
        ),
        // Binding rules inside a carrier pattern.
        rejects(
            "?(a, a)",
            "match t(1):\n    ?(a, a):\n        print(a)\n    None:\n        print(0)",
            "identifier 'a' is bound more than once in this pattern",
        ),
        rejects(
            "duplicate ?v",
            "match o(1):\n    ?v:\n        print(v)\n    ?w:\n        print(w)\n    None:\n        print(0)",
            "duplicate match arm '?_'",
        ),
        rejects(
            "duplicate !e",
            "match r(1):\n    ?v:\n        print(v)\n    !e:\n        print(e)\n    !f:\n        print(f)",
            "duplicate match arm '!_'",
        ),
        rejects(
            "constant inside ?",
            "match o(1):\n    ?LIMIT:\n        print(1)\n    None:\n        print(0)",
            "`LIMIT` is a constant",
        ),
        rejects(
            "bare unimported variant inside ?",
            "match o(1):\n    ?Red:\n        print(1)\n    _:\n        print(0)",
            "Red",
        ),
    ];
    run_grid("carrier-pattern", &cells);
}

/// A `?v` arm over an un-inferable scrutinee is a structural test on an unknown shape.
#[test]
fn carrier_pattern_on_uninferable_scrutinee_is_rejected() {
    let src = "fn f(x):\n    match x:\n        ?v:\n            print(v)\n        _:\n            print(0)\n";
    let cells = vec![Cell {
        name: "un-inferable".to_string(),
        files: vec![("main.chz".to_string(), src.to_string())],
        expect: Expect::Rejects(
            "cannot match a variant pattern on a value of un-inferable type; annotate it",
        ),
    }];
    run_grid("carrier-pattern-unknown", &cells);
}

/// A carrier or variant pattern on an `int` scrutinee is a checker error on `chezzi run` too: the
/// program never reaches the compiler, so no `internal:` text can appear.
#[test]
fn variant_pattern_on_int_is_a_type_error_on_run() {
    let root = std::env::temp_dir().join(format!("chezzi-pattern-int-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let progs = [
        (
            "fn main():\n    match 5:\n        ?x: print(x)\nmain()\n",
            "`?v` matches a present `T?` or a successful `T!E`, found int",
        ),
        (
            "enum E:\n    A(int)\n    B\nfn main():\n    match 5:\n        E.A(x): print(x)\n        _: print(0)\nmain()\n",
            "cannot match a variant against int",
        ),
    ];
    for (i, (src, want)) in progs.iter().enumerate() {
        let file = root.join(format!("p{i}.chz"));
        std::fs::write(&file, src).unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&file)
            .output()
            .expect("spawn chezzi");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.status.success(), "must exit non-zero: {text}");
        assert!(text.contains(want), "want {want:?} in: {text}");
        assert!(!text.contains("internal:"), "no internal error: {text}");
    }
    let _ = std::fs::remove_dir_all(&root);
}
