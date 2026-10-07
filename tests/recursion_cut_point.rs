//! TICKET-224 cell 2 — a loop-free recursion has no cut point, so `os.exit` waits for it to finish.
//! Real-PROCESS test: the exit fires at 100 ms; the recursion alone runs far longer. Judged on
//! stdout alone, with no wall clock (TICKET-050): the recursion prints only if the exit waited for it.

use std::process::Command;

const SRC: &str = "import std.time
import std.os
import std.concurrency
fn fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)
fn quit():
    time.sleep_ms(100)
    os.exit(7)
ex := Executor()
ex.submit(quit)
print(fib(36))
";

#[test]
fn exit_lands_inside_a_loop_free_recursion() {
    let path = std::env::temp_dir().join(format!("t224_rec_{}.chz", std::process::id()));
    std::fs::write(&path, SRC).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--threads=2"])
        .arg(&path)
        .output()
        .expect("spawn chezzi");
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(stdout, "", "nothing prints after os.exit");
    assert_eq!(
        out.status.code(),
        Some(7),
        "the run exits with the os.exit code"
    );
}

/// TICKET-224 — the run-wide halt grid: a CPU-bound party in five shapes x five run-wide events x
/// T {1, 2}. Each party prints only after its CPU work returns, so a halt that waited for the work
/// shows on stdout. The halt fires after a channel handshake (DEC-050), and no clock is read.
const GRID_HDR: &str = r#"import std.os
import std.concurrency
started := Channel[int](1)
fn fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)
fn ev(n: int) -> int:
    if n < 2:
        return n
    return od(n - 1) + od(n - 2)
fn od(n: int) -> int:
    if n < 2:
        return 1 - n
    return ev(n - 1) + ev(n - 2)
fn spin(n: int) -> int:
    s := 0
    i := 0
    while i < n:
        s += i % 7
        i += 1
    return s
fn noisy(n: int) -> int:
    v := fib(n)
    print("late key {v}")
    return v
fn burn(shape: str) -> int:
    if shape == "loop":
        return spin(2000000000)
    if shape == "rec":
        return fib(34)
    if shape == "mutual":
        return ev(34)
    if shape == "map":
        return [34].map(fib)[0]
    xs := [34, 1]
    xs.sort_by_key(noisy)
    return xs[0]
fn work():
    started.send(1)
    print("late {burn(SHAPE)}")
fn quit():
    started.recv()
    os.exit(7)
fn bad():
    started.recv()
    panic("boom")
ex := Executor()
"#;

const SHAPES: [&str; 5] = ["loop", "rec", "mutual", "map", "sortkey"];

/// (party, event, main body, exit code, stderr must contain). `main` is the flagless party; a
/// `job` holds its executor's cancel flag, so `Vm::exit_halt` takes its other arm.
const EVENTS: [(&str, &str, &[&str], i32, &str); 5] = [
    ("main", "exit", &["ex.submit(quit)", "work()"], 7, ""),
    ("main", "fault", &["ex.submit(bad)", "work()"], 1, "boom"),
    (
        "job",
        "exit",
        &["ex.submit(work)", "started.recv()", "os.exit(7)"],
        7,
        "",
    ),
    (
        "job",
        "fault",
        &[
            "ex.submit(work)",
            "ex.submit(bad)",
            "ex.shutdown()",
            r#"print("late main")"#,
        ],
        1,
        "boom",
    ),
    (
        "job",
        "mainfault",
        &["ex.submit(work)", "started.recv()", r#"panic("boom")"#],
        1,
        "boom",
    ),
];

#[test]
fn run_wide_halts_land_inside_every_cpu_bound_shape() {
    let dir = std::env::temp_dir().join(format!("t224_grid_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut red = Vec::new();
    for (party, event, body, code, err_has) in EVENTS {
        for shape in SHAPES {
            let src = format!("{GRID_HDR}SHAPE := \"{shape}\"\n{}\n", body.join("\n"));
            let path = dir.join(format!("{party}_{event}_{shape}.chz"));
            std::fs::write(&path, src).unwrap();
            for t in ["--threads=1", "--threads=2"] {
                let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
                    .args(["run", t])
                    .arg(&path)
                    .output()
                    .expect("spawn chezzi");
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stdout.is_empty()
                    || out.status.code() != Some(code)
                    || !stderr.contains(err_has)
                {
                    red.push(format!(
                        "{party} x {event} x {shape} {t}: code={:?} stdout={stdout:?} stderr={stderr:?}",
                        out.status.code()
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        red.is_empty(),
        "{} red cell(s); nothing prints after a run-wide halt:\n{}",
        red.len(),
        red.join("\n")
    );
}
