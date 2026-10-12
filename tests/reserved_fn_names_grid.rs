//! TICKET-253 — which top-level `fn` and `extern fn` names the checker rejects.
//!
//! The expected column is the verdict of the `main` binary (commit 902b678c, measured with
//! `chezzi check` on one file per cell), so a change to the guard that rejects a name `main`
//! accepts fails here. The one new row is `Range`: the struct of `std.prelude` that every module
//! binds through an import no one wrote. That import has no source position, so no message may
//! print one.

use std::path::PathBuf;
use std::process::Command;

/// Names `main` rejects for both a plain `fn` and an `extern fn`.
const REJECTED_BOTH: &[&str] = &[
    "int",
    "float",
    "bool",
    "str",
    "bytes",
    "bytearray",
    "List",
    "Set",
    "Map",
    "Channel",
    "range",
    "Shared",
    "RwShared",
    "Atomic",
    "AtomicInt",
    "Executor",
    "timer",
    "ord",
    "chr",
    "print",
    "panic",
];

/// Reserved TYPE names that are no callable: `main` accepts them for both kinds.
const ACCEPTED_BOTH: &[&str] = &[
    "Result",
    "Option",
    "Iterator",
    "tuple",
    "Socket",
    "Listener",
    "Writer",
    "Reader",
    "ptr",
    "owned_str",
];

fn check(tag: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-reserved-fn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("main.chz");
    std::fs::write(&file, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", "main.chz"])
        .current_dir(&dir)
        .output()
        .expect("spawn chezzi");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.success(), text)
}

fn plain_fn(name: &str) -> String {
    format!("fn {name}(x: float) -> float:\n    return x\n")
}

fn extern_fn(name: &str) -> String {
    format!("extern \"libm.so.6\":\n    fn {name}(x: float) -> float\n")
}

#[test]
fn a_fn_name_main_accepts_stays_accepted() {
    for name in ACCEPTED_BOTH {
        for (kind, src) in [("fn", plain_fn(name)), ("extern fn", extern_fn(name))] {
            let (ok, text) = check(&format!("acc-{name}"), &src);
            assert!(ok, "`{kind} {name}` was accepted on main; now: {text}");
        }
    }
}

#[test]
fn a_fn_name_main_rejects_stays_rejected() {
    for name in REJECTED_BOTH {
        let (ok, text) = check(&format!("rej-fn-{name}"), &plain_fn(name));
        assert!(
            !ok && text.contains(&format!("function name '{name}' is reserved (builtin)")),
            "`fn {name}`: {text}"
        );
        let (ok, text) = check(&format!("rej-ext-{name}"), &extern_fn(name));
        assert!(
            !ok && text.contains(&format!(
                "'{name}' is a builtin/reserved name and cannot be an extern fn"
            )),
            "`extern fn {name}`: {text}"
        );
    }
}

#[test]
fn range_is_the_one_new_row_and_reports_once() {
    let (ok, text) = check("range-fn", &plain_fn("Range"));
    assert!(!ok);
    assert!(
        text.contains("function name 'Range' is reserved (builtin)"),
        "{text}"
    );
    assert!(
        text.contains("chezzi: 1 type error"),
        "one error only: {text}"
    );
    // The same fn, called: the call must not add a second error.
    let (ok, text) = check(
        "range-fn-call",
        "fn Range(a: int) -> int:\n    return a\nprint(Range(5))\n",
    );
    assert!(!ok);
    assert!(
        text.contains("chezzi: 1 type error"),
        "one error only: {text}"
    );
    let (ok, text) = check("range-ext", &extern_fn("Range"));
    assert!(!ok);
    assert!(
        text.contains("'Range' is a builtin/reserved name and cannot be an extern fn"),
        "{text}"
    );
}

/// A message may print a line the file has. The import the resolver adds to every module has no
/// line, so a program that touches the name `Range` in any declaration position must never print
/// the sentinel it carries, and every position it prints must lie inside the file.
#[test]
fn no_message_prints_a_line_the_file_does_not_have() {
    let programs: &[(&str, &str)] = &[
        (
            "fn",
            "fn Range(a: int) -> int:\n    return a\nprint(Range(5))\n",
        ),
        ("struct", "struct Range:\n    a: int\n"),
        ("enum", "enum Range:\n    A\n    B\n"),
        ("alias", "type Range = int\n"),
        ("protocol", "protocol Range:\n    fn f(self) -> int\n"),
        ("let", "Range := 3\nprint(Range)\n"),
        (
            "param",
            "fn f(Range: int) -> int:\n    return Range\nprint(f(2))\n",
        ),
        (
            "nested-fn",
            "fn outer() -> int:\n    fn Range(a: int) -> int:\n        return a\n    return Range(5)\nprint(outer())\n",
        ),
        (
            "explicit-import",
            "import Range from std.prelude\nprint(Range(0, 3))\n",
        ),
        (
            "explicit-import-after",
            "print(Range(0, 3))\nimport Range from std.prelude\n",
        ),
        (
            "import-alias",
            "import Range as Range from std.prelude\nprint(Range(0, 3))\n",
        ),
        (
            "alias-to-range",
            "import Range as R from std.prelude\nprint(R(0, 3))\n",
        ),
        ("import-as-range", "import abs as Range from std.math\n"),
        (
            "fn-then-import",
            "fn Range(a: int) -> int:\n    return a\nimport Range from std.prelude\n",
        ),
        (
            "import-then-fn",
            "import Range from std.prelude\nfn Range(a: int) -> int:\n    return a\n",
        ),
    ];
    for (tag, src) in programs {
        let (_, text) = check(&format!("span-{tag}"), src);
        assert!(
            !text.contains("4294967295"),
            "`{tag}` printed the sentinel line: {text}"
        );
        let lines = src.lines().count();
        for part in text.split("main.chz:").skip(1) {
            let line: usize = part
                .split(':')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            assert!(
                line >= 1 && line <= lines,
                "`{tag}` printed line {line} of a {lines}-line file: {text}"
            );
        }
    }
}
