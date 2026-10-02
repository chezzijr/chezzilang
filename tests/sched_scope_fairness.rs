//! TICKET-199 (family S1): scheduler scope identity and fairness.
//! H1: a fiber-owned nursery's scope id is popped/reused while a worker still holds it
//! (`cancel_drain` after the owner joined) -> index out of bounds / wrong nursery drained.
//! H2: at `CHEZZI_THREADS=1` a cap-0 ping-pong keeps refilling `runnext`, so the ring never runs.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(src: &str, threads: &str, tag: &str) -> Option<(String, String, Option<i32>)> {
    let dir = std::env::temp_dir().join(format!("chz-t199-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.chz");
    std::fs::write(&path, src).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            return Some((
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                out.status.code(),
            ));
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const PINGPONG_STARVES_INNER_SIBLING: &str = r#"import std.concurrency
fn main():
    c := Channel[int](0)
    stop := AtomicInt(0)
    parallel:
        spawn:
            while stop.load() == 0:
                c.send(1)
            c.close()
        spawn:
            parallel:
                spawn:
                    for _v in c:
                        pass
                spawn:
                    print("inner sibling ran")
                    stop.store(1)
    print("done")
main()
"#;

#[test]
fn runnext_pingpong_does_not_starve_the_ring_at_threads_1() {
    let r = run(PINGPONG_STARVES_INNER_SIBLING, "1", "h2");
    let (out, err, _) = r.expect("hang: no exit within 20s (inner sibling starved)");
    assert!(
        out.contains("inner sibling ran") && out.contains("done"),
        "stdout={out:?} stderr={err:?}"
    );
}

const NURSERY_IN_SPAWN_CHILD_FAULT: &str = r#"fn run():
    c := Channel[int](0)
    parallel:
        spawn:
            _r := recover:
                parallel:
                    spawn:
                        while true:
                            c.send(1)
                    spawn: panic("boom")
            c.close()
        spawn:
            for _v in c:
                pass
fn main():
    for _k in range(0, 50):
        run()
    print("done")
main()
"#;

#[test]
fn fiber_owned_nursery_scope_id_is_stable_under_child_fault_at_threads_4() {
    for round in 0..10 {
        let r = run(NURSERY_IN_SPAWN_CHILD_FAULT, "4", "h1");
        let (out, err, code) = r.expect("hang: no exit within 20s");
        assert!(
            out == "done\n" && code == Some(0),
            "round {round}: code={code:?} stdout={out:?} stderr={err:?}"
        );
    }
}

const TWO_DEEP_CHILD_FAULT: &str = r#"import std.concurrency
fn core(c: Channel[int], started: Channel[int]):
    parallel:
        spawn:
            while true:
                c.send(1)
        spawn:
            panic("boom")
fn outer(c: Channel[int], started: Channel[int]):
    parallel:
        spawn:
            parallel:
                spawn:
                    core(c, started)
        spawn:
            for _v in c:
                pass
fn run():
    c := Channel[int](0)
    started := Channel[int](1)
    _r := recover: outer(c, started)
fn main():
    for _k in range(0, 400):
        run()
    print("done")
main()
"#;

#[test]
fn fiber_owned_nursery_scope_id_is_stable_two_deep_child_fault_at_default_threads() {
    for round in 0..20 {
        let r = run(TWO_DEEP_CHILD_FAULT, "0", "h1d");
        let (out, err, code) = r.expect("hang: no exit within 20s");
        assert!(
            out == "done\n" && code == Some(0) && !err.contains("panicked at"),
            "round {round}: code={code:?} stdout={out:?} stderr={err:?}"
        );
    }
}
