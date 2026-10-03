//! TICKET-205 — the runner hand-over grid. A party in a CPU loop must hand the runner over at the
//! end of its reduction budget, whatever kind of party it is and whatever kind waits for it:
//! spinner kind x victim kind x victim action x worker count. Every victim acts within a bounded
//! time while the spinner still runs (Go at `GOMAXPROCS=1` acts at ~100 ms in every cell).
//!
//! Before the fix the red cells were spinner {job, job2, jobnursery} x victim {job, job2,
//! jobnursery} x every action at `CHEZZI_THREADS=1`: an Executor job kept its pool thread until
//! it ended.
//!
//! Spawns and polls: a starved child still runs, so the test kills it at the deadline.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SPINNERS: [&str; 7] = ["spawn", "par", "job", "job2", "jobnursery", "map", "update"];
const VICTIMS: [&str; 8] = [
    "spawn",
    "par",
    "job",
    "job2",
    "jobnursery",
    "map",
    "update",
    "main",
];
/// (action, the lines the victim runs after its 100 ms sleep, the exit code that proves it acted)
const ACTIONS: [(&str, &[&str], i32); 4] = [
    ("exit", &["os.exit(3)"], 3),
    ("panic", &["panic(\"boom\")"], 7),
    ("print", &["print(\"V\")", "os.exit(6)"], 6),
    ("send", &["ch.send(1)"], 5),
];
/// `None` leaves `CHEZZI_THREADS` unset (the default worker count).
const THREADS: [Option<&str>; 3] = [Some("1"), Some("2"), None];

const PASS_BOUND: Duration = Duration::from_secs(2);
const KILL_AT: Duration = Duration::from_secs(3);

const PRELUDE: &str = r#"import std.time
import std.os
import std.concurrency
ch := Channel[int](1)
sa := Shared(0)
sb := Shared(0)
fn spin():
    i := 0
    while i < 400000000:
        i = i + 1
fn spinr(x: int) -> int:
    spin()
    return x
fn act():
    time.sleep_ms(100)
    {ACT}
fn actr(x: int) -> int:
    act()
    return x
fn spin_nest():
    parallel:
        spawn spin()
fn act_nest():
    parallel:
        spawn act()
fn spin_map():
    _ys := [1].map(fn(x): spinr(x))
fn act_map():
    _ys := [1].map(fn(x): actr(x))
fn spin_upd():
    sa.update(fn(x): spinr(x))
fn act_upd():
    sb.update(fn(x): actr(x))
fn act_caught():
    _r := recover:
        act()
    os.exit(7)
fn act_nest_caught():
    _r := recover:
        act_nest()
    os.exit(7)
ex := Executor()
ex2 := Executor()
"#;

/// (spinner fn, victim fn) of a party that runs as a spawned task of the main nursery.
fn fiber_party(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "spawn" => Some(("spin", "act")),
        "par" => Some(("spin_nest", "act_nest")),
        "map" => Some(("spin_map", "act_map")),
        "update" => Some(("spin_upd", "act_upd")),
        _ => None,
    }
}

/// (executor, spinner fn, victim fn) of a party that runs as an Executor job. A job's panic is
/// caught inside the job: an uncaught one is reported at the exit join, which is not a runner fact.
fn job_party(kind: &str, panics: bool) -> Option<(&'static str, &'static str, &'static str)> {
    match (kind, panics) {
        ("job", false) => Some(("ex", "spin", "act")),
        ("job", true) => Some(("ex", "spin", "act_caught")),
        ("job2", false) => Some(("ex2", "spin", "act")),
        ("job2", true) => Some(("ex2", "spin", "act_caught")),
        ("jobnursery", false) => Some(("ex", "spin_nest", "act_nest")),
        ("jobnursery", true) => Some(("ex", "spin_nest", "act_nest_caught")),
        _ => None,
    }
}

