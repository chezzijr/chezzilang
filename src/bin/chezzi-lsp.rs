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

use std::collections::{HashMap, HashSet};

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

/// Everything a publish must decide ATOMICALLY: which buffers are open, and what each open buffer
/// currently reports, grouped by the TARGET uri it reports on. One struct behind one lock — see
/// `Backend::publish` — so a publish's decide-then-send sequence can never straddle a concurrent
/// `did_open`/`did_close` for the same source (F1/F2, adversarial review 2026-08-18): tower-lsp
/// dispatches notification handlers CONCURRENTLY (vendored `transport.rs`:
/// `DEFAULT_MAX_CONCURRENCY = 4`, `.buffer_unordered(...)`), so two `publish()` calls whose sources
/// share a target module could otherwise compute their unions in one order under a lock and then
/// deliver them in the OPPOSITE order after releasing it — `publish_diagnostics` is called with
/// `version: None`, so the client has no way to reject the stale delivery (F1). Worse, a `did_change`
/// whose diagnostics computation (unlocked — a real type-check) finishes after a concurrent
/// `did_close` for the same uri would, without the `open` check, re-insert the closed buffer as a live
/// source with nothing left to ever clear it (F2).
struct Published {
    /// URIs the client currently considers open. `did_open` inserts (eagerly, before the diagnostics
    /// computation — see `did_open`); `did_close` removes. `publish` consults this under the SAME lock
    /// it sends under and DECLINES ENTIRELY — no insert, no send — for a uri no longer here, so an
    /// in-flight computation from before a close can never resurrect it.
    open: HashSet<Url>,
    /// Per SOURCE buffer, the diagnostics that buffer's last check produced, grouped by TARGET uri.
    /// A target URI is published as the UNION across every source that currently reports on it —
    /// `publishDiagnostics` replaces a URI's whole array, so a server with cross-file diagnostics MUST
    /// merge here or one buffer's publish silently erases another's (A5/F2: two open buffers importing
    /// the same broken module must each keep the other's squiggle alive on that module's URI).
    /// ponytail: the union is recomputed by a linear scan over every open source on every publish/close
    /// — fine for an editor session's open-buffer count, not indexed. Upgrade to a target→sources
    /// reverse index if a project with many simultaneously-open buffers on one shared broken module
    /// ever makes this measurably slow.
    by_source: HashMap<Url, HashMap<Url, Vec<Diagnostic>>>,
}

struct Backend {
    client: Client,
    /// Live document text, keyed by URI. `tokio::sync::Mutex` (no extra dep) — contention is trivial.
    /// NEVER held at the same time as `published` (a previous review verified this; every call site
    /// takes `docs` in its own scope and lets the guard drop before touching `published`, so there is
    /// no second lock to order against the one held across sends below — see `Published`'s doc).
    docs: tokio::sync::Mutex<HashMap<Url, String>>,
    /// See [`Published`]. Held across the `publish_diagnostics` awaits in `publish`/`did_close`
    /// (deliberately — a `tokio::sync::Mutex` supports this): that is what makes "last update wins"
    /// true AT THE CLIENT, not only in this map. It serializes diagnostic delivery across every open
    /// buffer; that is the correct trade for a diagnostics server — a publish is a handful of small
    /// JSON notifications over the same stdio pipe — not a throughput bottleneck. Do not "optimize"
    /// this back to release-before-send; that is exactly the bug F1/F2 fixed.
    published: tokio::sync::Mutex<Published>,
}

