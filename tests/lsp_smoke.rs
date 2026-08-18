//! End-to-end smoke test for the `chezzi-lsp` binary: a real stdio JSON-RPC handshake
//! (initialize → initialized → didOpen of a broken document) must yield a `publishDiagnostics`
//! notification carrying at least one diagnostic. Only built under `--features lsp`.
#![cfg(feature = "lsp")]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Frame a JSON-RPC payload with the LSP `Content-Length` header and write it.
fn send(stdin: &mut impl Write, body: &str) {
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
    stdin.flush().unwrap();
}

/// Read exactly one framed LSP message body from `out`, or `None` at EOF.
fn read_message(out: &mut BufReader<ChildStdout>) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if out.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().ok()?);
        }
    }
    let n = content_length?;
    let mut buf = vec![0u8; n];
    out.read_exact(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

struct ServerGuard(Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn the server and run the initialize → initialized handshake, returning the piped stdin and a
/// channel of framed response bodies. Shared by the diagnostics and hover round-trip tests.
fn start_server() -> (
    impl Write,
    mpsc::Receiver<String>,
    ServerGuard,
    String, // the initialize response body
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi-lsp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let guard = ServerGuard(child);

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
    );
    let init_resp = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("initialize response");
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    );
    (stdin, rx, guard, init_resp)
}

#[test]
fn hover_round_trip() {
    let (mut stdin, rx, _guard, init_resp) = start_server();
    // The server must advertise hover support in its capabilities.
    assert!(
        init_resp.contains("hoverProvider"),
        "initialize did not advertise hoverProvider: {init_resp}"
    );

    // didOpen a CLEAN document, then hover the binding `x` (line 0, char 0).
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover.chz","languageId":"chezzi","version":1,"text":"x := 41\n"}}}"#,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover.chz"},"position":{"line":0,"character":0}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_hover = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                // The hover reply is the response to id 2 carrying MarkupContent.
                if msg.contains("\"id\":2") {
                    assert!(
                        msg.contains("int"),
                        "hover response missing the inferred type: {msg}"
                    );
                    assert!(
                        msg.contains("```"),
                        "hover response missing the code block fence: {msg}"
                    );
                    // The fence must NOT be tagged `chezzi`: no chezzi treesitter parser exists, and a
                    // language-tagged fence crashes some editors' markdown hover renderers (Neovim 0.12)
                    // on a missing-language injection. Guard against regressing back to a tagged fence.
                    assert!(
                        !msg.contains("```chezzi"),
                        "hover fence must stay untagged to avoid the treesitter injection crash: {msg}"
                    );
                    saw_hover = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_hover, "never received a hover response for id 2");
}

#[test]
fn hover_doc_comment_round_trip() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();
    // didOpen a document with a `#` doc-comment above the binding `x`, then hover `x` (line 1, char 0).
    send(
        &mut stdin,
        r##"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_doc.chz","languageId":"chezzi","version":1,"text":"# the meaning of life\nx := 41\n"}}}"##,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_doc.chz"},"position":{"line":1,"character":0}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_hover = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("\"id\":2") {
                    // The doc text appears in the rendered hover…
                    assert!(
                        msg.contains("the meaning of life"),
                        "hover response missing the doc-comment text: {msg}"
                    );
                    // …ABOVE the type code fence: the doc substring must precede the first fence.
                    let doc_at = msg.find("the meaning of life").unwrap();
                    let fence_at = msg.find("```").expect("hover response missing the fence");
                    assert!(
                        doc_at < fence_at,
                        "doc-comment must render ABOVE the type fence: {msg}"
                    );
                    saw_hover = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_hover, "never received a hover response for id 2");
}

#[test]
fn hover_fn_decl_name_round_trip() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();
    // didOpen a document with a free function, then hover the FUNCTION-NAME token at its declaration
    // (`foo` at line 0, char 3) — the decl-site fn-name hover must surface the signature, matching
    // the call-site hover.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_fn.chz","languageId":"chezzi","version":1,"text":"fn foo(bar: int) -> int:\n    return bar\n"}}}"#,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_fn.chz"},"position":{"line":0,"character":3}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_hover = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("\"id\":2") {
                    // The fn signature is rendered inside the code fence.
                    assert!(
                        msg.contains("fn(int) -> int"),
                        "hover response missing the fn signature at the decl name: {msg}"
                    );
                    assert!(
                        msg.contains("```"),
                        "hover response missing the code block fence: {msg}"
                    );
                    saw_hover = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_hover, "never received a hover response for id 2");
}

