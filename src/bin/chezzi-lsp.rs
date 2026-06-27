//! `chezzi-lsp` — a Language Server for Chezzi over stdio, for editors with a built-in LSP client
//! (primary target: neovim; secondary: VSCode). It is a THIN shell: all real work delegates to
//! `chezzi::editor`, which wraps the existing compiler front-end (lexer / parser / checker /
//! resolver). The server provides:
//!
//!   * **push diagnostics** — on `didOpen` / `didChange` / `didSave` it type-checks the live buffer
//!     (`chezzi::editor::diagnostics`) and publishes squiggles.
//!   * **semantic tokens** — `textDocument/semanticTokens/full` classifies the lexer's token stream
//!     (`chezzi::editor::semantic_tokens`), so a new keyword in the lexer highlights with no extra
//!     grammar to maintain.
//!
//! Built only with the `lsp` feature (`cargo build --features lsp --bin chezzi-lsp`); the heavy async
//! deps (tower-lsp + tokio) never touch the default build.

use std::collections::HashMap;

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// The legend, in the exact index order of `chezzi::editor`'s token-type constants
/// (`KEYWORD=0 … VARIABLE=5`). The semantic-token `u32` types index this list.
fn legend_token_types() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::KEYWORD,
        SemanticTokenType::OPERATOR,
        SemanticTokenType::STRING,
        SemanticTokenType::NUMBER,
        SemanticTokenType::COMMENT,
        SemanticTokenType::VARIABLE,
        SemanticTokenType::FUNCTION,
        SemanticTokenType::TYPE,
        SemanticTokenType::PROPERTY,
        SemanticTokenType::PARAMETER,
    ]
}

struct Backend {
    client: Client,
    /// Live document text, keyed by URI. `tokio::sync::Mutex` (no extra dep) — contention is trivial.
    docs: tokio::sync::Mutex<HashMap<Url, String>>,
}

impl Backend {
    /// Type-check `text` and publish the resulting diagnostics for `uri`.
    async fn publish(&self, uri: Url, text: &str) {
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| std::path::PathBuf::from(uri.path()));
        let diags = chezzi::editor::diagnostics(&path, text)
            .into_iter()
            .map(|d| Diagnostic {
                range: Range {
                    start: Position::new(d.line, d.col),
                    end: Position::new(d.end_line, d.end_col),
                },
                severity: Some(DiagnosticSeverity::ERROR),
                source: Some("chezzi".to_string()),
                message: d.message,
                ..Default::default()
            })
            .collect();
        self.client.publish_diagnostics(uri, diags, None).await;
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "chezzi-lsp".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            capabilities: ServerCapabilities {
                // Full-document sync: each change carries the whole buffer (simplest, robust).
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: legend_token_types(),
                                token_modifiers: vec![],
                            },
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: Some(false),
                            ..Default::default()
                        },
                    ),
                ),
                // Hover (`K`): show the checker-inferred type of the symbol under the cursor.
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "chezzi-lsp ready")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = params.text_document.text;
        self.docs.lock().await.insert(uri.clone(), text.clone());
        self.publish(uri, &text).await;
    }

    async fn did_change(&self, mut params: DidChangeTextDocumentParams) {
        // FULL sync → the last change event holds the entire new document.
        let uri = params.text_document.uri;
        let Some(change) = params.content_changes.pop() else {
            return;
        };
        let text = change.text;
        self.docs.lock().await.insert(uri.clone(), text.clone());
        self.publish(uri, &text).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        let text = match params.text {
            Some(t) => t,
            None => self
                .docs
                .lock()
                .await
                .get(&uri)
                .cloned()
                .unwrap_or_default(),
        };
        self.publish(uri, &text).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.docs.lock().await.remove(&params.text_document.uri);
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let uri = params.text_document.uri;
        let text = self
            .docs
            .lock()
            .await
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        let toks = chezzi::editor::semantic_tokens(&text);
        let flat = chezzi::editor::encode_semantic_tokens(&toks);
        let data: Vec<SemanticToken> = flat
            .chunks_exact(5)
            .map(|c| SemanticToken {
                delta_line: c[0],
                delta_start: c[1],
                length: c[2],
                token_type: c[3],
                token_modifiers_bitset: c[4],
            })
            .collect();
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let text = self
            .docs
            .lock()
            .await
            .get(&uri)
            .cloned()
            .unwrap_or_default();
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| std::path::PathBuf::from(uri.path()));
        let Some(info) = chezzi::editor::hover(&path, &text, pos.line, pos.character) else {
            return Ok(None);
        };
        // Render the inferred type as a PLAIN fenced code block — deliberately NOT tagged `chezzi`.
        // No `chezzi` treesitter parser exists, and a language-tagged fence makes some editors'
        // markdown hover renderers (e.g. Neovim 0.12) attempt a missing-language injection and crash
        // on the float instead of skipping it. An untagged fence still renders as monospace everywhere.
        // When the symbol has a doc-comment, render it as plain markdown lines ABOVE the type fence.
        // The doc is user-authored free text and may itself contain a language-tagged ```` ```lang ````
        // fence; strip the language tag from any fence in it first, for the SAME reason the type fence
        // above is untagged — a missing-language injection crashes some markdown hover renderers
        // (Neovim 0.12, commit 0f36a59). `untag_fences` keeps the (untagged) fence, so code blocks in
        // docs still render as monospace; it just removes the injection trigger.
        let value = match &info.doc {
            Some(doc) => format!("{}\n\n```\n{}\n```", untag_fences(doc), info.display),
            None => format!("```\n{}\n```", info.display),
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }
}

