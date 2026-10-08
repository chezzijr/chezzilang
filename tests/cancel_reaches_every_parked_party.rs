//! TICKET-200 (Family B3): a cancel must reach a socket-parked fiber of a job's nursery, and an
//! unjoined job fault that precedes a `--timeout` must be reported, not dropped.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

const NET_PRELUDE: &str = "import std.time\nimport std.net\nimport std.concurrency\nfn lis() -> Listener:\n    match net.listen(\"127.0.0.1:0\"):\n        Ok(l): return l\n        Err(e): panic(\"{e}\")\n";

#[test]
fn shutdown_now_reaches_a_job_nursery_fiber_parked_in_accept() {
    let dir = std::env::temp_dir().join(format!("chz-t200-accept-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("n5.chz");
    let body = "ln := lis()\nfn srv():\n    defer:\n        print(\"srv defer\")\n    parallel:\n        spawn:\n            defer:\n                print(\"fiber defer\")\n            r := ln.accept()\n            print(\"accept returned {r}\")\nex := Executor()\nex.submit(srv)\ntime.sleep_ms(100)\nex.shutdown_now()\nprint(\"after shutdown_now\")\n";
    std::fs::write(&path, format!("{NET_PRELUDE}{body}")).expect("write program");
    let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.expect("shutdown_now hung: no exit within 10s");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("fiber defer") && stdout.contains("after shutdown_now"),
        "shutdown_now must cut the accept-parked fiber and run its defers: {stdout:?}"
    );
}

#[test]
fn unjoined_job_fault_before_timeout_is_reported() {
    let dir = std::env::temp_dir().join(format!("chz-t200-rank-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("y2_test.chz");
    std::fs::write(
        &path,
        "import std.time\nimport std.concurrency\nfn bad():\n    xs := [1]\n    print(xs[3])\ntest fn unjoined_then_times_out():\n    ex := Executor()\n    ex.submit(bad)\n    time.sleep_ms(3000)\n",
    )
    .expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--timeout=500"])
        .arg(&path)
        .output()
        .expect("run chezzi test");
    let _ = std::fs::remove_dir_all(&dir);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("index"),
        "the earlier unjoined job fault must be reported, not only TIMED-OUT: {all:?}"
    );
}

/// Every park the C1 grid runs: `(name, op text at 4-space indent, socket park)`.
const PARKS: [(&str, &str, bool); 9] = [
    ("recv", "    _ := stuck.recv()", false),
    ("send", "    fc.send(1)\n    fc.send(2)", false),
    (
        "wait",
        "    wait:\n        v := stuck.recv():\n            print(v)",
        false,
    ),
    ("sleep", "    time.sleep_ms(8000)", false),
    ("guard", "    g.update(bump)", false),
    ("accept", "    _ := ln2.accept()", true),
    ("read", "    _ := c.read(10)", true),
    ("write", "    _ := c.write(\"x\".repeat(8388608))", true),
    ("connect", "    _ := net.connect(\"10.255.255.1:9\")", true),
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    SiblingFault,
    ShutdownNow,
    NestedShutdownNow,
    Exit,
    Timeout,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Owner {
    MainNursery,
    Job,
    JobNursery,
    NestedJobNursery,
}

/// Shared by every C1 cell. `peer` keeps `c` connected and never reads or writes, and nobody dials
/// `ln2`, so read, write and accept park (Digest gotcha 5).
const GRID_PRELUDE: &str = "import std.time
import std.net
import std.os
import std.concurrency
fn must[T](r: Result[T]) -> T:
    match r:
        Ok(v): return v
        Err(e): panic(e.message())
stuck := Channel[int](0)
fc := Channel[int](1)
g := Shared[int](0)
ln := must(net.listen(\"127.0.0.1:0\"))
ln2 := must(net.listen(\"127.0.0.1:0\"))
c := must(net.connect(must(ln.addr())))
peer := must(ln.accept())
fn hold3s(v: int) -> int:
    time.sleep_ms(3000)
    return v
fn holder() -> None:
    g.update(hold3s)
fn bump(v: int) -> int:
    return v + 1
";

/// Holds `g` for 3 s so the guard park waits on it (copied from `tests/owner_fault_grid.rs`).
const GUARD_PRELUDE: &str = "hx := Executor()\nhx.submit(holder)\ntime.sleep_ms(20)\n";

/// Every cell of the C1 grid that applies. N/A, by reason:
/// - owner job x {accept, read, write, connect}: a socket op in an Executor job body Refuses
///   (DEC-181).
/// - sibling fault x owner job: a job has no sibling in its cancel scope.
/// - shutdown_now and nested shutdown_now x owner main nursery: there is no executor to shut.
/// - nested shutdown_now x owner job and job's nursery: only a nested job's nursery has an `ex2`.
fn applies(src: Source, owner: Owner, socket: bool) -> bool {
    if owner == Owner::Job && socket {
        return false;
    }
    match src {
        Source::SiblingFault => owner != Owner::Job,
        Source::ShutdownNow => owner != Owner::MainNursery,
        Source::NestedShutdownNow => owner == Owner::NestedJobNursery,
        Source::Exit | Source::Timeout => true,
    }
}

/// The program text of one C1 cell. The `--timeout` source puts the main lines inside `test fn t()`.
fn grid_program(src: Source, owner: Owner, op: &str, guard: bool) -> String {
    let sibling = match (src, owner) {
        (Source::SiblingFault, _) => {
            "        spawn:\n            time.sleep_ms(50)\n            panic(\"boom\")\n"
        }
        (Source::Exit, Owner::MainNursery) => {
            "        spawn:\n            time.sleep_ms(100)\n            os.exit(3)\n"
        }
        // A live sleeper, so a channel park ends on the deadline, not on a deadlock verdict.
        (Source::Timeout, Owner::MainNursery) => {
            "        spawn:\n            time.sleep_ms(3000)\n"
        }
        _ => "",
    };
    let inner_end = if src == Source::NestedShutdownNow {
        "    time.sleep_ms(50)\n    ex2.shutdown_now()\n"
    } else {
        "    ex2.shutdown()\n"
    };
    let mut p = String::from(GRID_PRELUDE);
    if guard {
        p.push_str(GUARD_PRELUDE);
    }
    p.push_str(&format!(
        "fn op():\n    defer:\n        print(\"fiber defer\")\n{op}\n    print(\"op returned\")\n"
    ));
    p.push_str(&format!(
        "fn nurs():\n    parallel:\n        spawn:\n            op()\n{sibling}"
    ));
    p.push_str(&format!(
        "fn outer():\n    ex2 := Executor()\n    ex2.submit(nurs)\n{inner_end}"
    ));
    let mut main: Vec<&str> = match owner {
        Owner::MainNursery => vec!["nurs()"],
        Owner::Job => vec!["ex := Executor()", "ex.submit(op)"],
        Owner::JobNursery => vec!["ex := Executor()", "ex.submit(nurs)"],
        Owner::NestedJobNursery => vec!["ex := Executor()", "ex.submit(outer)"],
    };
    if owner != Owner::MainNursery {
        main.extend(match src {
            Source::SiblingFault | Source::NestedShutdownNow => {
                vec!["ex.shutdown()", "print(\"after\")"]
            }
            Source::ShutdownNow => vec![
                "time.sleep_ms(100)",
                "ex.shutdown_now()",
                "print(\"after\")",
            ],
            Source::Exit => vec!["time.sleep_ms(100)", "os.exit(3)"],
            Source::Timeout => vec!["time.sleep_ms(3000)", "ex.shutdown()"],
        });
    }
    if src == Source::Timeout {
        p.push_str("test fn t():\n");
        for l in main {
            p.push_str(&format!("    {l}\n"));
        }
    } else {
        for l in main {
            p.push_str(&format!("{l}\n"));
        }
    }
    p
}

/// Runs `chezzi <args> <program>` at `CHEZZI_THREADS=<threads>` (env removed when `None`); `None`
/// means it was still running after 10 s and was killed. `chezzi run` goes through
/// `support/hang_deadline.rs`; `chezzi test` uses the same poll here, because that helper runs only
/// `chezzi run`.
fn run_bounded(args: &[&str], program: &Path, threads: Option<&str>) -> Option<Output> {
    if args == ["run"] {
        return hang_deadline::run_with_hang_deadline(program, threads);
    }
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args).arg(program);
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("try_wait chezzi").is_some() {
            return Some(child.wait_with_output().expect("collect chezzi output"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Writes `src` to a fresh temp file named `file`, runs it bounded, and returns `(rc, stdout +
/// stderr)`; `None` = hung.
fn run_src(
    tag: &str,
    file: &str,
    src: &str,
    args: &[&str],
    threads: Option<&str>,
) -> Option<(i32, String)> {
    let dir = std::env::temp_dir().join(format!("chz-t200-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(file);
    std::fs::write(&path, src).expect("write program");
    let out = run_bounded(args, &path, threads);
    let _ = std::fs::remove_dir_all(&dir);
    out.map(|o| {
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        (o.status.code().unwrap_or(-1), all)
    })
}

/// Runs one C1 cell and returns why it failed, or `None` when it passed.
fn grid_cell(src: Source, owner: Owner, park: &(&str, &str, bool), t: &str) -> Option<String> {
    let (pname, op, _) = *park;
    let prog = grid_program(src, owner, op, pname == "guard");
    let tag = format!("{src:?}-{pname}-{owner:?}-{t}");
    let got = if src == Source::Timeout {
        run_src(
            &tag,
            "cell_test.chz",
            &prog,
            &["test", "--timeout=500"],
            Some(t),
        )
    } else {
        run_src(&tag, "cell.chz", &prog, &["run"], Some(t))
    };
    let Some((rc, all)) = got else {
        return Some("HUNG: no exit within 10s".into());
    };
    let mut why = Vec::new();
    if all.contains("op returned") {
        why.push("the op returned".to_string());
    }
    let defers = all.matches("fiber defer").count();
    let unwinds = matches!(
        src,
        Source::SiblingFault | Source::ShutdownNow | Source::NestedShutdownNow
    );
    if unwinds && defers != 1 {
        why.push(format!("{defers} `fiber defer` lines, want 1"));
    }
    if matches!(src, Source::ShutdownNow | Source::NestedShutdownNow) && !all.contains("after") {
        why.push("no `after`".into());
    }
    if src == Source::Exit && rc != 3 {
        why.push(format!("rc {rc}, want 3"));
    }
    if src == Source::Timeout && !all.contains("TIMED-OUT") {
        why.push("no TIMED-OUT".into());
    }
    if why.is_empty() {
        None
    } else {
        Some(format!("{}: {all:?}", why.join(", ")))
    }
}

/// Runs every applicable C1 cell, eight at a time, and returns the failing ones as
/// `source/park/owner/T: why`.
fn cancel_grid_failures() -> Vec<String> {
    const SOURCES: [Source; 5] = [
        Source::SiblingFault,
        Source::ShutdownNow,
        Source::NestedShutdownNow,
        Source::Exit,
        Source::Timeout,
    ];
    const OWNERS: [Owner; 4] = [
        Owner::MainNursery,
        Owner::Job,
        Owner::JobNursery,
        Owner::NestedJobNursery,
    ];
    let mut cells = Vec::new();
    for src in SOURCES {
        for owner in OWNERS {
            for park in &PARKS {
                if !applies(src, owner, park.2) {
                    continue;
                }
                for t in ["1", "2", "0"] {
                    cells.push((src, owner, park, t));
                }
            }
        }
    }
    let failures = Mutex::new(Vec::new());
    for chunk in cells.chunks(8) {
        std::thread::scope(|s| {
            for &(src, owner, park, t) in chunk {
                let failures = &failures;
                s.spawn(move || {
                    if let Some(why) = grid_cell(src, owner, park, t) {
                        let name = format!("{src:?}/{}/{owner:?}/{t}", park.0);
                        failures.lock().unwrap().push(format!("{name}: {why}"));
                    }
                });
            }
        });
    }
    let mut f = failures.into_inner().unwrap();
    f.sort();
    f
}

/// C1 grid: cancel source x park x owner x T. Every cell is cut, and runs its defers where the
/// source unwinds (sibling fault, shutdown_now). os.exit runs no defers (Digest gotcha 3; Go's
/// `os.Exit` runs none either).
#[test]
fn cancel_grid_every_source_park_owner_and_thread_count() {
    let failures = cancel_grid_failures();
    assert!(
        failures.is_empty(),
        "{} C1 cells failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The prelude of every ranking cell: `bad` faults at once, `slowbad` 300 ms later.
const RANK_PRELUDE: &str = "import std.time
import std.os
import std.concurrency
fn bad():
    xs := [1]
    print(xs[3])
fn slowbad():
    time.sleep_ms(300)
    xs := [1]
    print(xs[3])
fn late_exit():
    defer:
        time.sleep_ms(100)
        os.exit(3)
";

const SUBMIT_BAD: &str = "ex := Executor()\nex.submit(bad)\ntime.sleep_ms(100)\n";
const SUBMIT_SLOWBAD: &str = "ex := Executor()\nex.submit(slowbad)\n";
const DEADLOCK: &str = "_ := Channel[int]().recv()\n";
const GROW: &str = "xs: List[str] = []\ni := 0\nwhile true:\n    xs.push(\"{i} abcdefghijklmnopqrstuvwxyz0123456789abcd\")\n    i += 1\n";

/// One ranking cell: name, the `chezzi test` flag (`None` = `chezzi run`, `""` = no flag), the main
/// lines, the texts the report must hold, the exit code it must have, the texts it must not hold.
struct RankCell {
    name: &'static str,
    flag: Option<&'static str>,
    main: String,
    want: Vec<&'static str>,
    rc: Option<i32>,
    unwanted: Vec<&'static str>,
}

fn rank(
    name: &'static str,
    flag: Option<&'static str>,
    main: String,
    want: Vec<&'static str>,
    unwanted: Vec<&'static str>,
) -> RankCell {
    RankCell {
        name,
        flag,
        main,
        want,
        rc: None,
        unwanted,
    }
}

fn ranking_cells() -> Vec<RankCell> {
    let bad = |tail: &str| format!("{SUBMIT_BAD}{tail}");
    let first = |tail: &str| format!("{SUBMIT_SLOWBAD}{tail}");
    let index = "index 3 out of bounds";
    vec![
        rank("run deadlock", None, bad(DEADLOCK), vec![index], vec![]),
        // An exit published AFTER an earlier job fault outranks it (DEC-200). The exit sits in a
        // `defer`, which a job fault does not cut; `main`'s own sleep would be cut before it.
        RankCell {
            rc: Some(3),
            ..rank(
                "run exit",
                None,
                "ex := Executor()\nex.submit(bad)\nlate_exit()\n".to_string(),
                vec![],
                vec![],
            )
        },
        RankCell {
            rc: Some(3),
            ..rank(
                "run exit first",
                None,
                first("os.exit(3)\n"),
                vec![],
                vec![],
            )
        },
        rank(
            "run own fault after",
            None,
            bad("panic(\"main boom\")\n"),
            vec![index, "at bad"],
            vec![],
        ),
        rank(
            "run own fault first",
            None,
            first("panic(\"main first\")\n"),
            vec!["main first"],
            vec![index],
        ),
        rank(
            "test timeout",
            Some("--timeout=500"),
            bad("time.sleep_ms(3000)\n"),
            vec!["ERROR", index],
            vec!["TIMED-OUT"],
        ),
        rank(
            "test max-heap",
            Some("--max-heap=20000000"),
            bad(GROW),
            vec!["ERROR", index],
            vec!["OVER-MEMORY"],
        ),
        rank(
            "test deadlock",
            Some(""),
            bad(DEADLOCK),
            vec!["ERROR", index],
            vec![],
        ),
        rank(
            "test exit",
            Some(""),
            bad("os.exit(3)\n"),
            vec!["ERROR", "exit"],
            vec![],
        ),
        rank(
            "test own fault after",
            Some(""),
            bad("assert 1 == 2\n"),
            vec!["ERROR", index],
            vec![],
        ),
        rank(
            "test own fault first",
            Some(""),
            first("assert 1 == 2\n"),
            vec!["FAIL"],
            vec![index],
        ),
        rank(
            "test timeout no job",
            Some("--timeout=500"),
            "time.sleep_ms(3000)\n".into(),
            vec!["TIMED-OUT"],
            vec![],
        ),
        rank(
            "test max-heap no job",
            Some("--max-heap=20000000"),
            GROW.into(),
            vec!["OVER-MEMORY"],
            vec![],
        ),
    ]
}

/// Runs every ranking cell and returns the failing ones as `name: why`.
fn ranking_grid_failures() -> Vec<String> {
    let mut failures = Vec::new();
    for cell in ranking_cells() {
        let mut src = String::from(RANK_PRELUDE);
        let tag = cell.name.replace(' ', "-");
        let got = match cell.flag {
            None => {
                src.push_str(&cell.main);
                run_src(&tag, "rank.chz", &src, &["run"], None)
            }
            Some(flag) => {
                src.push_str("test fn t():\n");
                for l in cell.main.lines() {
                    src.push_str(&format!("    {l}\n"));
                }
                let mut args = vec!["test"];
                if !flag.is_empty() {
                    args.push(flag);
                }
                run_src(&tag, "rank_test.chz", &src, &args, None)
            }
        };
        let Some((rc, all)) = got else {
            failures.push(format!("{}: HUNG: no exit within 10s", cell.name));
            continue;
        };
        let mut why = Vec::new();
        for w in &cell.want {
            if !all.contains(w) {
                why.push(format!("no `{w}`"));
            }
        }
        for u in &cell.unwanted {
            if all.contains(u) {
                why.push(format!("holds `{u}`"));
            }
        }
        if let Some(r) = cell.rc
            && rc != r
        {
            why.push(format!("rc {rc}, want {r}"));
        }
        if !why.is_empty() {
            failures.push(format!("{}: {}: {all:?}", cell.name, why.join(", ")));
        }
    }
    failures
}

/// C2 grid: {unjoined job fault, own fault} x {deadlock, --timeout, --max-heap, exit} in run and
/// test modes. The ranking is exit > an earlier unjoined job fault > the run's own cause.
#[test]
fn ranking_grid_every_fault_and_terminal_cause() {
    let failures = ranking_grid_failures();
    assert!(
        failures.is_empty(),
        "{} ranking cells failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