#[test]
fn hover_enum_variant_decl_name_round_trip() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();
    // didOpen a document with an enum, then hover the VARIANT-NAME token at its declaration
    // (`Val` at line 1, char 4) — the decl-site variant hover must surface the ctor signature,
    // matching the use-site hover (`fn(int) -> Col`).
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_variant.chz","languageId":"chezzi","version":1,"text":"enum Col:\n    Val(int)\n"}}}"#,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_hover_variant.chz"},"position":{"line":1,"character":4}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_hover = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("\"id\":2") {
                    // The variant ctor signature is rendered inside the code fence.
                    assert!(
                        msg.contains("fn(int) -> Col"),
                        "hover response missing the variant ctor signature at the decl name: {msg}"
                    );
                    assert!(
                        msg.contains("```"),
                        "hover response missing the code block fence: {msg}"
                    );
                    saw_hover = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(saw_hover, "never received a hover response for id 2");
}

/// Extract the flat `u32` array following `"data":[` in a `semanticTokens/full` JSON-RPC response
/// body (hand-parsed rather than pulling in a JSON dep just for this one field — `tower-lsp` never
/// emits nested brackets inside this particular array).
fn extract_data_array(msg: &str) -> Vec<u32> {
    let start = msg
        .find("\"data\":[")
        .expect("response missing \"data\" array")
        + "\"data\":[".len();
    let end = msg[start..].find(']').expect("unterminated \"data\" array") + start;
    msg[start..end]
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse().expect("data element must be a u32"))
        .collect()
}

/// Minimal JSON string escaping for embedding source text in a hand-written JSON-RPC payload above:
/// these fixtures only ever contain newlines and printable ASCII, so handling `"`, `\` and `\n` is
/// enough and a full JSON string encoder is unneeded.
fn escape_json_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[test]
fn semantic_tokens_full_round_trip() {
    let (mut stdin, rx, _guard, init_resp) = start_server();
    assert!(
        init_resp.contains("semanticTokensProvider"),
        "initialize did not advertise semanticTokensProvider: {init_resp}"
    );

    // Shallow buffer: didOpen, then request semanticTokens/full and check the encoding shape.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_semtok.chz","languageId":"chezzi","version":1,"text":"x := 41\n"}}}"#,
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_semtok.chz"}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_tokens = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("\"id\":2") {
                    let data = extract_data_array(&msg);
                    assert!(
                        !data.is_empty(),
                        "expected non-empty semantic-token data: {msg}"
                    );
                    assert_eq!(
                        data.len() % 5,
                        0,
                        "semantic-token data must be a flat multiple of 5: {msg}"
                    );
                    for chunk in data.chunks_exact(5) {
                        let token_type = chunk[3];
                        assert!(
                            (token_type as usize) < chezzi::editor::SEMANTIC_TOKEN_TYPES.len(),
                            "token_type {token_type} out of legend bounds: {msg}"
                        );
                    }
                    saw_tokens = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_tokens,
        "never received a semanticTokens/full response for id 2"
    );
}

#[test]
fn semantic_tokens_full_deep_buffer_round_trip() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();

    // The R3 deep-but-parser-accepted shape (W7-50): R(0) = "a", R(k) = "f(g(" + R(k-1) + ")" +
    // ".f".repeat(498) + ")", lv = 15 — the deepest level the parser still accepts. Round-tripping it
    // through the real stdio server (not just the in-process unit test) confirms the LSP's actual
    // tokio worker survives the request end to end, not just the library call in isolation.
    fn r(k: usize) -> String {
        if k == 0 {
            "a".to_string()
        } else {
            format!("f(g({}){})", r(k - 1), ".f".repeat(498))
        }
    }
    let src = format!("x := {}\n", r(15));
    let text_json = escape_json_string(&src);

    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///tmp/chezzi_lsp_semtok_deep.chz","languageId":"chezzi","version":1,"text":"{text_json}"}}}}}}"#
        ),
    );
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/semanticTokens/full","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_semtok_deep.chz"}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_response = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("\"id\":2") {
                    // Require a `data` field, not just any id-2 response — a JSON-RPC `error` object
                    // for id 2 would otherwise also match and pass.
                    assert!(
                        msg.contains("\"data\""),
                        "expected a semanticTokens/full result (with \"data\"), got an error response: {msg}"
                    );
                    saw_response = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }
    assert!(
        saw_response,
        "never received a semanticTokens/full response for the deep buffer (id 2) — server likely crashed"
    );
}

