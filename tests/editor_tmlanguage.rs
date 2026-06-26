//! Generator + CI drift-guard for the VSCode TextMate grammar.
//!
//! `chezzi::editor::tmlanguage_json()` is the single source of truth for
//! `editors/vscode/syntaxes/chezzi.tmLanguage.json` — its keyword/operator lists are derived from the
//! lexer's `KEYWORDS` / `PUNCTUATION` tables. This test is BOTH the generator and the guard:
//!
//!   * `UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage` writes the committed file.
//!   * a plain `cargo test` asserts the committed file still matches the generator (so adding a
//!     keyword without regenerating fails CI instead of silently shipping a stale grammar).

use std::path::PathBuf;

fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("editors/vscode/syntaxes/chezzi.tmLanguage.json")
}

#[test]
fn tmlanguage_matches_committed() {
    let generated = chezzi::editor::tmlanguage_json();
    let path = committed_path();

    if std::env::var_os("UPDATE_EDITOR_ASSETS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &generated).unwrap();
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing {}; regenerate with UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage",
            path.display()
        )
    });
    assert_eq!(
        committed, generated,
        "TextMate grammar is stale; regenerate with \
         UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage"
    );
}

#[test]
fn tmlanguage_has_core_structure() {
    let g = chezzi::editor::tmlanguage_json();
    assert!(g.contains("\"scopeName\": \"source.chezzi\""));
    // single-sourced from the lexer: a keyword and an operator must both appear.
    assert!(
        g.contains("newtype"),
        "keyword alternation missing a keyword"
    );
    assert!(g.contains("keyword.control.chezzi"));
    assert!(g.contains("constant.numeric.chezzi"));
}
