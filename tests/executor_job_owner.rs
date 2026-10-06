//! TICKET-219 (wave 21 family E2): an Executor job has one owner. After a job calls `os.exit`,
//! a held job (Executor(1), second job queued) must never start. Ancestors: CPython
//! `ThreadPoolExecutor(1)` + `os._exit`, Go semaphore + `os.Exit` -- nothing runs after the exit.

use std::process::Command;

const PROG: &str = r#"import std.os
import std.time
import std.concurrency
import std.concurrency.task
fn job(i: int) -> int:
    if i == 1:
        time.sleep_ms(100)
        os.exit(17)
    print("job {i} ran")
    return i
fn main():
    ex := Executor(1)
    h1 := task.submit_task(ex, fn() -> int: job(1))
    h2 := task.submit_task(ex, fn() -> int: job(2))
    print("h2 {h2.get()}")
main()
"#;

#[test]
fn held_job_never_starts_after_exit() {
    let dir = std::env::temp_dir().join(format!("chezzi_t219_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("main.chz");
    std::fs::write(&f, PROG).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&f)
        .output()
        .expect("spawn chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(17));
    assert_eq!(stdout, "", "nothing may run or print after os.exit");
}
