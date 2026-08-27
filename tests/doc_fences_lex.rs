//! Every ```chezzi fence in the docs `chezzi docs` bundles is copy-pasted by an LLM reading that
//! bundle, so it must lex. The file list comes from `src/main.rs`'s `DOC_TOPICS` `include_str!`
//! calls, not a hardcoded list, so a new bundled topic is covered automatically. This lexes only
//! (`chezzi::lexer::tokenize` — the same lexer `chezzi tokens` runs) and never type-checks: most
//! fences are fragments (a bare `struct` body, a lone expression), so a check-based guard would
//! fail on correct docs. `tokenize` stops at the first error in a fence, so one fence reports at
//! most one failure per run — a fence that goes green after one fix may still hold a second bad
//! line. Guards `docs/gaps.md` W8-41: four lines used `;` as a statement separator, which is not
//! a token in Chezzi.

use std::fs;

/// Doc paths bundled by `chezzi docs`, derived from `src/main.rs`'s `DOC_TOPICS` entries
/// (`include_str!("../docs/<topic>.md")`), so the guard tracks the real bundle without a
/// hand-maintained list.
fn doc_paths() -> Vec<String> {
    let src = fs::read_to_string("src/main.rs").unwrap_or_else(|e| panic!("read src/main.rs: {e}"));
    let needle = "include_str!(\"../docs/";
    let mut paths = Vec::new();
    let mut rest = src.as_str();
    while let Some(idx) = rest.find(needle) {
        rest = &rest[idx + needle.len()..];
        let end = rest
            .find("\")")
            .unwrap_or_else(|| panic!("unterminated include_str! path"));
        paths.push(format!("docs/{}", &rest[..end]));
        rest = &rest[end..];
    }
    paths
}

#[test]
fn doc_paths_covers_the_bundled_topics() {
    let paths = doc_paths();
    assert!(!paths.is_empty(), "doc_paths() derived an empty list");
    assert!(paths.contains(&"docs/spec.md".to_string()), "{paths:?}");
    assert!(paths.contains(&"docs/syntax.md".to_string()), "{paths:?}");
    assert!(paths.contains(&"docs/stdlib.md".to_string()), "{paths:?}");
}

fn chezzi_fences(path: &str) -> Vec<(usize, String)> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut fences = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((i, line)) = lines.next() {
        if line.trim() == "```chezzi" {
            let start = i + 2; // 1-indexed, block starts next line
            let mut block = String::new();
            for (_, l) in lines.by_ref() {
                if l.trim() == "```" {
                    break;
                }
                block.push_str(l);
                block.push('\n');
            }
            fences.push((start, block));
        }
    }
    fences
}

#[test]
fn every_chezzi_fence_in_docs_lexes() {
    let mut failures = Vec::new();
    for path in doc_paths() {
        for (start_line, block) in chezzi_fences(&path) {
            if let Err(e) = chezzi::lexer::tokenize(&block) {
                let line = start_line + e.line - 1;
                failures.push(format!("{path}:{line}: {e} (fence at {path}:{start_line})"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "chezzi fences that fail to lex:\n{}",
        failures.join("\n")
    );
}