impl Backend {
    /// Type-check `text` (the buffer at `uri`) and publish diagnostics to the URI each ACTUALLY
    /// belongs to (`chezzi::editor::Diag::file`, mapped to a URI via `Url::from_file_path` — falling
    /// back to the edited `uri` for an entry-module diagnostic, `None`, or a path that fails to convert,
    /// so a diagnostic is never dropped). The edited `uri` always gets an entry, even empty, so fixing
    /// the last error in the open file still clears its own squiggle. Replaces `uri`'s own entry in
    /// `published`, then republishes the UNION over every source for each target `uri` used to report
    /// on before or reports on now — merging in every other open buffer that still legitimately reports
    /// on a shared target, and clearing a target no source reports on any more.
    ///
    /// The diagnostics computation above runs UNLOCKED (a real type-check, not cheap); everything from
    /// here down — the open-check, the union recompute, and the sends — runs under one continuous hold
    /// of `published`'s lock, so a `did_close` that completed while this computation was running is
    /// authoritative: this call sees `uri` missing from `open` and returns without sending anything.
    async fn publish(&self, uri: Url, text: &str) {
        let path = uri
            .to_file_path()
            .unwrap_or_else(|_| std::path::PathBuf::from(uri.path()));
        let diags = chezzi::editor::diagnostics(&path, text);

        let mut new_targets: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
        new_targets.entry(uri.clone()).or_default();
        for d in diags {
            let target = d
                .file
                .as_deref()
                .and_then(|p| Url::from_file_path(p).ok())
                .unwrap_or_else(|| uri.clone());
            new_targets.entry(target).or_default().push(Diagnostic {
                range: Range {
                    start: Position::new(d.line, d.col),
                    end: Position::new(d.end_line, d.end_col),
                },
                severity: Some(match d.severity {
                    chezzi::editor::Severity::Error => DiagnosticSeverity::ERROR,
                    chezzi::editor::Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                source: Some("chezzi".to_string()),
                message: d.message,
                ..Default::default()
            });
        }

        let mut published = self.published.lock().await;
        let Some(unions) = decide_publish(&mut published, &uri, new_targets) else {
            return;
        };
        // Still holding `published` here — see the field doc on why that is deliberate (F1).
        for (target, diags) in unions {
            self.client.publish_diagnostics(target, diags, None).await;
        }
    }
}

/// The pure, synchronous decision core of `Backend::publish`: given the current `Published` state and
/// the freshly-computed `new_targets` for `uri`, either decline (`None`, `uri` no longer in `open` —
/// F2: no insert, no mutation at all) or commit `new_targets` as `uri`'s entry and return the unions to
/// send for every target `uri` used to report on before or reports on now.
///
/// Factored out of `publish` specifically so the F2 race — a `did_close` that removed `uri` from
/// `open` before a late-finishing `publish` call reaches the same lock — is reproducible by
/// CONSTRUCTING that `Published` state directly and calling this function, with no timing dependency.
/// See `decide_publish_declines_a_uri_that_closed_first` in `mod tests` below; `tests/lsp_smoke.rs`
/// documents why the end-to-end stdio harness cannot force this interleaving on demand.
///
/// Caller must hold `published`'s lock across both this call and the resulting sends (F1) — that
/// discipline lives in `publish`/`did_close`, not here, since holding a lock across an `.await` isn't
/// expressible in a plain sync fn signature.
fn decide_publish(
    published: &mut Published,
    uri: &Url,
    new_targets: HashMap<Url, Vec<Diagnostic>>,
) -> Option<Vec<(Url, Vec<Diagnostic>)>> {
    if !published.open.contains(uri) {
        // Closed (or never opened) by the time this computation finished — decline outright rather
        // than resurrecting it (F2). No insert, no send.
        return None;
    }
    let old_targets = published.by_source.remove(uri).unwrap_or_default();
    let mut affected: HashSet<Url> = old_targets.keys().cloned().collect();
    affected.extend(new_targets.keys().cloned());
    published.by_source.insert(uri.clone(), new_targets);
    Some(union_targets_locked(&published.by_source, affected))
}

/// The pure, synchronous decision core of `did_close`: removes `uri` from `open` (so a racing
/// `decide_publish` for it declines from here on) and from `by_source`, and returns the unions to send
/// for every target `uri` used to report on. Caller must hold `published`'s lock across both this call
/// and the resulting sends (F1), same as `decide_publish`.
fn decide_close(published: &mut Published, uri: &Url) -> Vec<(Url, Vec<Diagnostic>)> {
    published.open.remove(uri);
    let old_targets = published.by_source.remove(uri).unwrap_or_default();
    let affected: HashSet<Url> = old_targets.keys().cloned().collect();
    union_targets_locked(&published.by_source, affected)
}

/// Compute the UNION of every source's diagnostics for each of `targets`, iterating sources in
/// SORTED uri order so the merged array's element order is deterministic across publishes (a client
/// must not see a shared module's diagnostics reorder on every unrelated keystroke). Caller must
/// already hold `published`'s lock (so the read is a consistent snapshot with whatever update it just
/// made) — takes `Published::by_source` directly rather than the whole struct, since neither caller
/// needs `open` here.
fn union_targets_locked(
    by_source: &HashMap<Url, HashMap<Url, Vec<Diagnostic>>>,
    targets: HashSet<Url>,
) -> Vec<(Url, Vec<Diagnostic>)> {
    let mut sources: Vec<&Url> = by_source.keys().collect();
    sources.sort();
    targets
        .into_iter()
        .map(|target| {
            let mut merged = Vec::new();
            for source in &sources {
                if let Some(diags) = by_source[*source].get(&target) {
                    merged.extend(diags.iter().cloned());
                }
            }
            (target, merged)
        })
        .collect()
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
        // Mark open EAGERLY, before `publish`'s (unlocked, possibly slow) diagnostics computation
        // starts — so a `did_close` racing this open always has something to remove, and `publish`'s
        // own open-check (not this insert) is what then declines a stale publish if the close won (F2).
        self.published.lock().await.open.insert(uri.clone());
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
        let uri = params.text_document.uri;
        self.docs.lock().await.remove(&uri);

        // A closed buffer stops contributing to `published` entirely: remove it from BOTH `open` and
        // `by_source`, then republish the UNION (over the still-open sources) for every target this
        // buffer used to report on, so a cross-module squiggle whose only source just closed is
        // cleared instead of orphaned. Same atomic discipline as `publish` (see `Published`'s field
        // doc): the union is computed AND sent while still holding the lock, so a concurrent `publish`
        // for a shared target can't interleave its own compute/send with this one (F1), and removing
        // from `open` here is what makes a racing `publish` for THIS uri decline instead of
        // resurrecting it (F2).
        let mut published = self.published.lock().await;
        let unions = decide_close(&mut published, &uri);
        for (target, diags) in unions {
            self.client.publish_diagnostics(target, diags, None).await;
        }
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
            Some(doc) => format!(
                "{}\n\n```\n{}\n```",
                escape_brackets_outside_code(&untag_fences(doc)),
                info.display
            ),
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

/// Backslash-escape every `[`/`]` that is OUTSIDE an inline code span and outside a fenced code
/// block, so a bare type reference in a doc-comment (`Heap[T]`, `List[T]()`, `xs[i]`) renders
/// literally instead of being eaten by the Markdown hover renderer as link syntax (`[text]` /
/// `[text](url)` — `List[T]()` is literally an empty-URL link, so an unescaped doc shows `ListT`).
/// Brackets INSIDE a code span (`` `List[T]` ``) or a fenced block are left untouched — there they
/// already render verbatim and a backslash would show through. CommonMark renders `\[` as a literal
/// `[`. Applied after [`untag_fences`] in the hover render path (the type fence appended afterwards
/// is not passed through here — it is already literal monospace).
fn escape_brackets_outside_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut in_fence = false;
    for (i, line) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        // A fenced-block delimiter (3+ of the same fence char) toggles fence state; emit verbatim.
        if let Some(d @ ('`' | '~')) = trimmed.chars().next()
            && trimmed.chars().take_while(|&c| c == d).count() >= 3
        {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence {
            out.push_str(line);
            continue;
        }
        // Outside a fence: escape brackets, but skip those inside an inline code span. A run of
        // backticks is one span delimiter (so `` ``x[0]`` `` keeps its brackets too).
        let mut in_code = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '`' => {
                    in_code = !in_code;
                    out.push('`');
                    while chars.peek() == Some(&'`') {
                        out.push('`');
                        chars.next();
                    }
                }
                '[' | ']' if !in_code => {
                    out.push('\\');
                    out.push(c);
                }
                _ => out.push(c),
            }
        }
    }
    if s.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        docs: tokio::sync::Mutex::new(HashMap::new()),
        published: tokio::sync::Mutex::new(Published {
            open: HashSet::new(),
            by_source: HashMap::new(),
        }),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::{
        Published, decide_close, decide_publish, escape_brackets_outside_code, legend_token_types,
        untag_fences,
    };
    use std::collections::{HashMap, HashSet};
    use tower_lsp::lsp_types::Url;

