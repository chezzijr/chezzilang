//! `std.request` — a blocking HTTP/HTTPS client (M9), backed by `ureq` 3 (rustls TLS, no async
//! runtime — fits the single-threaded engine). `get`/`post` return a `Result[Response]`; a non-2xx
//! status is **not** an error (it comes back as a normal `Response` carrying the status), only
//! transport/DNS/TLS failures lower to `Err`. `Response` is the synthetic struct
//! `{ status: int, body: str, headers: map[str, str] }` (header names are lowercased by the `http`
//! crate; each value is read as raw bytes and decoded latin-1, byte -> code point, which never
//! fails — a UTF-8 `café` reads back `cafÃ©`, as CPython's `urllib` does, W14-30b).
//!
//! `get_bytes(url, timeout_ms?)` is the binary-download sibling: it returns `Result[bytes]` (the body
//! read byte-exact via `into_reader`, no UTF-8 decode), GET-only + body-only, and — since
//! it has no status channel — a non-2xx status becomes `Err` (a 404 error page can't pose as a
//! successful download). See `io.read_bytes`, the file twin this mirrors.
//!
//! Surface: `get(url, timeout_ms?)` / `post(url, body, timeout_ms?)`, the verb wrappers
//! `put(url, body)` / `patch(url, body)` / `delete(url)` / `head(url)`, and the general
//! `request(method, url, body, headers, timeout_ms?)` carrying a `map[str, str]` of custom request
//! headers (read in insertion order). The optional trailing `timeout_ms: int` sets a per-request
//! total deadline overriding the agent's default caps for that call (`<= 0`/omitted = defaults; a
//! timeout lowers to `Err` like any transport failure). Streaming bodies are still deferred.
//!
//! Redirects are followed up to ten hops (ureq 3's default; CPython and Go both cap at ten) and the
//! eleventh is `Err("... too many redirects")`. Measured wire/message changes the ureq 2 -> 3 move
//! brought: custom REQUEST header names go out lowercased (the `http` crate normalizes every
//! `HeaderName`; RFC 9110 field names are case-insensitive); the `HTTP_PROXY`/`ALL_PROXY` family is
//! still ignored (`.proxy(None)`, W14-30d); `get_bytes`' non-2xx `Err` names the canonical reason,
//! not the server's wire phrase (`http::StatusCode` drops it); and the parser now REJECTS a control
//! byte or NUL in a header value, an `HTTP/1.2` status line and an obs-fold continuation, which
//! ureq 2 accepted with the header dropped (W14-30c).

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args, expect_args_range};
use std::io::Read;
use std::time::Duration;
use ureq::http::{Request, Response};
use ureq::{Agent, AsSendBody, Body};

/// Cap on a `get_bytes` download. Mirrors `io::read_bytes`' `MAX_READ_FILE_BYTES` — the text path is
/// already capped (`MAX_TEXT_BYTES`), so the binary path needs its own guard or a
/// hostile/huge download would OOM the engine.
// ponytail: 64MB cap mirrors io.read_bytes; make configurable only if a real download needs more.
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;

/// Cap on the text path's body (ureq 2's `into_string` limit, kept so the limit does not move).
const MAX_TEXT_BYTES: u64 = 10 * 1024 * 1024;

thread_local! {
    /// A process-lifetime agent with connect/send/response-head timeouts. The language is
    /// single-threaded with no way to abort a stuck call, so a hung peer would otherwise block the
    /// engine forever; these caps guarantee `get`/`post` eventually return (an `Err` on timeout).
    /// Three settings are NOT ureq 3's defaults and are load-bearing:
    /// - `http_status_as_error(false)`: ureq 3 otherwise turns a `>= 400` into `Error::StatusCode`
    ///   and drops the response, but a `>= 400` is a normal `Response` here.
    /// - `allow_non_standard_methods(true)`: `request("FOO", ...)` is refused otherwise.
    /// - `proxy(None)`: ureq 3 defaults to `Proxy::try_from_env()`; ureq 2 never read the env, Go
    ///   exempts loopback and CPython does not, so honouring it would reroute loopback requests
    ///   (W14-30d).
    ///
    /// The body-phase timeouts stay UNSET: ureq 2's read/write timeouts reset on every socket op, but
    /// ureq 3's are whole-phase deadlines that would kill a slow 64MB download. `max_redirects` is
    /// deliberately left at ureq 3's default of ten, which is what both CPython and Go cap at.
    static AGENT: Agent = Agent::config_builder()
        .http_status_as_error(false)
        .allow_non_standard_methods(true)
        .proxy(None)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build()
        .new_agent();
}