/// Strip the language tag from every fenced-code-block delimiter in `s` (a line whose first
/// non-whitespace is a run of 3+ backticks OR tildes), leaving the bare delimiter run. A
/// language-tagged fence reaching a markdown hover renderer triggers a missing-language treesitter
/// injection that crashes some clients (Neovim 0.12) — see commit 0f36a59. CommonMark §4.5 treats
/// ```` ``` ```` and `~~~` as fenced code blocks alike, and treesitter-markdown injects on the
/// info-string of BOTH regardless of delimiter, so both must be untagged. An untagged fence still
/// renders as plain monospace.
fn untag_fences(s: &str) -> String {
    s.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            // A fence delimiter is a leading run of 3+ of the same fence char (backtick or tilde).
            if let Some(delim @ ('`' | '~')) = trimmed.chars().next() {
                let run: String = trimmed.chars().take_while(|&c| c == delim).collect();
                if run.len() >= 3 {
                    let indent = &line[..line.len() - trimmed.len()];
                    return format!("{indent}{run}");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        docs: tokio::sync::Mutex::new(HashMap::new()),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::{legend_token_types, untag_fences};

    #[test]
    fn untag_fences_strips_language_tag() {
        // A doc-comment containing a language-tagged fence must NOT yield a tagged fence in the
        // rendered hover markup (regression guard for the Neovim injection crash, commit 0f36a59).
        let doc = "Example:\n```python\nfoo()\n```";
        let out = untag_fences(doc);
        assert!(
            !out.contains("```python"),
            "language tag must be stripped: {out:?}"
        );
        assert_eq!(out, "Example:\n```\nfoo()\n```");
    }

    #[test]
    fn untag_fences_strips_tilde_fence_tag() {
        // CommonMark tilde fences inject identically to backtick fences (treesitter-markdown matches
        // the info-string of both), so `~~~lang` must also be untagged — else it re-arms 0f36a59.
        let doc = "Example:\n~~~chezzi\nfoo()\n~~~";
        let out = untag_fences(doc);
        assert!(
            !out.contains("~~~chezzi"),
            "tilde language tag must be stripped: {out:?}"
        );
        assert_eq!(out, "Example:\n~~~\nfoo()\n~~~");
        // A short tilde run (< 3, e.g. a strikethrough or stray tilde) is not a fence — leave it.
        assert_eq!(untag_fences("~~not a fence~~"), "~~not a fence~~");
    }

    #[test]
    fn untag_fences_preserves_plain_text_and_indented_fences() {
        assert_eq!(untag_fences("just text\nno fence"), "just text\nno fence");
        // Indented / longer fences keep their indent and tick-count, lose only the tag.
        assert_eq!(
            untag_fences("    ````rust\n    x\n    ````"),
            "    ````\n    x\n    ````"
        );
    }

    /// The server's advertised legend MUST agree, name-for-name and in index order, with
    /// `chezzi::editor::SEMANTIC_TOKEN_TYPES` — the `u32` token types `semantic_tokens` emits index
    /// THAT slice, so any drift mis-colors every token. Guards both lists together.
    #[test]
    fn legend_matches_editor() {
        let types = legend_token_types();
        let legend: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
        assert_eq!(
            legend.as_slice(),
            chezzi::editor::SEMANTIC_TOKEN_TYPES,
            "chezzi-lsp legend and editor::SEMANTIC_TOKEN_TYPES disagree"
        );
    }
}
