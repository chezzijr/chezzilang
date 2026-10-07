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

/// One grid row: its file stem, its source, the report site, and whether `AFTER` may print.
struct Row {
    name: &'static str,
    src: &'static str,
    site: &'static str,
    after_ok: bool,
    /// Run under `chezzi test`: the report is one `ERROR` row on stdout, and the per-test reap and
    /// the end-of-file reap drain the executors after the verdict.
    test_mode: bool,
}

const ROWS: &[Row] = &[
    Row {
        name: "ex_guard",
        src: r#"import std.concurrency
import std.time
fn main():
    s := Shared(0)
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): s.update(fn(x: int) -> int: x + c.recv()))
    time.sleep_ms(50)
    s.update(fn(x: int) -> int: x + 1)
    print("AFTER")
    ex.shutdown()
main()
"#,
        site: "9:5",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "ex_send",
        src: r#"import std.concurrency
import std.time
fn main():
    s := Shared(0)
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): s.update(fn(x: int) -> int: x + c.recv()))
    time.sleep_ms(50)
    s.update(fn(x: int) -> int: x + 1)
    c.send(1)
    print("AFTER")
    ex.shutdown()
main()
"#,
        site: "9:5",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "ex_rw",
        src: r#"import std.concurrency
import std.time
fn main():
    r := RwShared(0)
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): r.write(fn(x: int) -> int: x + c.recv()))
    time.sleep_ms(50)
    v := r.read(fn(x: int) -> int: x)
    print("AFTER {v}")
    ex.shutdown()
main()
"#,
        site: "6:11",
        after_ok: true,
        test_mode: false,
    },
    Row {
        name: "ex_chan",
        src: r#"import std.concurrency
import std.time
fn main():
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): print(c.recv()))
    time.sleep_ms(50)
    ex.shutdown()
    print("AFTER")
main()
"#,
        site: "6:27",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "ex_cycle",
        src: r#"import std.concurrency
import std.time
fn main():
    a := Channel[int](0)
    b := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): b.send(a.recv()))
    ex.submit(fn(): a.send(b.recv()))
    time.sleep_ms(50)
    ex.shutdown()
    print("AFTER")
main()
"#,
        site: "7:28",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "ex_nested",
        src: r#"import std.concurrency
import std.time
fn inner(c: Channel[int]):
    ex2 := Executor()
    ex2.submit(fn(): print(c.recv()))
    ex2.shutdown()
fn main():
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): inner(c))
    time.sleep_ms(50)
    ex.shutdown()
    print("AFTER")
main()
"#,
        site: "9:11",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "sp_guard",
        src: r#"import std.concurrency
import std.time
fn main():
    s := Shared(0)
    c := Channel[int](0)
    parallel:
        spawn: s.update(fn(x: int) -> int: x + c.recv())
        time.sleep_ms(50)
        s.update(fn(x: int) -> int: x + 1)
        print("AFTER")
main()
"#,
        site: "6:5",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "sp_rw",
        src: r#"import std.concurrency
import std.time
fn main():
    r := RwShared(0)
    c := Channel[int](0)
    parallel:
        spawn: r.write(fn(x: int) -> int: x + c.recv())
        time.sleep_ms(50)
        v := r.read(fn(x: int) -> int: x)
        print("AFTER {v}")
main()
"#,
        site: "6:5",
        after_ok: true,
        test_mode: false,
    },
    Row {
        name: "sp_chan",
        src: r#"import std.time
fn main():
    c := Channel[int](0)
    parallel:
        spawn: print(c.recv())
        time.sleep_ms(50)
    print("AFTER")
main()
"#,
        site: "4:5",
        after_ok: false,
        test_mode: false,
    },
    Row {
        name: "sp_cycle",
        src: r#"import std.time
fn main():
    a := Channel[int](0)
    b := Channel[int](0)
    parallel:
        spawn: b.send(a.recv())
        spawn: a.send(b.recv())
        time.sleep_ms(50)
    print("AFTER")
main()
"#,
        site: "5:5",
        after_ok: false,
        test_mode: false,
    },
    // ex_guard under `chezzi test`, with a later test that reaps its own executor. The reaps after
    // the verdict must wait for the victims: a reap that leaves early leaves the deadlocked job to
    // the end-of-file reap, which reports it as a second `ERROR (executor job)` row.
    Row {
        name: "ex_guard_test",
        src: r#"import std.concurrency
import std.time

test fn a_deadlocks():
    s := Shared(0)
    c := Channel[int](0)
    ex := Executor()
    ex.submit(fn(): s.update(fn(x: int) -> int: x + c.recv()))
    time.sleep_ms(50)
    s.update(fn(x: int) -> int: x + 1)
    print("AFTER")

test fn b_runs():
    ex := Executor()
    ex.submit(fn(): print("B"))
    ex.shutdown()
"#,
        site: "10:5",
        after_ok: false,
        test_mode: true,
    },
];

