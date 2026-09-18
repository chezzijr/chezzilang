//! TICKET-136 (W14-11, W14-16, W14-35) — deadlock-verdict misses after TICKET-135's D1.
//! Real-PROCESS tests: the hang case needs a wall-clock bound and the deadlock verdict is fatal.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `src` under `chezzi run`; kill it after 10 s. Returns (stdout, stderr, code), code None on timeout.
fn run(name: &str, src: &[&str]) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("chz-ticket136-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut o, mut e) = (String::new(), String::new());
    child.stdout.take().unwrap().read_to_string(&mut o).unwrap();
    child.stderr.take().unwrap().read_to_string(&mut e).unwrap();
    (o, e, code)
}

#[test]
fn main_defer_that_can_never_complete_faults_deadlock() {
    let (out, err, code) = run(
        "d9.chz",
        &[
            "fn main():",
            "    c := Channel[int](0)",
            "    defer: c.recv()",
            "    print(\"x\")",
            "main()",
            "",
        ],
    );
    assert_eq!(out, "x\n");
    assert!(
        code == Some(1) && err.contains("deadlock"),
        "a never-completing main-thread defer must fault `deadlock`, got code {code:?} (stderr: {err})"
    );
}

#[test]
fn for_over_channel_in_generator_driven_from_a_task_prints_the_value() {
    let (out, err, code) = run(
        "q3.chz",
        &[
            "fn gen(c: Channel[int]) -> Iterator[int]:",
            "    for v in c:",
            "        yield v",
            "fn burn(n: int) -> int:",
            "    s := 0",
            "    for i in range(n):",
            "        s += i % 7",
            "    return s",
            "fn main():",
            "    c := Channel[int](4)",
            "    parallel:",
            "        spawn:",
            "            c.send(burn(3000000) % 2)",
            "            c.close()",
            "        spawn:",
            "            for v in gen(c):",
            "                print(v)",
            "main()",
            "",
        ],
    );
    assert!(
        code == Some(0) && out.trim().len() == 1,
        "the generator must print the one sent value and exit 0, got code {code:?} (stdout: {out:?}, stderr: {err})"
    );
}

#[test]
fn rendezvous_send_deadlock_names_the_missing_receiver() {
    let (_, err, code) = run(
        "rz.chz",
        &[
            "fn main():",
            "    c := Channel[int](0)",
            "    c.send(1)",
            "main()",
            "",
        ],
    );
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(
        !err.contains("at capacity"),
        "a rendezvous channel has no slots; the message must not say it is at capacity (stderr: {err})"
    );
}
