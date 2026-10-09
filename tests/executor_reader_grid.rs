//! TICKET-236 (wave 22 family B): the Executor reader grid. Reader x wait x job end x cutter x
//! worker count. `tests/executor_stop_grid.rs` always reads the handle from main; this grid moves
//! the reader. Each cell is a generated program run through the built binary.
//!
//! The rule the grid pins: a reader of a job's handle is never declared deadlocked, however the
//! job ends, because the handle always settles. A reader of a PLAIN channel whose feeder job was
//! cut is a real deadlock and must still be reported.
//!
//! Harness copied from `tests/executor_stop_grid.rs` (`run`, `Got`, `judge`): each integration
//! test is its own crate.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const JOB_FAULT: &str = "index 5 out of bounds (len 1)";
const GUARD: Duration = Duration::from_secs(30);
/// The guard of the declines class, whose expected outcome is a hang.
const DECLINES_GUARD: Duration = Duration::from_secs(5);
/// `0` is the default worker count.
const THREADS: [&str; 4] = ["1", "2", "4", "0"];

/// One finished run. `code` is `None` when the harness killed the child at its guard.
struct Got {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Got {
    fn show(&self) -> String {
        format!(
            "code={:?}\n--stdout--\n{}--stderr--\n{}",
            self.code, self.out, self.err
        )
    }
    fn r_lines(&self) -> Vec<&str> {
        self.out.lines().filter(|l| l.starts_with("r ")).collect()
    }
    fn ended(&self) -> bool {
        self.out.lines().any(|l| l == "end")
    }
    fn deadlock_fault(&self) -> bool {
        self.code.is_some_and(|c| c != 0) && self.err.contains("deadlock")
    }
}

/// Runs `src` at `threads` workers, killing it at `guard`.
fn run(src: &str, threads: &str, guard: Duration) -> Got {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t236-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn chezzi");
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            s.push_str(&line);
            s.push('\n');
        }
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let code = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > guard {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out_reader.join().expect("stdout reader");
    let err = err_reader.join().expect("stderr reader");
    let _ = std::fs::remove_dir_all(&dir);
    Got { code, out, err }
}

/// Asserts `misses` is empty, printing every red cell.
fn judge(misses: Vec<String>) {
    assert!(
        misses.is_empty(),
        "{} red cell(s):\n{}",
        misses.len(),
        misses.join("\n")
    );
}

/// Who waits for the job.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Reader {
    Main,
    Nursery,
    /// A nursery fiber, inside a `map` callback.
    Callback,
    /// A nursery fiber, inside a `defer`.
    Defer,
    /// A job of another Executor.
    OtherExec,
    /// A job of the Executor that runs the awaited job.
    SameExec,
}

/// What the reader waits on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Wait {
    /// `recv` on the `submit_result` channel.
    Recv,
    /// `Task.get`.
    Get,
    /// `recv` on a plain channel the job sends on as its last statement.
    Plain,
}

/// How the awaited job ends.
#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    Returns,
    Faults,
    /// Cut while held behind the cap.
    CutHeld,
    CutRunning,
    CutSleeping,
    /// Cut while parked on a channel.
    CutParked,
    /// Held behind a job that faults: dropped from that job's `finish`.
    DropFinish,
}

impl End {
    fn is_cut(self) -> bool {
        matches!(
            self,
            End::CutHeld | End::CutRunning | End::CutSleeping | End::CutParked
        )
    }
}

/// Who calls `shutdown_now()`. `None` for an end that is not a cut.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Cutter {
    None,
    Sibling,
    Main,
    Job,
}

type Cell = (Reader, Wait, End, Cutter);

/// The cutters of one (reader, end) pair. Main cannot both read and cut.
fn cutters(reader: Reader, end: End) -> Vec<Cutter> {
    if !end.is_cut() {
        return vec![Cutter::None];
    }
    [Cutter::Sibling, Cutter::Main, Cutter::Job]
        .into_iter()
        .filter(|&c| !(reader == Reader::Main && c == Cutter::Main))
        .collect()
}

const ARGS: &str = "inner, gate, plain, started";
const SIG: &str = "inner: Executor, gate: Channel[int], plain: Channel[int], started: Channel[int]";