fn prog(spinner: &str, victim: &str, action: &str) -> String {
    let act = ACTIONS
        .iter()
        .find(|a| a.0 == action)
        .expect("known action")
        .1;
    let panics = action == "panic";
    let mut submits: Vec<String> = Vec::new();
    let mut spawns: Vec<String> = Vec::new();
    let mut victim_is_job = false;
    match job_party(spinner, panics) {
        Some((ex, spin, _)) => submits.push(format!("{ex}.submit({spin})")),
        None => spawns.push(format!(
            "spawn {}()",
            fiber_party(spinner).expect("spinner").0
        )),
    }
    if let Some((ex, _, act_fn)) = job_party(victim, panics) {
        victim_is_job = true;
        submits.push(format!("{ex}.submit({act_fn})"));
    } else if victim != "main" {
        spawns.push(format!(
            "spawn {}()",
            fiber_party(victim).expect("victim").1
        ));
    }
    let mut body: Vec<String> = Vec::new();
    if victim == "main" {
        body.push("time.sleep_ms(100)".into());
        body.extend(act.iter().map(|l| l.to_string()));
    }
    if action == "send" {
        body.push("ch.recv()".into());
        body.push("os.exit(5)".into());
    } else if spawns.is_empty() || !panics || victim_is_job {
        body.push("time.sleep_ms(3000)".into());
    }
    let mut main: Vec<String> = if spawns.is_empty() {
        body
    } else {
        let inner = spawns.into_iter().chain(body).map(|l| format!("    {l}"));
        std::iter::once("parallel:".to_string())
            .chain(inner)
            .collect()
    };
    if panics && !victim_is_job {
        let inner = main.into_iter().map(|l| format!("    {l}"));
        main = std::iter::once("r := recover:".to_string())
            .chain(inner)
            .chain(std::iter::once("os.exit(7)".to_string()))
            .collect();
    }
    let mut out = PRELUDE.replace("{ACT}", &act.join("\n    "));
    for line in submits.into_iter().chain(main) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[test]
fn every_victim_acts_while_the_spinner_runs() {
    let dir = std::env::temp_dir().join(format!("chz-ticket205-grid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");

    let mut cells = Vec::new();
    for s in SPINNERS {
        for v in VICTIMS {
            for (a, _, want) in ACTIONS {
                let path = dir.join(format!("{s}-{v}-{a}.chz"));
                std::fs::write(&path, prog(s, v, a)).expect("write fixture");
                for t in THREADS {
                    cells.push((s, v, a, want, t, path.clone()));
                }
            }
        }
    }

    // Runs one cell; `true` when the victim acted inside `PASS_BOUND`. A closure, not a helper fn:
    // `tests/no_wall_clock_ratio_gates.rs` reads the clock and the sleep off this test body.
    let victim_acts = |path: &std::path::Path, action: &str, want: i32, threads: Option<&str>| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match threads {
            Some(t) => cmd.env("CHEZZI_THREADS", t),
            None => cmd.env_remove("CHEZZI_THREADS"),
        };
        let start = Instant::now();
        let mut child = cmd.spawn().expect("spawn chezzi");
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(st) => break Some(st),
                None if start.elapsed() >= KILL_AT => break None,
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let elapsed = start.elapsed();
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut out = String::new();
        let _ = child
            .stdout
            .take()
            .expect("stdout piped")
            .read_to_string(&mut out);
        status.and_then(|st| st.code()) == Some(want)
            && elapsed < PASS_BOUND
            && (action != "print" || out.contains('V'))
    };

    // Three cells at a time: each cell holds a spinner, so more would measure the machine.
    let next = AtomicUsize::new(0);
    let red = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((s, v, a, want, t, path)) = cells.get(i) else {
                        break;
                    };
                    if !victim_acts(path, a, *want, *t) {
                        let label = format!("{s}/{v}/{a}/{}", t.unwrap_or("unset"));
                        red.lock().unwrap().push((i, label));
                    }
                }
            });
        }
    });
    let _ = std::fs::remove_dir_all(&dir);

    let mut red = red.into_inner().unwrap();
    red.sort();
    let red: Vec<String> = red.into_iter().map(|(_, label)| label).collect();
    assert!(
        red.is_empty(),
        "runner hand-over grid red: {} of {} cells\n{}",
        red.len(),
        cells.len(),
        red.join("\n")
    );
}
