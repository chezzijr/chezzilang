//! TICKET-237 — only a keyword cannot be bound, and `None` is a keyword like `true` and `false`.
//! Every binder and declaration form rejects a keyword with ONE parse error; a local, a parameter
//! or a loop variable may still shadow a builtin function or type name (`print := 5`), and RUNS.
//! The grid is binder form x name, each cell pinned to what `chezzi run` prints.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn chezzi(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn chezzi")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A fresh directory holding `main.chz`.
fn scratch(tag: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chezzi-kwbinder-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    dir
}

fn run(tag: &str, src: &str) -> Output {
    let dir = scratch(tag, src);
    let out = chezzi(&["run", "main.chz"], &dir);
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// What one cell must do. `NAME` in a `Fails` text stands for the cell's name.
#[derive(Clone, Copy)]
enum Cell {
    /// Exit 0 and stdout is exactly this text plus a newline.
    Runs(&'static str),
    /// Non-zero exit and stderr contains this text.
    Fails(&'static str),
    /// `Fails` with the one keyword-as-a-name message, naming the cell's keyword.
    Keyword,
}
use Cell::{Fails, Keyword, Runs};

const NAMES: [&str; 11] = [
    "None", "true", "false", "if", "self", "print", "int", "len", "List", "nil", "Some",
];

/// The one message for a keyword in a name position, after the location prefix.
fn keyword_text(name: &str) -> String {
    format!(
        "expected identifier, found reserved keyword '{name}' (a keyword cannot be used as a name)"
    )
}

/// (form, template, the cell per name in `NAMES` order). `NAME` in a template is the cell's name.
#[rustfmt::skip]
const GRID: [(&str, &str, [Cell; 11]); 26] = [
    ("T01 walrus", "NAME := 5\nprint(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ':=' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
    ("T02 typed binding", "NAME: int = 4\nprint(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ':' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T03 const binding", "NAME: const int = 4\nprint(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ':' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T04 for destructuring", "for a, NAME in [(1, 2)]:\n    pass\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T05 param", "fn f(NAME: int) -> int:\n    return 1\nprint(f(2))\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T06 lambda param", "g := fn(NAME: int) -> int: 1\nprint(g(2))\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T07 for var", "for NAME in [1, 2]:\n    pass\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T08 comprehension var", "print([1 for NAME in [1, 2]])\n", [Keyword, Keyword, Keyword, Keyword, Runs("[1, 1]"), Runs("[1, 1]"), Runs("[1, 1]"), Runs("[1, 1]"), Runs("[1, 1]"), Runs("[1, 1]"), Runs("[1, 1]")]),
    ("T09 tuple pattern", "x: (int, int) = (3, 4)\nmatch x:\n    (NAME, b): print(1)\n", [Fails("variant pattern 'None' cannot match a value of type int"), Fails("literal of type bool cannot match a value of type int"), Fails("literal of type bool cannot match a value of type int"), Keyword, Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
    ("T10 else binder", "fn r() -> int!str:\n    return 1\nfn m() -> int:\n    v := r() else NAME:\n        return 0\n    return v\nprint(m())\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T11 wait arm", "ch := Channel[int](1)\nch.send(1)\nwait:\n    NAME := ch.recv():\n        print(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ':=' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
    ("T12 fn name", "fn NAME(x: int) -> int:\n    return x\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Fails("function name 'NAME' is reserved (builtin)"), Fails("function name 'NAME' is reserved (builtin)"), Runs("1"), Fails("function name 'NAME' is reserved (builtin)"), Runs("1"), Runs("1")]),
    ("T13 nested fn name", "fn outer() -> int:\n    fn NAME(x: int) -> int:\n        return x\n    return 1\nprint(outer())\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T14 method name", "struct S:\n    x: int\n    fn NAME(self) -> int:\n        return 1\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T15 struct name", "struct NAME:\n    x: int\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Runs("1")]),
    ("T16 enum name", "enum NAME:\n    A\n    B\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Runs("1")]),
    ("T17 type alias name", "type NAME = int\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Runs("1")]),
    ("T18 protocol name", "protocol NAME:\n    fn go(self) -> int\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Runs("1")]),
    ("T19 generic param", "fn f[NAME](x: NAME) -> int:\n    return 1\nprint(f(2))\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("type 'NAME' is reserved (builtin)"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
    ("T20 field", "struct S:\n    NAME: int\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T21 variant", "enum E:\n    NAME\n    B\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Runs("1")]),
    ("T22 module alias", "import std.math as NAME\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Fails("import alias 'NAME' is reserved (builtin)"), Fails("import alias 'NAME' is reserved (builtin)"), Runs("1"), Fails("import alias 'NAME' is reserved (builtin)"), Runs("1"), Fails("import alias 'NAME' is reserved (builtin)")]),
    ("T23 import alias", "import sqrt as NAME from std.math\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Runs("1"), Fails("import alias 'NAME' is reserved (builtin)"), Fails("import alias 'NAME' is reserved (builtin)"), Runs("1"), Fails("import alias 'NAME' is reserved (builtin)"), Runs("1"), Fails("import alias 'NAME' is reserved (builtin)")]),
    ("T24 extern fn", "extern \"libm\":\n    fn NAME(x: float) -> float\nprint(1)\n", [Keyword, Keyword, Keyword, Keyword, Fails("symbol 'NAME' not found in 'libm'"), Fails("'NAME' is a builtin/reserved name and cannot be an extern fn"), Fails("'NAME' is a builtin/reserved name and cannot be an extern fn"), Fails("symbol 'NAME' not found in 'libm'"), Fails("'NAME' is a builtin/reserved name and cannot be an extern fn"), Fails("symbol 'NAME' not found in 'libm'"), Fails("'NAME' is a builtin/reserved name and cannot be an extern fn")]),
    ("T25 destructuring let, first", "NAME, a := 1, 2\nprint(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ',' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
    ("T26 destructuring let, second", "a, NAME := 1, 2\nprint(1)\n", [Keyword, Keyword, Keyword, Fails("unexpected ':=' in expression"), Runs("1"), Fails("int is not callable"), Runs("1"), Runs("1"), Runs("1"), Runs("1"), Fails("`Some(x)` is removed; write `x` or `?x` (pattern `?v`)")]),
];

/// The message of the first diagnostic line of `stderr`, without its `kind (file:line:col): `.
fn first_message(stderr: &str) -> String {
    stderr
        .lines()
        .next()
        .and_then(|l| l.split_once("): "))
        .map(|(_, m)| m.to_string())
        .unwrap_or_default()
}

#[test]
fn binder_form_by_name_grid() {
    let mut wrong = Vec::new();
    for (form, template, cells) in GRID {
        for (name, cell) in NAMES.iter().zip(cells) {
            let out = run("grid", &template.replace("NAME", name));
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            let (good, want) = match cell {
                Runs(t) => (
                    out.status.success() && stdout == format!("{t}\n"),
                    format!("runs and prints {t:?}"),
                ),
                Fails(t) => {
                    let t = t.replace("NAME", name);
                    (
                        !out.status.success() && stderr.contains(&t),
                        format!("fails with {t:?}"),
                    )
                }
                Keyword => {
                    let t = keyword_text(name);
                    (
                        !out.status.success() && stderr.contains(&t),
                        format!("fails with {t:?}"),
                    )
                }
            };
            if !good {
                wrong.push(format!(
                    "{form} x {name}: want {want}, got:\n{}",
                    text(&out)
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "{} cell(s) moved:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The three `:=` sites parse their target as an expression first; each must still report the
/// message every token-reading binder site reports, not a text of its own.
#[test]
fn every_walrus_site_gives_the_one_keyword_message() {
    for src in [
        "NAME := 5\nprint(1)\n",
        "NAME, a := 1, 2\nprint(1)\n",
        "a, NAME := 1, 2\nprint(1)\n",
        "ch := Channel[int](1)\nch.send(1)\nwait:\n    NAME := ch.recv():\n        print(1)\n",
    ] {
        for name in ["None", "true", "false"] {
            let out = run("walrus", &src.replace("NAME", name));
            assert!(!out.status.success(), "{src:?} with {name} must not run");
            let got = first_message(&String::from_utf8_lossy(&out.stderr));
            assert_eq!(got, keyword_text(name), "{src:?} with {name}");
        }
    }
}

/// Owner decision 2026-10-10: only keywords cannot be bound. A local, a parameter or a loop
/// variable named after a builtin function or type shadows it, and the program runs.
#[test]
fn shadowing_a_builtin_runs() {
    for name in ["print", "int", "len", "List", "nil"] {
        for (form, template, want) in [
            (
                "local",
                "fn f() -> int:\n    NAME := 5\n    return NAME + 1\nprint(f())\n",
                "6\n",
            ),
            (
                "param",
                "fn f(NAME: int) -> int:\n    return NAME + 1\nprint(f(2))\n",
                "3\n",
            ),
            (
                "loop var",
                "fn f() -> int:\n    s := 0\n    for NAME in [1, 2]:\n        s += NAME\n    return s\nprint(f())\n",
                "3\n",
            ),
        ] {
            let out = run("shadow", &template.replace("NAME", name));
            assert!(
                out.status.success() && String::from_utf8_lossy(&out.stdout) == want,
                "{name} as a {form} must run and print {want:?}, got:\n{}",
                text(&out)
            );
        }
    }
    // The shadowed builtin is the local now, so a call of it is a call of an int.
    let out = run("shadow", "print := 5\nprint(1)\n");
    assert!(
        !out.status.success() && text(&out).contains("int is not callable"),
        "a call of the shadowed `print` keeps the existing text, got:\n{}",
        text(&out)
    );
}