/// Decode header-value bytes latin-1 (byte -> code point). RFC 9110 makes a field value opaque
/// bytes; this never fails, unlike a UTF-8 decode, so no header is ever dropped.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// Build and run one request. `timeout` is a per-request total deadline over the agent's caps.
fn send<T: AsSendBody>(
    agent: &Agent,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    timeout: Option<Duration>,
    body: T,
) -> Result<Response<Body>, ureq::Error> {
    let mut builder = Request::builder().method(method).uri(url);
    for (k, v) in headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let req = builder.body(body).map_err(ureq::Error::Http)?;
    let req = match timeout {
        Some(d) => agent.configure_request(req).timeout_global(Some(d)).build(),
        None => req,
    };
    agent.run(req)
}

/// ureq 3's `Display` drops the URL that ureq 2 printed, so put the `"<url>: "` prefix back.
fn transport_msg(url: &str, e: &ureq::Error) -> String {
    format!("{url}: {e}")
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
/// invariant; a header sent more than once is joined with `, `, as Python `requests` does, W14-30;
/// each value is decoded latin-1, W14-30b), and body out of a `Response<Body>`, then build a
/// `Result[Response]`. Headers must be read before `into_body` consumes the response. A body-read
/// failure (truncated/aborted stream) becomes `Err` rather than a misleading empty-body success.
fn lower_response(resp: Response<Body>) -> NativeRet {
    let status = resp.status().as_u16() as i64;
    let mut names: Vec<String> = resp
        .headers()
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    names.sort();
    names.dedup(); // `keys()` is already unique; kept so the map invariant does not rest on it.
    let headers: Vec<(String, String)> = names
        .into_iter()
        .map(|n| {
            let joined = resp
                .headers()
                .get_all(n.as_str())
                .iter()
                .map(|v| latin1(v.as_bytes()))
                .collect::<Vec<_>>()
                .join(", ");
            (n, joined)
        })
        .collect();
    let mut buf = Vec::new();
    match resp
        .into_body()
        .into_reader()
        .take(MAX_TEXT_BYTES + 1)
        .read_to_end(&mut buf)
    {
        Ok(_) if buf.len() as u64 > MAX_TEXT_BYTES => {
            NativeRet::Err("failed to read response body: response too big for into_string".into())
        }
        Ok(_) => {
            let body = String::from_utf8_lossy(&buf).into_owned();
            NativeRet::Ok(Box::new(response_ret(status, body, headers)))
        }
        Err(e) => NativeRet::Err(format!("failed to read response body: {e}")),
    }
}

/// Map a ureq call result to a chezzi `Result[Response]`. A `>= 400` status is a normal `Response`
/// (the agent sets `http_status_as_error(false)`); only transport-level failures
/// (DNS/TLS/timeout/connection/parse) become `Err`.
fn lower_result(url: &str, r: Result<Response<Body>, ureq::Error>) -> NativeRet {
    match r {
        Ok(resp) => lower_response(resp),
        Err(e) => NativeRet::Err(transport_msg(url, &e)),
    }
}

/// Read a `Response<Body>`'s body as raw bytes (byte-exact, no UTF-8 decode) into a `Result[bytes]`.
/// A download exceeding `MAX_DOWNLOAD_BYTES` lowers to `Err`; a read that errors mid-stream (e.g. a
/// `Content-Length`/chunked body that ends short) also lowers to `Err` rather than a lying empty-body
/// success. Ceiling: a `Connection: close`-delimited body has no promised length, so a premature peer
/// close is indistinguishable from a clean end — that returns `Ok(partial)` (no HTTP client can detect
/// it). Called only for a non-`>= 400` response — that status is turned into `Err`
/// by [`lower_result_bytes`] before we get here, so the caller never mistakes a 404/500 error page for
/// a successful download. Headers are dropped — a binary download is GET-only and body-only.
fn lower_response_bytes(resp: Response<Body>) -> NativeRet {
    let mut buf = Vec::new();
    match resp
        .into_body()
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
/// normal `Response` for the caller to inspect), a `>= 400` status here MUST become `Err` — otherwise a
/// 404/500 HTML error page comes back as `Ok(bytes)` and a caller writes it to disk as if the download
/// succeeded. This matches `io.read_bytes` semantics (a failed read is `Err`, not empty `Ok`). The
/// threshold is `>= 400`, not `!is_success()`, so a body-less 304 still answers `Ok`.
fn lower_result_bytes(url: &str, r: Result<Response<Body>, ureq::Error>) -> NativeRet {
    match r {
        Ok(resp) if resp.status().as_u16() >= 400 => NativeRet::Err(format!(
            "HTTP {} {}",
            resp.status().as_u16(),
            resp.status().canonical_reason().unwrap_or("")
        )),
        Ok(resp) => lower_response_bytes(resp),
        Err(e) => NativeRet::Err(transport_msg(url, &e)),
    }
}

fn do_get(url: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| lower_result(url, send(a, "GET", url, &[], timeout, ())))
}

fn do_get_bytes(url: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| lower_result_bytes(url, send(a, "GET", url, &[], timeout, ())))
}