/// The program of one cell: `rounds` rounds, one `r <value>` line each, then `end`.
fn prog(reader: Reader, wait: Wait, end: End, cutter: Cutter, rounds: usize) -> String {
    let end_body = match end {
        End::Returns | End::CutHeld | End::DropFinish => "time.sleep_ms(1)",
        End::Faults => "time.sleep_ms(1)\n    if boom() == 0:\n        pass",
        End::CutRunning => {
            "s := 0\n    for i in 0..3000000:\n        s += i\n    if s < 0:\n        pass"
        }
        End::CutSleeping => "time.sleep_ms(30)",
        End::CutParked => "_ := gate.recv()",
    };
    let wait_body = match wait {
        Wait::Recv => {
            "    ch := inner.submit_result(fn() -> int: job(gate))\n    started.send(1)\n    match ch.recv():\n        ?v: return v\n        !e: return -1\n"
        }
        Wait::Get => {
            "    h := task.submit_task(inner, fn() -> int: job(gate))\n    started.send(1)\n    match h.get():\n        ?v: return v\n        !e: return -1\n"
        }
        Wait::Plain => {
            "    inner.submit(fn(): jobp(gate, plain))\n    started.send(1)\n    return plain.recv()\n"
        }
    };
    let mut s =
        String::from("import std.concurrency\nimport std.concurrency.task\nimport std.time\n");
    s.push_str("fn boom() -> int:\n    xs := [1]\n    return xs[5]\n");
    s.push_str(&format!(
        "fn job(gate: Channel[int]) -> int:\n    {end_body}\n    return 7\n"
    ));
    s.push_str(&format!(
        "fn jobp(gate: Channel[int], plain: Channel[int]):\n    {end_body}\n    plain.send(7)\n"
    ));
    s.push_str("fn blocker(gate: Channel[int]):\n    _ := gate.recv()\n");
    s.push_str("fn faulty():\n    time.sleep_ms(2)\n    if boom() == 0:\n        pass\n");
    s.push_str(&format!("fn await_j({SIG}) -> int:\n{wait_body}"));
    s.push_str(&format!(
        "fn reader({SIG}, out: Channel[int]):\n    out.send(await_j({ARGS}))\n"
    ));
    s.push_str(&format!(
        "fn reader_cb({SIG}, out: Channel[int]):\n    ys := [0].map(fn(i: int) -> int: await_j({ARGS}))\n    out.send(ys[0])\n"
    ));
    s.push_str(&format!(
        "fn reader_defer({SIG}, out: Channel[int]):\n    defer:\n        out.send(await_j({ARGS}))\n"
    ));
    s.push_str(
        "fn stopper(inner: Executor, started: Channel[int], ms: int):\n    _ := started.recv()\n    time.sleep_ms(ms)\n    inner.shutdown_now()\n",
    );

    // One round. The lines land inside `for rn in 0..rounds:`.
    let cap = if reader == Reader::SameExec { 2 } else { 1 };
    let mut b: Vec<String> = vec![
        format!("inner := Executor({cap})"),
        "cx := Executor(1)".into(),
        "rx := Executor(1)".into(),
        "gate := Channel[int](1)".into(),
        "plain := Channel[int](1)".into(),
        "started := Channel[int](1)".into(),
        "out := Channel[int](1)".into(),
    ];
    if end == End::CutHeld {
        b.push("inner.submit(fn(): blocker(gate))".into());
    }
    if end == End::DropFinish {
        b.push("inner.submit(fn(): faulty())".into());
    }
    if cutter == Cutter::Job {
        b.push("cx.submit(fn(): stopper(inner, started, rn % 3))".into());
    }
    let main_cut: &[&str] = if cutter == Cutter::Main {
        &[
            "_ := started.recv()",
            "time.sleep_ms(rn % 3)",
            "inner.shutdown_now()",
        ]
    } else {
        &[]
    };
    const SPAWN_STOPPER: &str = "    spawn stopper(inner, started, rn % 3)";
    const PRINT: &str = "print(\"r {v}\")";
    match reader {
        Reader::Main => {
            if cutter == Cutter::Sibling {
                b.push("parallel:".into());
                b.push(SPAWN_STOPPER.into());
                b.push(format!("    v := await_j({ARGS})"));
                b.push(format!("    {PRINT}"));
            } else {
                b.push(format!("v := await_j({ARGS})"));
                b.push(PRINT.into());
            }
        }
        Reader::Nursery | Reader::Callback | Reader::Defer => {
            let f = match reader {
                Reader::Callback => "reader_cb",
                Reader::Defer => "reader_defer",
                _ => "reader",
            };
            b.push("parallel:".into());
            b.push(format!("    spawn {f}({ARGS}, out)"));
            if cutter == Cutter::Sibling {
                b.push(SPAWN_STOPPER.into());
            }
            b.extend(main_cut.iter().map(|m| format!("    {m}")));
            b.push("    v := out.recv()".into());
            b.push(format!("    {PRINT}"));
        }
        Reader::OtherExec | Reader::SameExec => {
            let ex = if reader == Reader::OtherExec {
                "rx"
            } else {
                "inner"
            };
            b.push(format!("{ex}.submit(fn(): reader({ARGS}, out))"));
            if cutter == Cutter::Sibling {
                b.push("parallel:".into());
                b.push(SPAWN_STOPPER.into());
                b.push("    v := out.recv()".into());
                b.push(format!("    {PRINT}"));
            } else {
                b.extend(main_cut.iter().map(|m| m.to_string()));
                b.push("v := out.recv()".into());
                b.push(PRINT.into());
            }
        }
    }
    s.push_str(&format!("fn main():\n    for rn in 0..{rounds}:\n"));
    for line in &b {
        s.push_str(&format!("        {line}\n"));
    }
    s.push_str("main()\nprint(\"end\")\n");
    s
}