    fn url(s: &str) -> Url {
        s.parse().unwrap()
    }

    /// F2, reproduced deterministically (no timing dependency): a `did_close` that already removed
    /// `uri` from `open` (and dropped its `by_source` entry) before a late-finishing `publish` call
    /// reaches the lock. `decide_publish` must decline outright — return `None`, insert nothing — not
    /// resurrect the closed buffer as a live diagnostics source.
    #[test]
    fn decide_publish_declines_a_uri_that_closed_first() {
        let mut published = Published {
            open: HashSet::new(),
            by_source: HashMap::new(),
        };
        let uri = url("file:///tmp/app.chz");
        // uri is NOT in `open` — simulating did_close having already run (or the uri never having been
        // opened at all).
        let mut new_targets: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> = HashMap::new();
        new_targets.insert(uri.clone(), Vec::new());

        let result = decide_publish(&mut published, &uri, new_targets);

        assert!(
            result.is_none(),
            "a publish for a uri not in `open` must decline"
        );
        assert!(
            !published.by_source.contains_key(&uri),
            "a declined publish must not insert into by_source either"
        );
    }

    /// The exact F1/F2 race scenario, forced by direct construction: `did_open` marks `uri` open;
    /// `did_close` then races ahead and completes (removing `uri` from `open` and from `by_source`,
    /// exactly as `decide_close` does); only THEN does the in-flight `did_change`'s `publish` call
    /// reach the lock with its (freshly computed, still-broken) diagnostics. It must decline — the
    /// closed buffer must not reappear as a live source, and nothing is left to ever clear it if it did
    /// (the bug this whole fix targets).
    #[test]
    fn decide_publish_after_a_racing_close_never_resurrects_the_source() {
        let mut published = Published {
            open: HashSet::new(),
            by_source: HashMap::new(),
        };
        let uri = url("file:///tmp/app.chz");
        let shared_target = url("file:///tmp/core/badmod.chz");

        // did_open: marks open, an earlier publish already populated by_source for this source.
        published.open.insert(uri.clone());
        let mut first_targets: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> = HashMap::new();
        first_targets.insert(shared_target.clone(), vec![Default::default()]);
        published.by_source.insert(uri.clone(), first_targets);

        // did_close wins the race and runs to completion first.
        let close_unions = decide_close(&mut published, &uri);
        assert!(
            close_unions
                .iter()
                .any(|(target, diags)| *target == shared_target && diags.is_empty()),
            "did_close must clear the shared target when it was the sole source"
        );
        assert!(!published.open.contains(&uri));
        assert!(!published.by_source.contains_key(&uri));

        // The in-flight did_change's publish() now reaches the lock with a fresh (still-broken)
        // diagnostics computation for the same uri.
        let mut late_targets: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> = HashMap::new();
        late_targets.insert(shared_target.clone(), vec![Default::default()]);
        let result = decide_publish(&mut published, &uri, late_targets);

        assert!(
            result.is_none(),
            "a publish racing behind a completed did_close must decline, not resurrect the source"
        );
        assert!(
            !published.open.contains(&uri),
            "declining must not reopen the uri"
        );
        assert!(
            !published.by_source.contains_key(&uri),
            "declining must not reinsert the closed source's entry"
        );
    }

