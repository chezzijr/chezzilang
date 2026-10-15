//! Source layout rules (`docs/owners.md`, `docs/code-conventions.md`). A file small enough to read
//! whole, a header that names its topic, and an owner index that names only real files:
//!
//! 1. A production file under `src/` has at most `PROD_MAX_LINES` lines; a test file (in a `tests/`
//!    directory, or named `*tests.rs`) at most `TEST_MAX_LINES`. No file is exempt: split a file
//!    by topic before it reaches the cap.
//! 2. The first line of every `src/` file that is not an inner attribute (`#![...]`) is a comment
//!    that names the file's topic.
//! 3. Every `src/`-relative `.rs` path that `docs/owners.md` names exists.

use std::fs;
use std::path::{Path, PathBuf};

const PROD_MAX_LINES: usize = 2_500;
const TEST_MAX_LINES: usize = 3_500;

/// Every `.rs` file under `dir`, at any depth, in path order.
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

fn is_test_file(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "tests")
        || path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with("tests.rs"))
}

#[test]
fn every_source_file_is_under_its_line_cap() {
    let mut over = Vec::new();
    for path in rust_files(Path::new("src")) {
        let lines = fs::read_to_string(&path).unwrap().lines().count();
        let cap = if is_test_file(&path) {
            TEST_MAX_LINES
        } else {
            PROD_MAX_LINES
        };
        if lines > cap {
            over.push(format!("{} has {lines} lines (cap {cap})", path.display()));
        }
    }
    assert!(
        over.is_empty(),
        "split these files by topic:\n{}",
        over.join("\n")
    );
}

#[test]
fn every_source_file_opens_with_a_topic_comment() {
    let mut bare = Vec::new();
    for path in rust_files(Path::new("src")) {
        let text = fs::read_to_string(&path).unwrap();
        let first = text.lines().find(|l| !l.starts_with("#!["));
        if !first.is_some_and(|l| l.starts_with("//")) {
            bare.push(path.display().to_string());
        }
    }
    assert!(
        bare.is_empty(),
        "give these files a topic comment on their first line:\n{}",
        bare.join("\n")
    );
}

/// A `src/`-relative path as the owner index spells it: `x.rs`, `dir/x.rs` or `dir/`.
fn as_src_path(span: &str) -> Option<&str> {
    let plain = span
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_/.".contains(c));
    // A directory names at least two segments (`parser/tests/`): a bare `src/` is prose.
    let shaped =
        span.ends_with(".rs") || (span.ends_with('/') && span.trim_end_matches('/').contains('/'));
    (plain && shaped).then_some(span)
}

/// Does `text` define an item named `name` (fn, type, const, static, trait, macro)?
fn defines(text: &str, name: &str) -> bool {
    [
        "fn ",
        "struct ",
        "enum ",
        "const ",
        "static ",
        "type ",
        "trait ",
        "macro_rules! ",
    ]
    .iter()
    .any(|kw| {
        text.match_indices(&format!("{kw}{name}")).any(|(at, m)| {
            let next = text[at + m.len()..].chars().next();
            !next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    })
}

#[test]
fn every_path_in_the_owner_index_exists() {
    let index = fs::read_to_string("docs/owners.md").expect("read docs/owners.md");
    let spans: Vec<&str> = index.split('`').skip(1).step_by(2).collect();
    let mut named = 0;
    let mut missing = Vec::new();
    for span in &spans {
        let Some(rel) = as_src_path(span) else {
            continue;
        };
        named += 1;
        // `tests/...` names an integration test at the repo root; every other path is under `src/`.
        let path = if rel.starts_with("tests/") {
            PathBuf::from(rel)
        } else {
            Path::new("src").join(rel)
        };
        let exists = if rel.ends_with('/') {
            path.is_dir()
        } else {
            path.is_file()
        };
        if !exists {
            missing.push(rel.to_string());
        }
    }
    assert!(named > 0, "docs/owners.md names no src file");
    assert!(
        missing.is_empty(),
        "docs/owners.md names paths that do not exist under src/:\n{}",
        missing.join("\n")
    );
}

/// Every `` `name` (`path`) `` pair in the owner index: the file at `path` defines `name`. A move
/// that carries an owner to another file must update its row.
#[test]
fn every_owner_in_the_index_lives_in_its_named_file() {
    let index = fs::read_to_string("docs/owners.md").expect("read docs/owners.md");
    let mut pairs = 0;
    let mut wrong = Vec::new();
    for (at, _) in index.match_indices("` (`") {
        let name_start = index[..at].rfind('`').expect("an opening backtick") + 1;
        let name = &index[name_start..at];
        let rest = &index[at + 4..];
        let Some(path_end) = rest.find("`)") else {
            continue;
        };
        let Some(rel) = as_src_path(&rest[..path_end]) else {
            continue;
        };
        let is_ident =
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !is_ident || rel.ends_with('/') {
            continue;
        }
        pairs += 1;
        let text = fs::read_to_string(Path::new("src").join(rel)).unwrap_or_default();
        if !defines(&text, name) {
            wrong.push(format!("`{name}` is not defined in src/{rel}"));
        }
    }
    assert!(pairs > 0, "docs/owners.md has no `name` (`path`) pair");
    assert!(
        wrong.is_empty(),
        "fix these rows of docs/owners.md:\n{}",
        wrong.join("\n")
    );
}
