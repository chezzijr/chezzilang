//! One owner per fact (`docs/owners.md`, CLAUDE.md "Derive, don't mirror"). Each rule names a
//! spelling that decides a fact and the one file allowed to hold it. The scan reads production code
//! only: comments, string literals and `#[cfg(test)] mod NAME { .. }` blocks are removed first, and
//! test files (a `tests/` directory, `*tests.rs`, `test_support.rs`) are skipped. A second copy of a
//! fact fails here, at build time, instead of in review.

use std::fs;
use std::path::{Path, PathBuf};

/// (spelling, the one `src/`-relative file that may hold it, the fact and its owner)
const RULES: &[(&str, &str, &str)] = &[
    (
        "== \"_\"",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "!= \"_\"",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "\"_\" ==",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "struct TmpDir",
        "test_support.rs",
        "the unit-test fixture directory: use `test_support::TmpDir`",
    ),
];

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn is_test_file(rel: &Path) -> bool {
    let name = rel.file_name().unwrap().to_string_lossy();
    rel.components().any(|c| c.as_os_str() == "tests")
        || name.ends_with("tests.rs")
        || name == "test_support.rs"
}

/// `text` without `//` comments, string-literal contents and inline `#[cfg(test)] mod NAME { .. }`
/// blocks. String delimiters stay (`""`), so a spelling with quotes still matches only code.
fn production_code(text: &str) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut skipping = false;
    while let Some(line) = lines.next() {
        if skipping {
            if line.starts_with('}') {
                skipping = false;
            }
            continue;
        }
        if line == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|n| n.starts_with("mod ") && n.trim_end().ends_with('{'))
        {
            lines.next();
            skipping = true;
            continue;
        }
        out.push_str(&strip_line(line));
        out.push('\n');
    }
    out
}

/// One line without its `//` comment; a string literal keeps its quotes and loses its contents,
/// except a one-character literal, which is the fact spellings this test looks for.
fn strip_line(line: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            break;
        }
        if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.extend(&chars[i..i + 3]);
            i += 3;
            continue;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            let body: String = chars[start + 1..i.min(chars.len())].iter().collect();
            out.push('"');
            if body.chars().count() == 1 {
                out.push_str(&body);
            }
            out.push('"');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[test]
fn each_fact_is_spelled_only_by_its_owner() {
    let src = Path::new("src");
    let mut copies = Vec::new();
    for path in rust_files(src) {
        let rel = path.strip_prefix(src).unwrap();
        if is_test_file(rel) {
            continue;
        }
        let code = production_code(&fs::read_to_string(&path).unwrap());
        for (spelling, owner, fact) in RULES {
            if rel == Path::new(owner) {
                continue;
            }
            for (n, line) in code.lines().enumerate() {
                if line.contains(spelling) {
                    copies.push(format!(
                        "src/{}:{}: `{spelling}` ({fact}; owner {owner})",
                        rel.display(),
                        n + 1
                    ));
                }
            }
        }
    }
    assert!(
        copies.is_empty(),
        "a second copy of a fact with an owner:\n{}",
        copies.join("\n")
    );
}

/// The scan itself: a spelling inside a comment or a longer string never counts; one in code does.
#[test]
fn the_scan_reads_code_only() {
    let code =
        production_code("let a = x == \"_\"; // y == \"_\"\nlet b = \"a == \\\"_\\\" here\";\n");
    assert_eq!(code.matches("== \"_\"").count(), 1, "{code}");
    let gated =
        production_code("#[cfg(test)]\nmod tests {\n    fn f() { x == \"_\" }\n}\nfn g() {}\n");
    assert!(!gated.contains("== \"_\""), "{gated}");
    assert!(gated.contains("fn g()"), "{gated}");
}
