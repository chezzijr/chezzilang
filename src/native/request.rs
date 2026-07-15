//! `std.request` — a blocking HTTP/HTTPS client (M9), backed by `ureq` (rustls TLS, no async
//! runtime — fits the single-threaded engine). `get`/`post` return a `Result[Response]`; a non-2xx
//! status is **not** an error (it comes back as a normal `Response` carrying the status), only
//! transport/DNS/TLS failures lower to `Err`. `Response` is the synthetic struct
//! `{ status: int, body: str, headers: map[str, str] }` (header names are lowercased by `ureq`).
//!
//! `get_bytes(url, timeout_ms?)` is the binary-download sibling: it returns `Result[bytes]` (the body
//! read byte-exact via `into_reader`, no `into_string` UTF-8 decode), GET-only + body-only, and — since
//! it has no status channel — a non-2xx status becomes `Err` (a 404 error page can't pose as a
//! successful download). See `io.read_bytes`, the file twin this mirrors.
//!
//! Surface: `get(url, timeout_ms?)` / `post(url, body, timeout_ms?)`, the verb wrappers
//! `put(url, body)` / `patch(url, body)` / `delete(url)` / `head(url)`, and the general
//! `request(method, url, body, headers, timeout_ms?)` carrying a `map[str, str]` of custom request
//! headers (read in insertion order). The optional trailing `timeout_ms: int` sets a per-request
//! total deadline overriding the agent's default caps for that call (`<= 0`/omitted = defaults; a
//! timeout lowers to `Err` like any transport failure). Redirect configuration and streaming bodies
//! are still deferred.

use super::{Host, HostError, NativeFn, NativeRet, expect_args, expect_args_range};
use std::io::Read;
use std::time::Duration;

/// Cap on a `get_bytes` download. Mirrors `io::read_bytes`' `MAX_READ_FILE_BYTES` — the text path is
/// already capped (ureq's ~10MB `into_string` limit), so the binary path needs its own guard or a
/// hostile/huge download would OOM the engine.
// ponytail: 64MB cap mirrors io.read_bytes; make configurable only if a real download needs more.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

thread_local! {
    /// A process-lifetime agent with connect/read/write timeouts. The language is single-threaded
    /// with no way to abort a stuck call, so a hung peer would otherwise block the engine forever;
    /// these caps guarantee `get`/`post` eventually return (an `Err` on timeout).
    static AGENT: ureq::Agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .timeout_write(Duration::from_secs(30))
        .build();
}

/// Build the `Response` struct value from its parts.
fn response_ret(status: i64, body: String, headers: Vec<(String, String)>) -> NativeRet {
    NativeRet::Struct {
        name: "Response".into(),
        fields: vec![
            ("status".into(), NativeRet::Int(status)),
            ("body".into(), NativeRet::Str(body)),
            (
                "headers".into(),
                NativeRet::Map(
                    headers
                        .into_iter()
                        .map(|(k, v)| (NativeRet::Str(k), NativeRet::Str(v)))
                        .collect(),
                ),
            ),
        ],
    }
}

/// Read status, headers (sorted + deduped for determinism and to honor the map unique-key
/// invariant), and body out of a `ureq::Response`, then build a `Result[Response]`. Headers must be
/// read before `into_string` consumes the response. A body-read failure (truncated/aborted stream)
/// becomes `Err` rather than a misleading empty-body success.
fn lower_response(resp: ureq::Response) -> NativeRet {
    let status = resp.status() as i64;
    let mut names = resp.headers_names();
    names.sort();
    names.dedup(); // a header name could be listed more than once; one entry per key.
    let headers: Vec<(String, String)> = names
        .iter()
        .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
        .collect();
    match resp.into_string() {
        Ok(body) => NativeRet::Ok(Box::new(response_ret(status, body, headers))),
        Err(e) => NativeRet::Err(format!("failed to read response body: {e}")),
    }
}

/// Map a ureq call result to a chezzi `Result[Response]`. A `>= 400` status is a normal `Response`
/// (ureq models it as `Error::Status`); only transport-level failures (DNS/TLS/timeout/connection)
/// become `Err`.
fn lower_result(r: Result<ureq::Response, ureq::Error>) -> NativeRet {
    match r {
        Ok(resp) => lower_response(resp),
        Err(ureq::Error::Status(_, resp)) => lower_response(resp),
        Err(ureq::Error::Transport(t)) => NativeRet::Err(t.to_string()),
    }
}