fn do_post(url: &str, body: &str, timeout: Option<Duration>) -> NativeRet {
    AGENT.with(|a| lower_result(url, send(a, "POST", url, &[], timeout, body)))
}

/// Whether ureq 3 frames an empty body on this verb. A `()` body on these puts
/// `transfer-encoding: chunked` on the wire where ureq 2 sent nothing, so they take `""` instead.
fn takes_body(method: &str) -> bool {
    ["POST", "PUT", "PATCH"]
        .iter()
        .any(|m| method.eq_ignore_ascii_case(m))
}

/// The general request path shared by `request`/`put`/`patch`/`delete`/`head`: build a request for
/// `method` (UPPERCASE verb), apply each custom header, then send. A non-empty `body` is sent as is;
/// an empty `body` on POST/PUT/PATCH sends `""` (framed `content-length: 0`, never chunked) and on any
/// other verb sends no body at all (no framing — correct for `DELETE`/`HEAD`/header-only calls). A
/// `Some(timeout)` applies a per-request total deadline overriding the agent's default caps for this
/// one call (a hit lowers to `Err` like any transport failure). Lowers to `Result[Response]` exactly
/// like `get`/`post`.
fn do_request(
    method: &str,
    url: &str,
    body: &str,
    headers: &[(String, String)],
    timeout: Option<Duration>,
) -> NativeRet {
    AGENT.with(|a| {
        let r = if !body.is_empty() || takes_body(method) {
            send(a, method, url, headers, timeout, body)
        } else {
            send(a, method, url, headers, timeout, ())
        };
        lower_result(url, r)
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

/// Callable members. `(name, fn, kind)`.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("get", get, Kind::Blocking),
    ("get_bytes", get_bytes, Kind::Blocking),
    ("post", post, Kind::Blocking),
    ("request", request, Kind::Blocking),
    ("put", put, Kind::Blocking),
    ("patch", patch, Kind::Blocking),
    ("delete", delete, Kind::Blocking),
    ("head", head, Kind::Blocking),
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
        // Lowercase: the `http` crate lowercases every `HeaderName` and RFC 9110 makes field names
        // case-insensitive, so ureq 3 sends `content-length`, not ureq 2's `Content-Length`.
        assert!(
            req.contains("content-length: 7"),
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
        // Lowercased on the wire: the `http` crate normalizes every `HeaderName` (RFC 9110 field
        // names are case-insensitive); ureq 3 has no knob that carries the original case.
        assert!(
            req.contains("x-custom: value"),
            "custom header missing: {req:?}"
        );
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    /// Serve the caller's exact bytes as the whole response (no framing added), so a test can send a
    /// malformed or non-ASCII head. Returns the bound URL and the server thread's join handle.
    fn serve_raw(raw: &'static [u8]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(raw);
        });
        (format!("http://{addr}/"), handle)
    }

    /// The `(name, value)` pairs of a lowered `Response`'s `headers` map, in map order.
    fn header_pairs(ret: &NativeRet) -> Vec<(String, String)> {
        let NativeRet::Map(entries) = field(ret, "headers") else {
            panic!("expected Map headers");
        };
        entries
            .iter()
            .map(|(k, v)| match (k, v) {
                (NativeRet::Str(k), NativeRet::Str(v)) => (k.clone(), v.clone()),
                other => panic!("expected Str pair, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn a_latin1_header_value_reads_back_as_its_code_points() {
        // `X-Cafe: caf` + the single byte 0xe9: ureq 2 dropped the key, ureq 3 keeps the raw byte
        // and the latin-1 decode turns 0xe9 into U+00E9.
        let (url, handle) = serve_raw(
            b"HTTP/1.1 200 OK\r\nX-Cafe: caf\xe9\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        let ret = do_get(&url, None);
        handle.join().unwrap();
        assert!(
            header_pairs(&ret).contains(&("x-cafe".into(), "café".into())),
            "x-cafe missing or misdecoded: {:?}",
            header_pairs(&ret)
        );
    }

    #[test]
    fn a_utf8_header_value_reads_back_latin1_decoded() {
        // UTF-8 `café` is the bytes 63 61 66 c3 a9; latin-1 reads c3 a9 as `Ã©`, as CPython does.
        let (url, handle) = serve_raw(
            "HTTP/1.1 200 OK\r\nX-Cafe: café\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                .as_bytes(),
        );
        let ret = do_get(&url, None);
        handle.join().unwrap();
        assert!(
            header_pairs(&ret).contains(&("x-cafe".into(), "cafÃ©".into())),
            "x-cafe missing or misdecoded: {:?}",
            header_pairs(&ret)
        );
    }

    #[test]
    fn a_control_byte_header_value_is_an_error_not_a_silent_drop() {
        // ureq 2 answered Ok with the header dropped; ureq 3's parser rejects the response (W14-30c).
        let (url, handle) = serve_raw(
            b"HTTP/1.1 200 OK\r\nX-Bad: a\x01b\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        let ret = do_get(&url, None);
        handle.join().unwrap();
        match ret {
            NativeRet::Err(m) => assert!(m.contains("invalid header value"), "message: {m}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn an_http_1_2_status_line_is_an_error() {
        let (url, handle) =
            serve_raw(b"HTTP/1.2 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        let ret = do_get(&url, None);
        handle.join().unwrap();
        match ret {
            NativeRet::Err(m) => assert!(m.contains("invalid HTTP version"), "message: {m}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_post_body_is_framed_content_length_zero_not_chunked() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let ret = do_request("POST", &url, "", &[], None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone().to_ascii_lowercase();
        assert!(req.contains("content-length: 0"), "wire: {req:?}");
        assert!(!req.contains("chunked"), "wire: {req:?}");
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    #[test]
    fn an_empty_delete_body_sends_no_framing() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let ret = do_request("DELETE", &url, "", &[], None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone().to_ascii_lowercase();
        assert!(!req.contains("content-length"), "wire: {req:?}");
        assert!(!req.contains("transfer-encoding"), "wire: {req:?}");
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    #[test]
    fn a_non_standard_method_still_reaches_the_wire() {
        let (url, handle, recorded) = serve_once_recording("ok");
        let ret = do_request("FOO", &url, "", &[], None);
        handle.join().unwrap();
        let req = recorded.lock().unwrap().clone();
        assert!(req.starts_with("FOO "), "wire: {req:?}");
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
    }

    #[test]
    fn a_400_status_is_a_normal_response_not_an_error() {
        let (url, handle) = serve_raw(
            b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 2\r\n\r\nno",
        );
        let ret = do_get(&url, None);
        handle.join().unwrap();
        assert_eq!(field(&ret, "status"), &NativeRet::Int(404));
        assert_eq!(field(&ret, "body"), &NativeRet::Str("no".into()));
    }

    #[test]
    fn a_non_2xx_get_bytes_error_names_the_canonical_reason() {
        // `http::StatusCode` drops the wire phrase (`Nope`), so the message carries the canonical one.
        let (url, handle) =
            serve_raw(b"HTTP/1.1 404 Nope\r\nConnection: close\r\nContent-Length: 2\r\n\r\nno");
        let ret = do_get_bytes(&url, None);
        handle.join().unwrap();
        assert_eq!(ret, NativeRet::Err("HTTP 404 Not Found".into()));
    }

    /// Serve a redirect chain: the first `hops` accepted sockets get a `302` to `/hop<i>`, the next
    /// one a `200` with body `done`. Non-blocking accept so the thread still ends when the client
    /// gives up early (over-cap test): it leaves after `hops + 1` sockets, 500 ms with no new socket
    /// once the first arrived, or a 5 s deadline.
    fn serve_redirect_chain(hops: usize) -> (String, thread::JoinHandle<()>) {
        use std::time::Instant;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut served = 0usize;
            let mut last = None::<Instant>;
            while served <= hops && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        let resp = if served < hops {
                            format!(
                                "HTTP/1.1 302 Found\r\nLocation: /hop{served}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            )
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone"
                                .to_string()
                        };
                        let _ = stream.write_all(resp.as_bytes());
                        served += 1;
                        last = Some(Instant::now());
                    }
                    Err(_) => {
                        if last.is_some_and(|t| t.elapsed() > Duration::from_millis(500)) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        });
        (format!("http://{addr}/"), handle)
    }

    #[test]
    fn a_seven_hop_redirect_chain_is_followed_like_cpython_and_go() {
        let (url, handle) = serve_redirect_chain(7);
        let ret = do_get(&url, None);
        handle.join().unwrap();
        assert_eq!(field(&ret, "status"), &NativeRet::Int(200));
        assert_eq!(field(&ret, "body"), &NativeRet::Str("done".into()));
    }

    #[test]
    fn a_twelve_hop_redirect_chain_stops_with_too_many_redirects() {
        let (url, handle) = serve_redirect_chain(12);
        let ret = do_get(&url, None);
        handle.join().unwrap();
        match ret {
            NativeRet::Err(m) => assert!(m.contains("too many redirects"), "message: {m}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }
}