#[test]
fn diagnostics_round_trip() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi-lsp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi-lsp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = ServerGuard(child);

    // Reader thread: forward every framed message body over a channel so the main thread can wait
    // with a timeout instead of blocking forever on a hung server.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        while let Some(msg) = read_message(&mut reader) {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    // initialize — and WAIT for the result before sending anything else. The LSP spec requires the
    // client to await the `initialize` response before the `initialized` notification; tower-lsp
    // enforces this by dropping messages that arrive before initialize completes.
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#,
    );
    let init_resp = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("initialize response");
    assert!(
        init_resp.contains("\"id\":1") && init_resp.contains("capabilities"),
        "unexpected initialize response: {init_resp}"
    );

    // initialized
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#,
    );
    // didOpen a syntactically broken document → expect a diagnostic
    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///tmp/chezzi_lsp_smoke.chz","languageId":"chezzi","version":1,"text":"x := = 5\n"}}}"#,
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_diag = false;
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("textDocument/publishDiagnostics") {
                    // A non-empty diagnostics array: the "range" of the first diagnostic is present.
                    assert!(
                        msg.contains("\"diagnostics\":[{"),
                        "publishDiagnostics had no diagnostics: {msg}"
                    );
                    assert!(
                        msg.contains("\"source\":\"chezzi\""),
                        "missing source: {msg}"
                    );
                    saw_diag = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        saw_diag,
        "never received a publishDiagnostics with a diagnostic"
    );
}

/// Wait (5s per read, matching this file's other loops) for a `publishDiagnostics` notification whose
/// body mentions `target_uri`, returning the full message — or `None` on the first timeout/disconnect.
fn wait_for_uri_diagnostics(rx: &mpsc::Receiver<String>, target_uri: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(msg) => {
                if msg.contains("textDocument/publishDiagnostics") && msg.contains(target_uri) {
                    return Some(msg);
                }
            }
            Err(_) => return None,
        }
    }
    None
}

/// A5 — a cross-module error resolves onto the IMPORTED module's own URI (not the edited buffer's),
/// and a later publish that no longer reports it sends an EMPTY diagnostics array clearing it.
#[test]
fn cross_module_diagnostic_publishes_to_the_imported_module_uri_and_clears_when_fixed() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();

    let dir = std::env::temp_dir().join(format!("chezzi_lsp_a5_cross_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
    // Canonicalize (matching what the resolver itself stores for an on-disk module — see
    // `resolver::canonical_or_abs`) so this string matches byte-for-byte even if the OS temp dir
    // resolves through a symlink.
    let badmod_uri = format!(
        "file://{}",
        std::fs::canonicalize(&badmod_path).unwrap().display()
    );
    let app_uri = format!("file://{}/app.chz", dir.display());

    // didOpen the entry buffer importing the broken module. `app.chz` is never written to disk — the
    // LSP type-checks the LIVE text while `core.badmod` resolves from disk as usual.
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{app_uri}","languageId":"chezzi","version":1,"text":"import core.badmod\n"}}}}}}"#
        ),
    );
    let first = wait_for_uri_diagnostics(&rx, &badmod_uri);

    // Fix badmod.chz on disk, then re-check by re-sending the (unchanged) entry text via didChange —
    // the resolver re-reads every imported module's source fresh on each check, so the fix is visible.
    std::fs::write(&badmod_path, "y: int = 5\n").unwrap();
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{app_uri}","version":2}},"contentChanges":[{{"text":"import core.badmod\n"}}]}}}}"#
        ),
    );
    let second = wait_for_uri_diagnostics(&rx, &badmod_uri);

    let _ = std::fs::remove_dir_all(&dir);

    let first_msg = first.unwrap_or_else(|| {
        panic!(
            "never received a publishDiagnostics for the imported module's own URI ({badmod_uri})"
        )
    });
    assert!(
        first_msg.contains("\"diagnostics\":[{"),
        "expected a non-empty diagnostics array for the imported module: {first_msg}"
    );

    let second_msg = second.unwrap_or_else(|| {
        panic!(
            "never received a clearing publishDiagnostics for the imported module's URI after the fix"
        )
    });
    assert!(
        second_msg.contains("\"diagnostics\":[]"),
        "expected an EMPTY diagnostics array clearing the fixed imported module: {second_msg}"
    );
}