/// What a cell's run must look like.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Class {
    /// Exit 0, `end`, one `r 7` or `r -1` line per round.
    Settle,
    /// A real deadlock: nonzero exit, stderr names a deadlock.
    Deadlock,
    /// `docs/gaps.md` W22-1: a real deadlock the verdict does not report. A hang or a deadlock
    /// fault, never a settle.
    Declines,
    /// The job's own fault ends the run; no deadlock is reported.
    JobFault,
    /// The same `shutdown_now()` cuts the reader. Two legal outcomes: every round settles, or the
    /// run ends with a deadlock fault.
    Race,
}

fn class((reader, wait, end, cutter): Cell) -> Class {
    let cut = end.is_cut();
    if end == End::DropFinish || (wait == Wait::Plain && end == End::Faults) {
        Class::JobFault
    } else if reader == Reader::SameExec && wait != Wait::Plain && cut {
        Class::Race
    } else if wait == Wait::Plain && cut {
        if reader == Reader::OtherExec && cutter == Cutter::Sibling {
            Class::Declines
        } else {
            Class::Deadlock
        }
    } else {
        Class::Settle
    }
}

/// `None` when every round settled, else what is wrong.
fn settled(end: End, rounds: usize, g: &Got) -> Option<String> {
    if g.code != Some(0) {
        return Some("exit status is not zero".to_string());
    }
    if !g.ended() {
        return Some("the run did not print `end`".to_string());
    }
    let legal = |l: &str| match end {
        End::Returns => l == "r 7",
        End::Faults => l == "r -1",
        _ => l == "r 7" || l == "r -1",
    };
    let rs = g.r_lines();
    if rs.len() != rounds || !rs.iter().all(|l| legal(l)) {
        return Some(format!(
            "want {rounds} settled round(s), got {} `r` line(s)",
            rs.len()
        ));
    }
    None
}

/// `None` when the run obeys its cell's class, else what is wrong.
fn check(cell: Cell, rounds: usize, g: &Got) -> Option<String> {
    match class(cell) {
        Class::Settle => settled(cell.2, rounds, g),
        Class::Deadlock => (!g.deadlock_fault())
            .then(|| "a cut feeder of a plain channel must end in a deadlock fault".to_string()),
        Class::Declines => (g.code.is_some() && !g.deadlock_fault())
            .then(|| "W22-1 must hang or report a deadlock, never settle".to_string()),
        Class::JobFault => (!g.err.contains(JOB_FAULT) || g.err.contains("deadlock"))
            .then(|| "the job's fault must end the run, with no deadlock".to_string()),
        Class::Race => {
            if g.deadlock_fault() {
                None
            } else {
                settled(cell.2, rounds, g)
            }
        }
    }
}