    /// `decide_close` on a uri that is the SOLE source for a target clears that target to empty (not
    /// merely absent from the returned unions) — a client must see an explicit empty array to actually
    /// clear a previously-shown squiggle.
    #[test]
    fn decide_close_clears_open_and_by_source() {
        let mut published = Published {
            open: HashSet::new(),
            by_source: HashMap::new(),
        };
        let uri = url("file:///tmp/app.chz");
        let target = url("file:///tmp/app.chz");
        published.open.insert(uri.clone());
        let mut targets: HashMap<Url, Vec<tower_lsp::lsp_types::Diagnostic>> = HashMap::new();
        targets.insert(target.clone(), vec![Default::default()]);
        published.by_source.insert(uri.clone(), targets);

        let unions = decide_close(&mut published, &uri);

        assert!(!published.open.contains(&uri));
        assert!(!published.by_source.contains_key(&uri));
        assert!(
            unions
                .iter()
                .any(|(t, diags)| *t == target && diags.is_empty()),
            "the sole source's own target must clear to an empty array: {unions:?}"
        );
    }

    #[test]
    fn escape_brackets_outside_code_escapes_bare_type_refs() {
        // Bare `Heap[T]` / `List[T]()` in prose would be eaten as Markdown link syntax (rendering
        // `HeapT` / `ListT`); escape so they render literally.
        assert_eq!(
            escape_brackets_outside_code("Heap[T] — a heap; build with List[T]()"),
            "Heap\\[T\\] — a heap; build with List\\[T\\]()"
        );
    }

    #[test]
    fn escape_brackets_outside_code_leaves_code_spans_and_fences() {
        // Brackets inside an inline code span render verbatim already — must NOT be escaped (a
        // backslash would show through inside the span).
        assert_eq!(
            escape_brackets_outside_code("use `List[T]` here"),
            "use `List[T]` here"
        );
        // Brackets inside a fenced block are likewise left untouched.
        assert_eq!(
            escape_brackets_outside_code("```\nm[k] = v\n```"),
            "```\nm[k] = v\n```"
        );
        // Mixed: bare ref escaped, code-span ref preserved, on the same line.
        assert_eq!(
            escape_brackets_outside_code("Heap[T] vs `Heap[T]`"),
            "Heap\\[T\\] vs `Heap[T]`"
        );
    }

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
