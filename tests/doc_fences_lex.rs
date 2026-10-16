//! Every ```chezzi fence in the docs `chezzi docs` bundles is copy-pasted by an LLM reading that
//! bundle, so it must lex. The file list comes from `src/main.rs`'s `DOC_TOPICS` `include_str!`
//! calls, not a hardcoded list, so a new bundled topic is covered automatically. This lexes only
//! (`chezzi::lexer::tokenize` — the same lexer `chezzi tokens` runs) and never type-checks: most
//! fences are fragments (a bare `struct` body, a lone expression), so a check-based guard would
//! fail on correct docs. `tokenize` stops at the first error in a fence, so one fence reports at
//! most one failure per run — a fence that goes green after one fix may still hold a second bad
//! line. Guards `docs/contributing/gaps.md` W8-41: four lines used `;` as a statement separator, which is not
//! a token in Chezzi.

use std::fs;

/// Doc paths bundled by `chezzi docs`, derived from `src/main.rs`'s `DOC_TOPICS` entries
/// (`include_str!("../docs/reference/<topic>/<chapter>.md")`), so the guard tracks the real bundle
/// without a hand-maintained list.
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

/// Every chapter file under `docs/reference/<topic>/` is bundled, so a new or renamed chapter that
/// `DOC_TOPICS` misses fails here instead of vanishing from `chezzi docs`.
#[test]
fn every_reference_chapter_is_bundled() {
    let paths = doc_paths();
    let mut chapters = Vec::new();
    for topic in fs::read_dir("docs/reference").unwrap() {
        let topic = topic.unwrap().path();
        if !topic.is_dir() {
            continue;
        }
        for chapter in fs::read_dir(&topic).unwrap() {
            let chapter = chapter.unwrap().path();
            if chapter.extension().is_some_and(|x| x == "md") {
                chapters.push(chapter.display().to_string());
            }
        }
    }
    assert!(chapters.len() > 20, "found only {chapters:?}");
    let missing: Vec<&String> = chapters.iter().filter(|c| !paths.contains(c)).collect();
    assert!(
        missing.is_empty(),
        "chapters not in src/main.rs DOC_TOPICS: {missing:?}"
    );
    let stale: Vec<&String> = paths.iter().filter(|p| !chapters.contains(p)).collect();
    assert!(
        stale.is_empty(),
        "DOC_TOPICS names files outside docs/reference/<topic>/: {stale:?}"
    );
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
