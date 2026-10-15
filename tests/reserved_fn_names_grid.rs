//! TICKET-253, then the owner rule of 2026-10-15: only keywords are reserved. Every builtin name —
//! a builtin fn (`print`, `ord`), a constructor (`List`, `Channel`, `timer`), a builtin or removed
//! type name (`int`, `Result`, `ptr`) and the prelude's `Range` — may name a top-level `fn` and an
//! `extern fn`; the declaration shadows the builtin in its module (`tests/shadowing_grid.rs`). The
//! position test below keeps the TICKET-253 rule that the import the resolver adds to every module
//! (it has no source line) never prints a line the file does not have.

use std::path::PathBuf;
use std::process::Command;

/// Builtin names: each may name a plain `fn` and an `extern fn`.
const BUILTIN_NAMES: &[&str] = &[
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
    "Range",
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
fn a_builtin_name_may_name_a_fn_or_an_extern_fn() {
    for name in BUILTIN_NAMES {
        for (kind, src) in [("fn", plain_fn(name)), ("extern fn", extern_fn(name))] {
            let (ok, text) = check(&format!("acc-{name}"), &src);
            assert!(
                ok,
                "`{kind} {name}` must be accepted (only keywords are reserved): {text}"
            );
        }
    }
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
