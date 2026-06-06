//! `std.request` — a blocking HTTP/HTTPS client (M9), backed by `ureq` (rustls TLS, no async
//! runtime — fits the single-threaded engine). `get`/`post` return a `Result[Response]`; a non-2xx
//! status is **not** an error (it comes back as a normal `Response` carrying the status), only
//! transport/DNS/TLS failures lower to `Err`. `Response` is the synthetic struct
//! `{ status: int, body: str, headers: map[str, str] }` (header names are lowercased by `ureq`).
//!
//! v1 surface is `get(url)` / `post(url, body)`. Custom request headers, other verbs, redirect
//! configuration, and streaming bodies are deferred.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};
use std::time::Duration;

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

fn do_get(url: &str) -> NativeRet {
    AGENT.with(|a| lower_result(a.get(url).call()))
}

fn do_post(url: &str, body: &str) -> NativeRet {
    AGENT.with(|a| lower_result(a.post(url).send_string(body)))
}

fn get(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "get", 1)?;
    let url = h.arg_str(0)?;
    Ok(do_get(&url))
}

fn post(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "post", 2)?;
    let (url, body) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(do_post(&url, &body))
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[("get", get), ("post", post)];

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
        &fields.iter().find(|(k, _)| k == key).expect("field present").1
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
                    (NativeRet::Str("x-test".into()), NativeRet::Str("yes".into()))
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
    fn get_against_local_server_parses_status_body_headers() {
        let (url, handle) = serve_once("hello");
        let ret = do_get(&url);
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
        let ret = do_get(&format!("http://{addr}/"));
        handle.join().unwrap();
        assert!(matches!(ret, NativeRet::Err(_)), "expected Err for truncated body, got {ret:?}");
    }

    #[test]
    fn transport_error_is_err() {
        // Nothing is listening on this port → a transport failure → chezzi Err.
        let ret = do_get("http://127.0.0.1:1/");
        assert!(matches!(ret, NativeRet::Err(_)), "expected Err, got {ret:?}");
    }
}
