//! A call on a scalar type's name sets no field, so it gives the type's zero value (`int()` is `0`,
//! as Python's `int()` and Rust's `i64::default()`), and an argument is a type error. A conversion is
//! a method on the source value: `n.to_float()`, `f.to_int()` (`None` for NaN and the infinities),
//! `s.to_int()`, `s.to_float()` (an error that names the cause). Display is interpolation: `"{x}"`
//! (Go `fmt.Sprint`). The value cells of the `str!` / `int!` / `float!` results are in
//! `tests/chz/spec/conversion_error_test.chz`; this grid holds their type cells.
//!
//! Axes: name (int, float, str, bool, bytes) × arity (0, 1) × argument type (int, float, bool, str,
//! List, struct, `T?`, `Any`) for the reject cells, each with the help that names the new form; the
//! new methods on their edge values; `"{x}"` on each kind of value, whose expected text is the old
//! `str(x)` output on the base build (the twin oracle).

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, cmd: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-sconv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args([cmd, "main.chz"])
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

/// One value of each kind the reject cells and the display cells read.
const PRELUDE: &str = "\
struct P:
    x: int
struct Q:
    x: int
    fn str(self) -> str:
        return \"Q!\"
enum Color:
    Red
    Green(int)
i := 5
neg := -1
f := 1.5
nz := -0.0
big := 1e300
b := true
s := \"5\"
xs := [1, 2]
ss := [\"a\"]
m := {\"k\": [1]}
p := P(1)
q := Q(1)
o: int? = ?3
on: int? = None
t := (1, \"a\")
by := b\"hi\"
a: Any = 5
z := 0.0
nan := z / z
inf := 1.0 / z
";

/// (expression, expected display), each printed in its own statement.
const ACCEPT: &[(&str, &str)] = &[
    // Zero values: Python `int()` 0, `float()` 0.0, `str()` '', `bool()` False, `bytes()` b''.
    ("int()", "0"),
    ("float()", "0.0"),
    ("str()", ""),
    ("bool()", "false"),
    ("bytes()", "b''"),
    ("int() == 0", "true"),
    ("str().len()", "0"),
    ("bytes().len()", "0"),
    // int -> float is total; 2^53 + 1 rounds to the nearest float, as Rust `n as f64`.
    ("(0).to_float()", "0.0"),
    ("neg.to_float()", "-1.0"),
    ("(-3).to_float()", "-3.0"),
    ("(9007199254740993).to_float()", "9007199254740992.0"),
    // float -> int truncates toward zero; NaN, the infinities and a value outside i64 are None.
    ("(3.9).to_int()", "3"),
    ("(-3.9).to_int()", "-3"),
    ("(0.0).to_int()", "0"),
    ("nz.to_int()", "0"),
    ("big.to_int()", "None"),
    ("nan.to_int()", "None"),
    ("inf.to_int()", "None"),
    ("(-inf).to_int()", "None"),
    ("(9223372036854775807.0).to_int()", "None"),
    ("(-9223372036854775808.0).to_int()", "-9223372036854775808"),
    ("(3.9).to_int() == ?3", "true"),
    // The str parsers give an error that names the cause.
    ("\"42\".to_int()", "42"),
    ("\"x\".to_int()", "!to_int: cannot parse 'x' as int"),
    ("\"1.5\".to_float()", "1.5"),
    // `\"{x}\"` on each kind: the text the base build's `str(x)` printed.
    ("\"{i}\"", "5"),
    ("\"{neg}\"", "-1"),
    ("\"{f}\"", "1.5"),
    ("\"{nz}\"", "-0.0"),
    ("\"{big}\"", "1e+300"),
    ("\"{b}\"", "true"),
    ("\"{s}\"", "5"),
    ("\"{xs}\"", "[1, 2]"),
    ("\"{ss}\"", "['a']"),
    ("\"{m}\"", "{'k': [1]}"),
    ("\"{p}\"", "P(x=1)"),
    ("\"{q}\"", "Q!"),
    ("\"{o}\"", "3"),
    ("\"{on}\"", "None"),
    ("\"{t}\"", "(1, 'a')"),
    ("\"{by}\"", "b'hi'"),
    ("\"{a}\"", "5"),
    ("\"{Color.Red}\"", "Red"),
    ("\"{Color.Green(2)}\"", "Green(2)"),
    ("\"{m[\"k\"]}\"", "[1]"),
];

/// (statement block, expected stdout).
const ACCEPT_STMTS: &[(&str, &str)] = &[
    // The result types: `int.to_float` is total, `float.to_int` may miss.
    (
        "x: float = (2).to_float()\ny: int? = (2.5).to_int()\nprint(x, y)\n",
        "2.0 2\n",
    ),
    // `str.to_int`, `str.to_float` and `decode` give `T!`; `??` unwraps them to `T`.
    (
        "n: int = s.to_int() ?? 0\nx: float = s.to_float() ?? 0.0\nw: str = by.decode() ?? \"\"\nprint(n, x, w)\n",
        "5 5.0 hi\n",
    ),
    // The else guard handles the miss.
    (
        "fn g(v: float) -> int:\n    n := v.to_int() else:\n        return -1\n    return n\nprint(g(nan), g(2.5))\n",
        "-1 2\n",
    ),
];

/// (program, expected stdout): a module declaration of the name owns the call (only keywords are
/// reserved). These run without the prelude, whose `Q.str` returns the builtin `str`.
const SHADOW: &[(&str, &str)] = &[
    (
        "fn float(n: int) -> str:\n    return \"mine\"\nprint(float(3))\n",
        "mine\n",
    ),
    ("struct str:\n    n: int\nprint(str(4).n)\n", "4\n"),
];

#[test]
fn scalar_conversion_accept_cells() {
    let mut src = String::from(PRELUDE);
    let mut want = String::new();
    for (e, out) in ACCEPT {
        src.push_str(&format!("print({e})\n"));
        want.push_str(out);
        want.push('\n');
    }
    let (ok, got) = chezzi("accept", "run", &src);
    assert!(
        ok && got == want,
        "program:\n{src}\nwant:\n{want}\ngot:\n{got}"
    );
    for (i, (body, out)) in ACCEPT_STMTS.iter().enumerate() {
        let src = format!("{PRELUDE}{body}");
        let (ok, got) = chezzi(&format!("stmt{i}"), "run", &src);
        assert!(
            ok && got == *out,
            "program:\n{src}\nwant:\n{out}\ngot:\n{got}"
        );
    }
    for (i, (src, out)) in SHADOW.iter().enumerate() {
        let (ok, got) = chezzi(&format!("shadow{i}"), "run", src);
        assert!(
            ok && got == *out,
            "program:\n{src}\nwant:\n{out}\ngot:\n{got}"
        );
    }
}

/// The argument kinds of the reject cells: (argument, its type).
const ARGS: &[&str] = &["i", "f", "b", "s", "xs", "p", "o", "a"];

/// The help each one-argument call gives, per argument in `ARGS` order.
fn expected_help(name: &str, arg: &str) -> String {
    let ty = match arg {
        "i" => "int",
        "f" => "float",
        "b" => "bool",
        "s" => "str",
        "xs" => "List[int]",
        "p" => "P",
        "o" => "int?",
        _ => "Any",
    };
    let article = if ty.starts_with(['a', 'e', 'i', 'o', 'u']) {
        "an"
    } else {
        "a"
    };
    if ty == name {
        return format!("help: the argument is already {article} {ty}");
    }
    match (name, ty) {
        ("int", "float") => "help: convert with `x.to_int()`, which gives int?".into(),
        ("int", "str") => "help: convert with `x.to_int()`, which gives int!".into(),
        ("float", "int") => "help: convert with `x.to_float()`, which gives float".into(),
        ("float", "str") => "help: convert with `x.to_float()`, which gives float!".into(),
        ("int", "bool") => "help: a bool is not a number; write `if x: 1 else: 0`".into(),
        ("float", "bool") => "help: a bool is not a number; write `if x: 1.0 else: 0.0`".into(),
        ("str", _) => "help: render the value with `\"{x}\"`".into(),
        ("bool", "int" | "float") => "help: test the value: write `x != 0`".into(),
        ("bool", "int?") => "help: test the value: write `x != None`".into(),
        ("bool", "str" | "List[int]") => "help: test the value: write `x.len() > 0`".into(),
        ("bytes", "int") => "help: make n zero bytes with `bytes.zeroed(n)`".into(),
        ("bytes", _) => "help: build a bytes from an iterable with `bytes.from_iter(xs)`".into(),
        _ => format!("help: {ty} does not convert to {name}"),
    }
}

#[test]
fn scalar_conversion_reject_cells() {
    let mut failures = Vec::new();
    for name in ["int", "float", "str", "bool", "bytes"] {
        for arg in ARGS {
            let src = format!("{PRELUDE}print({name}({arg}))\n");
            let (ok, got) = chezzi(&format!("rej-{name}-{arg}"), "check", &src);
            let error = format!("{name}() takes no argument");
            let help = expected_help(name, arg);
            if ok || !got.contains(&error) || !got.contains(&help) {
                failures.push(format!(
                    "{name}({arg}): want `{error}` + `{help}`, got:\n{got}"
                ));
            }
        }
        // Two arguments: the same error.
        let src = format!("{PRELUDE}print({name}(i, i))\n");
        let (ok, got) = chezzi(&format!("rej2-{name}"), "check", &src);
        if ok || !got.contains(&format!("{name}() takes no argument")) {
            failures.push(format!("{name}(i, i): got:\n{got}"));
        }
    }
    // The methods exist only where the decls put them: no identity method, no bool method, and
    // `str` is only the `Stringable` hook.
    for (call, want) in [
        ("i.to_int()", "type int has no method 'to_int'"),
        ("f.to_float()", "type float has no method 'to_float'"),
        ("b.to_int()", "type bool has no method 'to_int'"),
        ("i.str()", "type int has no method 'str'"),
        // `to_int` / `to_float` give the reason, so the `parse_*` duplicates are gone.
        ("s.parse_int()", "type str has no method 'parse_int'"),
        ("s.parse_float()", "type str has no method 'parse_float'"),
    ] {
        let src = format!("{PRELUDE}print({call})\n");
        let (ok, got) = chezzi("rej-method", "check", &src);
        if ok || !got.contains(want) {
            failures.push(format!("{call}: want `{want}`, got:\n{got}"));
        }
    }
    // A `T!` result is not its `T`: the caller unwraps it.
    for (stmt, want) in [
        (
            "n: int = s.to_int()",
            "cannot assign int! to variable of type int",
        ),
        (
            "x: float = s.to_float()",
            "cannot assign float! to variable of type float",
        ),
        (
            "t: str = by.decode()",
            "cannot assign str! to variable of type str",
        ),
        (
            "t: str = bytearray.from_iter(by).decode()",
            "cannot assign str! to variable of type str",
        ),
    ] {
        let src = format!("{PRELUDE}{stmt}\n");
        let (ok, got) = chezzi("rej-carrier", "check", &src);
        if ok || !got.contains(want) {
            failures.push(format!("{stmt}: want `{want}`, got:\n{got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