/// Read a `ureq::Response`'s body as raw bytes (byte-exact, no UTF-8 decode) into a `Result[bytes]`.
/// A download exceeding `MAX_DOWNLOAD_BYTES` and a truncated/aborted read both lower to `Err` (never a
/// lying empty-body success). Called only for a 2xx response — a non-2xx status is turned into `Err`
/// by [`lower_result_bytes`] before we get here, so the caller never mistakes a 404/500 error page for
/// a successful download. Headers are dropped — a binary download is GET-only and body-only.
fn lower_response_bytes(resp: ureq::Response) -> NativeRet {
    let mut buf = Vec::new();
    match resp
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut buf)
    {
        Ok(_) if buf.len() as u64 > MAX_DOWNLOAD_BYTES => NativeRet::Err(format!(
            "download exceeds the {MAX_DOWNLOAD_BYTES}-byte limit"
        )),
        Ok(_) => NativeRet::Ok(Box::new(NativeRet::Bytes(buf))),
        Err(e) => NativeRet::Err(format!("failed to read response body: {e}")),
    }
}

/// Byte twin of [`lower_result`], but NOT status-transparent: `get_bytes` returns a bare
/// `Result[bytes]` with no status channel, so unlike the text `get` (which surfaces a `>= 400` as a
/// normal `Response` for the caller to inspect), a non-2xx status here MUST become `Err` — otherwise a
/// 404/500 HTML error page comes back as `Ok(bytes)` and a caller writes it to disk as if the download
/// succeeded. This matches `io.read_bytes` semantics (a failed read is `Err`, not empty `Ok`).
fn lower_result_bytes(r: Result<ureq::Response, ureq::Error>) -> NativeRet {
    match r {
        Ok(resp) => lower_response_bytes(resp),
        Err(ureq::Error::Status(code, resp)) => {
            NativeRet::Err(format!("HTTP {code} {}", resp.status_text()))
        }
        Err(ureq::Error::Transport(t)) => NativeRet::Err(t.to_string()),
    }
}

fn do_get(url: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| {
        let mut req = a.get(url);
        if let Some(d) = timeout {
            req = req.timeout(d);
        }
        lower_result(req.call())
    })
}

fn do_get_bytes(url: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| {
        let mut req = a.get(url);
        if let Some(d) = timeout {
            req = req.timeout(d);
        }
        lower_result_bytes(req.call())
    })
}

fn do_post(url: &str, body: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| {
        let mut req = a.post(url);
        if let Some(d) = timeout {
            req = req.timeout(d);
        }
        lower_result(req.send_string(body))
    })
}

/// The general request path shared by `request`/`put`/`patch`/`delete`/`head`: build a request for
/// `method` (UPPERCASE verb), apply each custom header via `set`, then send. An empty `body` uses
/// `.call()` (no request body — correct for `DELETE`/`HEAD`/header-only calls); a non-empty `body`
/// uses `.send_string(body)`. A `Some(timeout)` applies a per-request total deadline overriding the
/// agent's default caps for this one call (a hit lowers to `Err` like any transport failure). Lowers
/// to `Result[Response]` exactly like `get`/`post`.
fn do_request(
    method: &str,
    url: &str,
    body: &str,
    headers: &[(String, String)],
    timeout: Option<Duration>,
) -> NativeRet {
    AGENT.with(|a| {
        let mut req = a.request(method, url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        if let Some(d) = timeout {
            req = req.timeout(d);
        }
        lower_result(if body.is_empty() {
            req.call()
        } else {
            req.send_string(body)
        })
    })
}

/// Read an optional trailing `timeout_ms: int` at arg index `idx` (guarded by `arg_count`): absent
/// or `<= 0` → `None` (fall back to the agent's default caps); a positive value → `Some(Duration)`.
fn read_timeout(h: &mut dyn Host, idx: usize) -> Result<Option<Duration>, HostError> {
    if h.arg_count() > idx {
        let ms = h.arg_int(idx)?;
        if ms > 0 {
            return Ok(Some(Duration::from_millis(ms as u64)));
        }
    }
    Ok(None)
}

fn get(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args_range(h, "get", 1, 2)?;
    let url = h.arg_str(0)?;
    let timeout = read_timeout(h, 1)?;
    Ok(do_get(&url, timeout))
}

/// `get_bytes(url, timeout_ms?)` — download a body as raw `bytes` (byte-exact, no UTF-8 decode), the
/// HTTP sibling of `io.read_bytes` / `Socket.read_bytes`. Body-only: a non-2xx status is an `Err` (so
/// a 404/500 error page can't masquerade as a successful download), headers are dropped.
fn get_bytes(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args_range(h, "get_bytes", 1, 2)?;
    let url = h.arg_str(0)?;
    let timeout = read_timeout(h, 1)?;
    Ok(do_get_bytes(&url, timeout))
}

fn post(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args_range(h, "post", 2, 3)?;
    let (url, body) = (h.arg_str(0)?, h.arg_str(1)?);
    let timeout = read_timeout(h, 2)?;
    Ok(do_post(&url, &body, timeout))
}

/// `request(method, url, body, headers, timeout_ms?)` — the general verb + custom-header entry
/// point. `headers` is a `map[str, str]` read in insertion order (deterministic across engines);
/// the optional trailing `timeout_ms: int` overrides the agent default caps for this call.
fn request(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args_range(h, "request", 4, 5)?;
    let (method, url, body) = (h.arg_str(0)?, h.arg_str(1)?, h.arg_str(2)?);
    let headers = h.arg_str_map(3)?;
    let timeout = read_timeout(h, 4)?;
    Ok(do_request(&method, &url, &body, &headers, timeout))
}

fn put(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "put", 2)?;
    let (url, body) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(do_request("PUT", &url, &body, &[], None))
}