/// F2 — two open entry buffers (A and B) importing the SAME broken module M must each keep the
/// other's squiggle alive on M's URI: `publishDiagnostics` replaces a URI's whole array, so the server
/// must merge every open source's contribution before publishing to a shared target. Fixing buffer A
/// alone (so A no longer imports M at all) must NOT clear M's diagnostic while B still legitimately
/// imports it.
#[test]
fn shared_imported_module_diagnostic_survives_until_the_last_reporting_buffer_is_fixed() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();

    let dir = std::env::temp_dir().join(format!("chezzi_lsp_f2_shared_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
    let badmod_uri = format!(
        "file://{}",
        std::fs::canonicalize(&badmod_path).unwrap().display()
    );
    let a_uri = format!("file://{}/app_a.chz", dir.display());
    let b_uri = format!("file://{}/app_b.chz", dir.display());

    // Open A, then B — both import the same broken module.
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{a_uri}","languageId":"chezzi","version":1,"text":"import core.badmod\n"}}}}}}"#
        ),
    );
    wait_for_uri_diagnostics(&rx, &badmod_uri)
        .unwrap_or_else(|| panic!("no publishDiagnostics for {badmod_uri} after opening A"));
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{b_uri}","languageId":"chezzi","version":1,"text":"import core.badmod\n"}}}}}}"#
        ),
    );
    let after_b_open = wait_for_uri_diagnostics(&rx, &badmod_uri)
        .unwrap_or_else(|| panic!("no publishDiagnostics for {badmod_uri} after opening B"));
    assert!(
        after_b_open.contains("\"diagnostics\":[{"),
        "badmod should still be non-empty once B (also importing it) opens: {after_b_open}"
    );

    // "Fix" A — not by fixing badmod.chz, but by editing A so it no longer imports it at all. A's
    // contribution to badmod's target set must drop out, while B's must remain.
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{a_uri}","version":2}},"contentChanges":[{{"text":"x := 1\n"}}]}}}}"#
        ),
    );
    // A dropping badmod from its OWN target set (old had it, new doesn't) puts badmod in `affected`,
    // so the server always republishes it here — re-merged from B alone.
    let after_a_fixed = wait_for_uri_diagnostics(&rx, &badmod_uri);

    let _ = std::fs::remove_dir_all(&dir);

    let msg = after_a_fixed.unwrap_or_else(|| {
        panic!("no publishDiagnostics for {badmod_uri} after fixing only A (B should re-trigger a union republish)")
    });
    assert!(
        msg.contains("\"diagnostics\":[{"),
        "badmod must still carry B's diagnostic after only A is fixed, not be cleared: {msg}"
    );
}

/// F3 — closing a buffer that was the SOLE source of a cross-module diagnostic must clear that
/// diagnostic (a `did_close` that only forgets the closed URI's own text orphans the imported module's
/// squiggle forever, since nothing else will ever re-check it).
#[test]
fn did_close_clears_a_cross_module_diagnostic_it_was_the_sole_source_for() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();

    let dir = std::env::temp_dir().join(format!("chezzi_lsp_f3_close_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
    let badmod_uri = format!(
        "file://{}",
        std::fs::canonicalize(&badmod_path).unwrap().display()
    );
    let app_uri = format!("file://{}/app.chz", dir.display());

    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{app_uri}","languageId":"chezzi","version":1,"text":"import core.badmod\n"}}}}}}"#
        ),
    );
    let opened = wait_for_uri_diagnostics(&rx, &badmod_uri)
        .unwrap_or_else(|| panic!("no publishDiagnostics for {badmod_uri} after didOpen"));
    assert!(
        opened.contains("\"diagnostics\":[{"),
        "expected a non-empty diagnostics array for the imported module: {opened}"
    );

    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{app_uri}"}}}}}}"#
        ),
    );
    let closed = wait_for_uri_diagnostics(&rx, &badmod_uri);

    let _ = std::fs::remove_dir_all(&dir);

    let closed_msg = closed.unwrap_or_else(|| {
        panic!(
            "never received a clearing publishDiagnostics for {badmod_uri} after did_close orphaned it"
        )
    });
    assert!(
        closed_msg.contains("\"diagnostics\":[]"),
        "expected an EMPTY diagnostics array clearing badmod once its sole source closed: {closed_msg}"
    );
}

