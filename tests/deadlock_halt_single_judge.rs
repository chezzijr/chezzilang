//! TICKET-223 (W21 E2 fact 3, CHAN4) — a deadlock verdict is one run halt. Main locks a `Shared`
//! guard that a job holds while it waits on a channel nobody sends: the run must end at main's
//! site, never print past it. Before the fix a sched worker may judge first, fault the job, and
//! let main run on (3 of 20 runs at T=1).

use std::process::Command;

const SRC: &str = r#"import std.concurrency
import std.time
fn main():
    s := Shared(0)
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): s.update(fn(x: int) -> int: x + c.recv()))
    time.sleep_ms(50)
    s.update(fn(x: int) -> int: x + 1)
    print("main: past the guard, s={s.get()}")
    time.sleep_ms(100)
    print("main: still running")
    ex.shutdown()
main()
"#;

#[test]
fn deadlock_on_a_held_guard_ends_the_run_at_the_first_party_site() {
    let dir = std::env::temp_dir().join(format!("chz-ticket223-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("G.chz");
    std::fs::write(&path, SRC).expect("write fixture");
    let mut bad = Vec::new();
    for run in 0..500 {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", "1")
            .output()
            .expect("run chezzi");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.contains("main: past the guard") || !stderr.contains("G.chz:9:5") {
            bad.push(format!("run {run}: stdout {stdout:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "deadlock verdict did not end the run at main's site in {} of 500 runs; first: {}",
        bad.len(),
        bad[0]
    );
}