fn patch(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "patch", 2)?;
    let (url, body) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(do_request("PATCH", &url, &body, &[], None))
}

fn delete(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "delete", 1)?;
    let url = h.arg_str(0)?;
    Ok(do_request("DELETE", &url, "", &[], None))
}

fn head(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "head", 1)?;
    let url = h.arg_str(0)?;
    Ok(do_request("HEAD", &url, "", &[], None))
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("get", get),
    ("get_bytes", get_bytes),
    ("post", post),
    ("request", request),
    ("put", put),
    ("patch", patch),
    ("delete", delete),
    ("head", head),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Pull a named field out of a lowered `Response` struct `NativeRet` (test helper).
    fn field<'a>(ret: &'a NativeRet, key: &str) -> &'a NativeRet {
        let NativeRet::Ok(inner) = ret else {
            panic!("expected Ok(Response), got {ret:?}");
        };
        let NativeRet::Struct { name, fields } = inner.as_ref() else {
            panic!("expected Struct, got {inner:?}");
        };
        assert_eq!(name, "Response");
        &fields
            .iter()
            .find(|(k, _)| k == key)
            .expect("field present")
            .1
    }

    #[test]
    fn response_ret_builds_struct_with_header_map() {
        let ret = NativeRet::Ok(Box::new(response_ret(
            201,
            "hi".into(),
            vec![("x-test".into(), "yes".into())],
        )));
        assert_eq!(field(&ret, "status"), &NativeRet::Int(201));
        assert_eq!(field(&ret, "body"), &NativeRet::Str("hi".into()));
        match field(&ret, "headers") {
            NativeRet::Map(entries) => {
                assert_eq!(
                    entries[0],
                    (
                        NativeRet::Str("x-test".into()),
                        NativeRet::Str("yes".into())
                    )
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    /// Serve one canned HTTP/1.1 response on a fresh loopback port; returns the bound URL and the
    /// server thread's join handle. Deterministic and network-free.
    fn serve_once(body: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf); // drain the request line/headers
            let resp = format!(
                "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        (format!("http://{addr}/"), handle)
    }

    #[test]
    fn get_with_timeout_arg_threads_through() {
        // A generous per-call timeout is plumbed to the ureq Request (Some) and a normal 200 still
        // comes back — proves the optional Duration is threaded without breaking the happy path.
        let (url, handle) = serve_once("hello");
        let ret = do_get(&url, Some(Duration::from_secs(5)));
        handle.join().unwrap();
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
        assert_eq!(field(&ret, "body"), &NativeRet::Str("hello".into()));
    }

    #[test]
    fn get_against_local_server_parses_status_body_headers() {
        let (url, handle) = serve_once("hello");
        let ret = do_get(&url, None);
        handle.join().unwrap();

        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
        assert_eq!(field(&ret, "body"), &NativeRet::Str("hello".into()));
        match field(&ret, "headers") {
            NativeRet::Map(entries) => {
                // ureq lowercases header names.
                assert!(
                    entries
                        .iter()
                        .any(|(k, v)| *k == NativeRet::Str("x-test".into())
                            && *v == NativeRet::Str("hi".into())),
                    "x-test header missing: {entries:?}"
                );
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn truncated_body_is_err_not_fake_empty_ok() {
        // A body-read failure (here: Content-Length lies, the server closes early) must surface as
        // Err, not a lying empty 200. `into_string` returns an I/O error on the short read.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            // Promise 100 bytes, send 5, then drop the connection.
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort")
                .unwrap();
        });
        let ret = do_get(&format!("http://{addr}/"), None);
        handle.join().unwrap();
        assert!(
            matches!(ret, NativeRet::Err(_)),
            "expected Err for truncated body, got {ret:?}"
        );
    }

    #[test]
    fn transport_error_is_err() {
        // Nothing is listening on this port → a transport failure → chezzi Err.
        let ret = do_get("http://127.0.0.1:1/", None);
        assert!(
            matches!(ret, NativeRet::Err(_)),
            "expected Err, got {ret:?}"
        );
    }

    use std::sync::{Arc, Mutex};

    /// Like [`serve_once`], but RECORDS the raw bytes the server received (request line + headers)
    /// into a shared buffer so a test can assert on the method/headers the client actually sent.
    /// Returns the bound URL, the server thread handle, and the recording buffer.
    fn serve_once_recording(
        body: &'static str,
    ) -> (String, thread::JoinHandle<()>, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = Arc::new(Mutex::new(String::new()));
        let rec = Arc::clone(&recorded);
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            *rec.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).unwrap();
        });
        (format!("http://{addr}/"), handle, recorded)
    }

    #[test]
    fn put_reaches_server_with_put_request_line() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let ret = do_request("PUT", &url, "payload", &[], None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone();
        assert!(
            req.starts_with("PUT "),
            "expected PUT request line, got: {req:?}"
        );
        // The 7-byte body is announced via Content-Length (the body bytes themselves may arrive in a
        // later TCP segment than the headers, so assert on the header rather than the captured body).
        assert!(
            req.contains("Content-Length: 7"),
            "PUT body should be sent: {req:?}"
        );
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    #[test]
    fn delete_reaches_server_with_delete_request_line_and_no_body() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let ret = do_request("DELETE", &url, "", &[], None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone();
        assert!(
            req.starts_with("DELETE "),
            "expected DELETE request line, got: {req:?}"
        );
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    /// Byte-slice twin of [`serve_once`]: serves a raw (possibly non-UTF-8) body with an exact
    /// Content-Length, so a test can assert a byte-for-byte binary round-trip.
    fn serve_once_bytes(body: &'static [u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });
        (format!("http://{addr}/"), handle)
    }

    // A non-UTF-8 payload (0xff/0xfe/0x00 + a PNG-ish tail) — `from_utf8_lossy` mangles it.
    const BINARY_PAYLOAD: &[u8] = b"\xff\xfe\x00PNG\x89";

    #[test]
    fn get_bytes_returns_body_byte_exact() {
        let (url, handle) = serve_once_bytes(BINARY_PAYLOAD);
        let ret = do_get_bytes(&url, None);
        handle.join().unwrap();
        assert_eq!(
            ret,
            NativeRet::Ok(Box::new(NativeRet::Bytes(BINARY_PAYLOAD.to_vec())))
        );
    }

    #[test]
    fn into_string_corrupts_but_get_bytes_is_exact() {
        // TEXT path: the body comes back as a str that lost the non-UTF-8 bytes to U+FFFD.
        let (url, handle) = serve_once_bytes(BINARY_PAYLOAD);
        let text = do_get(&url, None);
        handle.join().unwrap();
        let NativeRet::Str(s) = field(&text, "body") else {
            panic!("expected Str body");
        };
        assert_ne!(
            s.as_bytes(),
            BINARY_PAYLOAD,
            "into_string was expected to corrupt the non-UTF-8 body"
        );

        // BYTES path: exact.
        let (url2, handle2) = serve_once_bytes(BINARY_PAYLOAD);
        let bytes = do_get_bytes(&url2, None);
        handle2.join().unwrap();
        assert_eq!(
            bytes,
            NativeRet::Ok(Box::new(NativeRet::Bytes(BINARY_PAYLOAD.to_vec())))
        );
    }

    #[test]
    fn get_bytes_truncated_body_is_err() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort")
                .unwrap();
        });
        let ret = do_get_bytes(&format!("http://{addr}/"), None);
        handle.join().unwrap();
        assert!(
            matches!(ret, NativeRet::Err(_)),
            "expected Err for truncated body, got {ret:?}"
        );
    }

    #[test]
    fn get_bytes_non_2xx_status_is_err() {
        // A 404 with an HTML error-page body must NOT come back as Ok(bytes) — get_bytes has no
        // status channel, so the failure has to surface as Err or a caller writes the error page to
        // disk thinking the download succeeded.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = b"<html>not found</html>";
            let head = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(body).unwrap();
        });
        let ret = do_get_bytes(&format!("http://{addr}/"), None);
        handle.join().unwrap();
        assert!(
            matches!(ret, NativeRet::Err(_)),
            "expected Err for a 404, got {ret:?}"
        );
    }

    #[test]
    fn custom_header_is_sent_via_request_helper() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let headers = vec![("X-Custom".to_string(), "value".to_string())];
        let ret = do_request("POST", &url, "", &headers, None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone();
        assert!(
            req.contains("X-Custom: value"),
            "custom header missing: {req:?}"
        );
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }
}
