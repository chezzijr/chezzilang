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
        // Render the inferred type as a fenced `chezzi` code block so editors syntax-highlight it.
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: format!("```chezzi\n{}\n```", info.display),
            }),
            range: None,
        }))
    }
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
    use super::legend_token_types;

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