/// F1/F2 end-to-end regression (adversarial review 2026-08-18) — a `did_change` fired immediately
/// followed (no wait in between; both notifications are written back-to-back over the same stdio pipe,
/// before any response to either) by a `did_close`, for the SOLE source of a shared diagnostic, through
/// the REAL stdio subprocess.
///
/// tower-lsp's own architecture is described as concurrent (vendored `tower-lsp-0.20.0/src/
/// transport.rs`: `DEFAULT_MAX_CONCURRENCY = 4`, `.buffer_unordered(...)`), and that description is why
/// this was charged as a race in the first place. But `Server::serve` runs `read_input`,
/// `process_server_tasks` and `print_output` as THREE FUTURES JOINED INTO ONE TASK (`join!`), not one
/// tokio task per request — measured empirically here (not guessed): a `did_change` on a payload big
/// enough to make its unlocked `chezzi::editor::diagnostics()` call take >100ms (80,000 lines; a plain
/// `chezzi check` on that shape measured ~0.5s) STILL always finished before a `did_close` sent
/// immediately after it even started, across every trial. `buffer_unordered`'s cooperative scheduling
/// only yields to a sibling future at a REAL suspension point (a full/rendezvous channel, e.g. an
/// awaited send), and this repo's handlers evidently don't hit one on this path in this build — so this
/// stdio harness cannot force the adversarial interleaving on demand.
///
/// The deterministic proof therefore lives at the unit level instead: `decide_publish`/`decide_close`
/// in `chezzi-lsp.rs` are the pure, synchronous core of the decide-then-send sequence, factored out
/// specifically so the F2 race (a `did_close` that removed `uri` from `open` before a late `publish`
/// call reaches the same lock) can be reproduced by CONSTRUCTING that exact `Published` state directly,
/// with no timing dependency — see `decide_publish_declines_a_uri_that_closed_first` and
/// `decide_close_clears_open_and_by_source` in that file's `#[cfg(test)] mod tests`.
///
/// This test stays as the next-best THING THIS HARNESS CAN pin: the ordinary (non-adversarial) case —
/// close-follows-change for the sole source of a shared diagnostic — must settle to empty end to end,
/// through the real subprocess, real transport, and real `Client::publish_diagnostics` calls.
#[test]
fn did_change_then_did_close_settles_to_the_closed_state_e2e() {
    let (mut stdin, rx, _guard, _init_resp) = start_server();

    let dir = std::env::temp_dir().join(format!("chezzi_lsp_f1f2_race_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
    let badmod_uri = format!(
        "file://{}",
        std::fs::canonicalize(&badmod_path).unwrap().display()
    );
    let app_uri = format!("file://{}/app.chz", dir.display());

    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"{app_uri}","languageId":"chezzi","version":1,"text":"import core.badmod\n"}}}}}}"#
        ),
    );
    wait_for_uri_diagnostics(&rx, &badmod_uri)
        .unwrap_or_else(|| panic!("no publishDiagnostics for {badmod_uri} after didOpen"));

    // Fire didChange (same broken content — still imports badmod) and didClose BACK TO BACK, with no
    // wait for any response in between.
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"{app_uri}","version":2}},"contentChanges":[{{"text":"import core.badmod\n"}}]}}}}"#
        ),
    );
    send(
        &mut stdin,
        &format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":"{app_uri}"}}}}}}"#
        ),
    );

    // Drain messages for badmod's URI until the burst settles (2s of silence), remembering the LAST
    // one seen — the client-observed final state must be empty.
    let mut last: Option<String> = None;
    let overall_deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if std::time::Instant::now() >= overall_deadline {
            break;
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(msg) => {
                if msg.contains("textDocument/publishDiagnostics") && msg.contains(&badmod_uri) {
                    last = Some(msg);
                }
            }
            Err(_) => break, // 2s of silence: the burst has settled.
        }
    }

    let _ = std::fs::remove_dir_all(&dir);

    let last_msg = last.unwrap_or_else(|| {
        panic!("no publishDiagnostics observed for {badmod_uri} after the didChange/didClose burst")
    });
    assert!(
        last_msg.contains("\"diagnostics\":[]"),
        "the settled state for {badmod_uri} must be EMPTY once its sole source closed: {last_msg}"
    );
}
