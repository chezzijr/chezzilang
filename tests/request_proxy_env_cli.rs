//! `std.request` ignores the proxy environment variables (W14-30d). ureq 3 defaults to
//! `Proxy::try_from_env()`, which would reroute even a loopback request through an exported dead
//! proxy; ureq 2 never read the env and Go exempts loopback, so the agent sets `.proxy(None)`.
//!
//! The env goes on a CHILD `chezzi` process: the agent is a `thread_local` built on first use, so an
//! in-process `set_var` would decide another test's agent.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;

#[test]
fn an_exported_proxy_env_var_does_not_reroute_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let resp = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 6\r\n\r\ndirect";
        let _ = stream.write_all(resp.as_bytes());
    });

    let dir = std::env::temp_dir().join(format!("chezzi_proxyenv_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prog = dir.join("p.chz");
    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    Ok(resp): print(resp.status)\n    Err(e): print(\"err\", e)\n"
    );
    std::fs::write(&prog, src).unwrap();

    let dead = "http://127.0.0.1:1";
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&prog)
        .env("ALL_PROXY", dead)
        .env("HTTP_PROXY", dead)
        .env("http_proxy", dead)
        .output()
        .expect("failed to run chezzi run");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // A routed request never reaches the server, so unblock its accept before joining.
    let _ = std::net::TcpStream::connect(addr);
    server.join().unwrap();
    assert_eq!(
        stdout.trim(),
        "200",
        "the request must go direct; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
