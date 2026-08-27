//! Every ```chezzi fence in docs/*.md is copy-pasted by an LLM reading `chezzi docs`, so it must
//! lex. This walks docs/stdlib.md and docs/syntax.md, extracts each ```chezzi fenced block, and
//! feeds it to the real lexer (`chezzi::lexer::tokenize`) — the same lexer `chezzi tokens` runs.
//! Guards `docs/gaps.md` W8-41: four lines used `;` as a statement separator, which is not a
//! token in Chezzi.

use std::fs;

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
    for path in ["docs/stdlib.md", "docs/syntax.md"] {
        for (start_line, block) in chezzi_fences(path) {
            if let Err(e) = chezzi::lexer::tokenize(&block) {
                failures.push(format!("{path}:{start_line}: {e:?}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "chezzi fences that fail to lex:\n{}",
        failures.join("\n")
    );
}