/// Run one cell; `None` when it is good, else a description of what went wrong.
fn run_cell(
    path: &std::path::Path,
    row: &Row,
    t: &str,
    seed: u32,
    pin: Option<&str>,
) -> Option<String> {
    use std::time::{Duration, Instant};
    let cell = format!("{} T={t} seed={seed} pin={pin:?}", row.name);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg(if row.test_mode { "test" } else { "run" })
        .arg(path)
        .env("CHEZZI_THREADS", t)
        .env("CHEZZI_SCHED_SEED", seed.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match pin {
        Some(p) => cmd.env("CHEZZI_TEST_VERDICT_JUDGE", p),
        None => cmd.env_remove("CHEZZI_TEST_VERDICT_JUDGE"),
    };
    let mut child = cmd.spawn().expect("spawn chezzi");
    // The pipes are drained on their own threads so a chatty child never blocks on a full pipe.
    let mut so = child.stdout.take().expect("stdout pipe");
    let mut se = child.stderr.take().expect("stderr pipe");
    let ho = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut so, &mut s).ok();
        s
    });
    let he = std::thread::spawn(move || {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut se, &mut s).ok();
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().expect("poll chezzi") {
            break Some(st);
        }
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().ok();
            child.wait().ok();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = ho.join().unwrap_or_default();
    let stderr = he.join().unwrap_or_default();
    let Some(status) = status else {
        return Some(format!("{cell}: hung past 20 s; stdout {stdout:?}"));
    };
    let site = format!("{}.chz:{}", row.name, row.site);
    // `chezzi test` reports each errored test as one `ERROR` row on stdout.
    let (report, reports) = if row.test_mode {
        (
            &stdout,
            stdout.lines().filter(|l| l.starts_with("ERROR ")).count(),
        )
    } else {
        (&stderr, stderr.matches("runtime error (").count())
    };
    if status.code() != Some(1)
        || !report.contains(&site)
        || reports != 1
        || (!row.after_ok && stdout.contains("AFTER"))
    {
        return Some(format!(
            "{cell}: status {:?}, stdout {stdout:?}, stderr {stderr:?}",
            status.code()
        ));
    }
    None
}

/// TICKET-223 — the whole family: every shape x which judge may decide first x worker count x seed.
/// The run always ends at the first party's site with one report, and nothing prints after the
/// verdict except where a `RwShared.read` legally runs beside a writer (ex_rw, sp_rw).
#[test]
fn deadlock_verdict_grid_ends_at_the_first_party_site() {
    let dir = std::env::temp_dir().join(format!("chz-ticket223-grid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let mut cells = Vec::new();
    for row in ROWS {
        let path = dir.join(format!("{}.chz", row.name));
        std::fs::write(&path, row.src).expect("write fixture");
        for pin in [None, Some("party"), Some("sched")] {
            for t in ["1", "2", "0"] {
                for seed in 1..=8u32 {
                    cells.push((path.clone(), row, t, seed, pin));
                }
            }
        }
    }
    let total = cells.len();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let bad = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((path, row, t, seed, pin)) = cells.get(i) else {
                        break;
                    };
                    if let Some(b) = run_cell(path, row, t, *seed, *pin) {
                        bad.lock().unwrap().push(b);
                    }
                }
            });
        }
    });
    let bad = bad.into_inner().unwrap();
    assert!(
        bad.is_empty(),
        "deadlock grid: {} bad cells of {total}; first: {}",
        bad.len(),
        bad[0]
    );
}