/// Runs every cell at every worker count on six threads and judges each run by its class.
fn sweep(cells: &[Cell], rounds: usize) {
    let jobs: Vec<(Cell, &str)> = cells
        .iter()
        .flat_map(|&c| THREADS.into_iter().map(move |t| (c, t)))
        .collect();
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..6 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(cell, t)) = jobs.get(i) else {
                        break;
                    };
                    let (reader, wait, end, cutter) = cell;
                    let guard = if class(cell) == Class::Declines {
                        DECLINES_GUARD
                    } else {
                        GUARD
                    };
                    let g = run(&prog(reader, wait, end, cutter, rounds), t, guard);
                    if let Some(why) = check(cell, rounds, &g) {
                        misses.lock().unwrap().push(format!(
                            "{reader:?} x {wait:?} x {end:?} x {cutter:?} T={t} ({:?}): {why}\n{}",
                            class(cell),
                            g.show()
                        ));
                    }
                }
            });
        }
    });
    judge(misses.into_inner().unwrap());
}

const READERS: [Reader; 6] = [
    Reader::Main,
    Reader::Nursery,
    Reader::Callback,
    Reader::Defer,
    Reader::OtherExec,
    Reader::SameExec,
];

#[test]
fn reader_grid_every_cell_ends_in_its_class() {
    let mut cells = Vec::new();
    for reader in READERS {
        for wait in [Wait::Recv, Wait::Get, Wait::Plain] {
            for end in [
                End::Returns,
                End::Faults,
                End::CutHeld,
                End::CutRunning,
                End::CutSleeping,
                End::CutParked,
                End::DropFinish,
            ] {
                for cutter in cutters(reader, end) {
                    cells.push((reader, wait, end, cutter));
                }
            }
        }
    }
    sweep(&cells, 1);
}

/// The false verdict is a race: one round rarely shows it. 100 rounds made the base binary red in
/// 28 of 40 runs of `Nursery x Recv x CutSleeping x Sibling`.
#[test]
fn reader_grid_handle_cells_survive_stress() {
    let mut cells = Vec::new();
    for reader in READERS {
        if reader == Reader::SameExec {
            continue;
        }
        for wait in [Wait::Recv, Wait::Get] {
            for end in [End::Returns, End::CutSleeping, End::CutParked] {
                for cutter in cutters(reader, end) {
                    cells.push((reader, wait, end, cutter));
                }
            }
        }
        cells.push((reader, Wait::Plain, End::Returns, Cutter::None));
    }
    assert!(cells.iter().all(|&c| class(c) == Class::Settle));
    sweep(&cells, 100);
}

/// The feeder is cut while it sleeps, before its send: nothing can feed `plain` any more.
const CUT_PLAIN_FEEDER: &str = r#"import std.concurrency
import std.time

fn feeder(plain: Channel[int]):
    time.sleep_ms(50)
    plain.send(1)

fn reader(plain: Channel[int], out: Channel[int]):
    out.send(plain.recv())

fn stopper(inner: Executor):
    inner.shutdown_now()

fn main():
    inner := Executor(1)
    plain := Channel[int](1)
    out := Channel[int](1)
    inner.submit(fn(): feeder(plain))
    parallel:
        spawn reader(plain, out)
        spawn stopper(inner)
        v := out.recv()
        print("got {v}")

main()
print("end")
"#;

#[test]
fn a_cut_plain_channel_feeder_is_still_a_deadlock() {
    let mut misses = Vec::new();
    for t in THREADS {
        let g = run(CUT_PLAIN_FEEDER, t, GUARD);
        if !g.deadlock_fault() {
            misses.push(format!("T={t}: want a deadlock fault\n{}", g.show()));
        }
    }
    judge(misses);
}
