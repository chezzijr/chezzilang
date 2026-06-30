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
