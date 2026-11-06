//! Checker tests: the expected-type table (`child_hint`, `checker/infer.rs`) over every wrapper a
//! return-only generic call can sit under (none, `?`, `??`, `else:`, `recover:`) in every hinted
//! position, plus the leak cells: a child the table gives no hint sees none.

use super::*;

/// `parse` returns `T!`, `opt` returns `T?`; only the expected type can pin `T`.
const PRELUDE: &str = "import TryFrom from std.convert
fn parse[T: TryFrom[str]](s: str) -> T!:
    return T.try_from(s)
fn opt[T: TryFrom[str]](s: str) -> T?:
    v := T.try_from(s) else:
        return None
    return ?v
fn takes_int(x: int) -> int:
    return x
fn takes_int_r(x: int!) -> int:
    return x ?? 0
fn takes_int_o(x: int?) -> int:
    return x ?? 0
";

/// One cell: the statement(s) for `pos` with the value `e` in a slot of type `slot`.
fn cell_body(pos: &str, e: &str, slot: &str) -> String {
    let takes = match slot {
        "int!" => "takes_int_r",
        "int?" => "takes_int_o",
        _ => "takes_int",
    };
    match pos {
        "let" => format!("    n: {slot} = {e}\n    print(n)\n"),
        "return" => format!("    return {e}\n"),
        "arg" => format!("    print({takes}({e}))\n"),
        "list" => format!("    xs: List[{slot}] = [{e}]\n    print(xs.len())\n"),
        "map" => format!("    m: Map[str, {slot}] = {{\"a\": {e}}}\n    print(m.len())\n"),
        "if" => format!("    n: {slot} = if c: {e} else: 0\n    print(n)\n"),
        "match" => {
            format!("    n: {slot} = match k:\n        0: {e}\n        _: 0\n    print(n)\n")
        }
        "tuple" => format!("    p: ({slot}, int) = ({e}, 1)\n    print(p.1)\n"),
        other => unreachable!("no position {other}"),
    }
}

