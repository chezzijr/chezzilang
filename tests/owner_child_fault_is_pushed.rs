//! TICKET-188 (W17 Family B) — a child task's fault must reach its blocked owner at every wait,
//! the way cancellation does. Go: `panic: boom` at ~50 ms. Before the fix a socket read hangs
//! forever (F2) and a spawned-task owner sleeps through its child's fault and keeps running (F3).

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `src` under `chezzi run`, killing it after `limit`. Returns (stdout, stderr, timed_out).
fn run_bounded(name: &str, src: &[&str], limit: Duration) -> (String, String, bool) {
    let dir = std::env::temp_dir().join(format!("chz-ticket188-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env_remove("CHEZZI_THREADS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    let mut timed_out = false;
    while child.try_wait().expect("try_wait").is_none() {
        if start.elapsed() > limit {
            timed_out = true;
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (mut out, mut err) = (String::new(), String::new());
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (out, err, timed_out)
}

#[test]
fn child_fault_cuts_owner_blocked_in_socket_read() {
    let src = [
        "import std.net",
        "import std.time",
        "fn must[T](r: Result[T]) -> T:",
        "    match r:",
        "        Ok(v): return v",
        "        Err(e): panic(e.message())",
        "ln := must(net.listen(\"127.0.0.1:0\"))",
        "c := must(net.connect(must(ln.addr())))",
        "peer := must(ln.accept())",
        "parallel:",
        "    spawn:",
        "        time.sleep_ms(50)",
        "        panic(\"boom\")",
        "    print(peer.read(10))",
        "",
    ];
    let (out, err, timed_out) = run_bounded("f2.chz", &src, Duration::from_secs(5));
    assert!(
        !timed_out && err.contains("boom"),
        "owner blocked in socket read never saw its child's fault: timed_out={timed_out} stdout={out:?} stderr={err:?}"
    );
}

#[test]
fn child_fault_cuts_spawned_owner_in_sleep() {
    let src = [
        "import std.time",
        "fn owner():",
        "    spawn:",
        "        time.sleep_ms(50)",
        "        panic(\"boom\")",
        "    time.sleep_ms(3000)",
        "    print(\"owner continued after child fault\")",
        "parallel:",
        "    spawn: owner()",
        "",
    ];
    let (out, err, timed_out) = run_bounded("f3.chz", &src, Duration::from_secs(10));
    assert!(
        !out.contains("owner continued after child fault"),
        "spawned owner kept running after its child faulted: stdout={out:?} stderr={err:?} timed_out={timed_out}"
    );
}