#[test]
fn a_return_only_generic_call_is_pinned_under_every_wrapper_in_every_position() {
    // (wrapper, value, slot type, enclosing fn's return type). rustc infers each of these
    // (`let n: i64 = parse("7")?`, `opt("7").unwrap_or(0)`, `if c { parse("7")? } else { 0 }`,
    // `vec![parse("7")?]`: `7 7 7 [7]`).
    let wrappers: &[(&str, &str, &str, &str)] = &[
        ("none", "parse(\"7\")", "int!", "int!"),
        ("none", "opt(\"7\")", "int?", "int?"),
        ("?", "parse(\"7\")?", "int", "int!"),
        ("?", "opt(\"7\")?", "int", "int?"),
        ("??", "parse(\"7\") ?? 0", "int", "int"),
        ("??", "opt(\"7\") ?? 0", "int", "int"),
        ("recover", "(recover: parse(\"7\")?)", "int!", "int!"),
        ("else", "parse(\"7\") else:\n        return 0", "int", "int"),
        ("else", "opt(\"7\") else:\n        return 0", "int", "int"),
    ];
    let positions = [
        "let", "return", "arg", "list", "map", "if", "match", "tuple",
    ];
    let mut failed = Vec::new();
    let mut cells = 0;
    for &(w, e, slot, ret) in wrappers {
        for pos in positions {
            // An `else` guard stands only on a binding (a parse error elsewhere); an inline
            // `recover:` is not a `return` value (the block form is).
            if (w == "else" && pos != "let") || (w == "recover" && pos == "return") {
                continue;
            }
            // A `return` position's slot is the fn's return type, except under `?`, whose
            // value is the payload of the carrier the fn returns.
            let fn_ret = if pos == "return" && w != "?" {
                slot
            } else {
                ret
            };
            let tail = if pos == "return" {
                ""
            } else {
                "    return 0\n"
            };
            let src = format!(
                "{PRELUDE}fn cell(c: bool, k: int) -> {fn_ret}:\n{}{tail}print(cell(true, 0))\n",
                cell_body(pos, e, slot)
            );
            cells += 1;
            let errs = check_src(&src);
            if !errs.is_empty() {
                failed.push(format!("[{w} / {pos}] {:?}", errs[0].message));
            }
        }
    }
    assert_eq!(cells, 57, "the grid lost a cell");
    assert!(
        failed.is_empty(),
        "{} of {cells} cells rejected:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// An `if` arm forwards its node's hint whole (`ChildHint::Same`): under `?`, `??` and
/// `else:` that is the payload hint, so a return-only generic call in an arm is pinned as it is
/// outside one (rustc: `let n: i64 = (if c { parse("7") } else { parse("8") })?`).
#[test]
fn a_branch_arm_keeps_the_payload_hint() {
    let cell = |ret: &str, body: &str| {
        format!("{PRELUDE}fn cell(c: bool, k: int) -> {ret}:\n{body}print(cell(true, 0))\n")
    };
    let arms = [
        ("int!", "parse(\"7\")", "parse(\"8\")"),
        ("int?", "opt(\"7\")", "opt(\"8\")"),
    ];
    let mut failed = Vec::new();
    for (ret, a, b) in arms {
        // A multi-line `match` is a whole value (no `(match ...)?`), so the arms are `if`'s.
        let iff = format!("(if c: {a} else: {b})");
        let elif = format!("(if c: {a} elif k == 1: {b} else: {a})");
        for v in [&iff, &elif] {
            for src in [
                cell(ret, &format!("    n: int = {v}?\n    return n\n")),
                cell("int", &format!("    n: int = {v} ?? 0\n    return n\n")),
                cell(
                    "int",
                    &format!("    n: int = {v} else:\n        return 0\n    return n\n"),
                ),
            ] {
                let errs = check_src(&src);
                if !errs.is_empty() {
                    failed.push(format!("{src}\n=> {:?}", errs[0].message));
                }
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

const LEAK_PRELUDE: &str = "struct Box[T]:
    v: T
fn mk[T]() -> Box[T]:
    return mk[T]()
fn mko[T]() -> Box[T]?:
    return None
fn pick[T]() -> T:
    return pick[T]()
fn pair[T]() -> (T, int):
    return pair[T]()
fn g() -> int:
    return 7
";

fn leak(body: &str) -> String {
    format!("{LEAK_PRELUDE}fn main():\n{body}main()\n")
}

#[test]
fn a_child_the_table_gives_no_hint_sees_none() {
    // Unchanged by the table: a call's receiver, unary and binary operands, a non-generic
    // fragment.
    ok(&leak(
        "    s := \"abc\"\n    n: int = s.len()\n    print(n)\n",
    ));
    ok(&leak("    n: int = -pick()\n    print(n)\n"));
    ok(&leak("    b: bool = not pick()\n    print(b)\n"));
    ok(&leak("    n: int = pick() + 1\n    print(n)\n"));
    ok(&leak("    s: str = \"{g()}\"\n    print(s)\n"));
    // A field / `?.` receiver and a subscript never saw a usable hint (`int` is not `Box[T]`).
    rejects(
        &leak("    n: int = mk().v\n    print(n)\n"),
        "cannot infer type parameter T for 'mk'",
    );
    rejects(
        &leak("    n: int = pair().0\n    print(n)\n"),
        "cannot infer type parameter T for 'pair'",
    );
    rejects(
        &leak("    n: int? = mko()?.v\n    print(n)\n"),
        "cannot infer type parameter T for 'mko'",
    );
    rejects(
        &leak("    xs: List[int] = [1]\n    n: int = xs[pick()]\n    print(n)\n"),
        "cannot infer type parameter T for 'pick'",
    );
    // Closed leaks: a fragment and a range bound used to take the slot's type as their own
    // (`s: str = "{pick()}"` checked with `T := str`, `pick()..5` with `T := Range`). rustc
    // rejects `let s: String = format!("{}", pick())` (E0282) too.
    rejects(
        &leak("    s: str = \"{pick()}\"\n    print(s)\n"),
        "cannot infer type parameter T for 'pick'",
    );
    rejects(
        &leak("    r: Range = pick()..5\n    print(r)\n"),
        "cannot infer type parameter T for 'pick'",
    );
}

const SLOT_PRELUDE: &str = "import Into, TryInto from std.convert
struct Rat:
    n: int
    d: int
    fn into(self) -> float:
        return 1.0
    fn into(self) -> str:
        return \"s\"
    fn try_into(self) -> int!:
        return self.n
    fn try_into(self) -> str!:
        return \"{self.n}/{self.d}\"
";

#[test]
fn a_payload_hint_picks_a_target_keyed_overload() {
    // The `Ret` key of `try_into(self) -> int!` is its payload `int`, so the payload hint under
    // `?` / `else:` names it (rustc: `let n: i64 = r.try_into()?`).
    let cell = |body: &str| format!("{SLOT_PRELUDE}fn f(r: Rat) -> int!:\n{body}    return 0\n");
    ok(&cell("    n: int = r.try_into()?\n    print(n)\n"));
    ok(&cell("    t: str = r.try_into()?\n    print(t)\n"));
    ok(&cell(
        "    n: int = r.try_into() else:\n        return 0\n    print(n)\n",
    ));
    ok(&cell(
        "    xs: List[str] = [r.try_into()?]\n    print(xs)\n",
    ));
    // Unchanged: a plain annotation picks `into`; a payload hint on a plain `into` picks the
    // overload and `?` reports the non-carrier.
    ok(&cell("    t: str = r.into()\n    print(t)\n"));
    rejects(
        &cell("    t: str = r.into()?\n    print(t)\n"),
        "'?' expects a `T?` or `T!E` value, found str",
    );
}
