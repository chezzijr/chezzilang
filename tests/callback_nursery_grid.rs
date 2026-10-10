//! TICKET-245 — a blocking operation inside a native callback, from every caller shape, at every
//! worker count. The defect: a nursery joiner that cannot park ran ANY runnable fiber on its own
//! stack, and that fiber could wait in place for something only the buried frame can give.
//!
//! Grid: operation x callback kind x callers x `CHEZZI_THREADS`; every cell is RUN in a fresh
//! process under a timeout and its printed value is checked. Go (`sync.Mutex` held across 4
//! goroutines joined by a `sync.WaitGroup`, 3 callers x 20 rounds, `GOMAXPROCS(1)`) prints `240`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);
const THREADS: [&str; 3] = ["1", "2", "0"];
const ROUNDS: usize = 20;

/// Runs `src` at `threads` workers; `None` = still running after [`LIMIT`] (killed).
fn run(src: &str, threads: &str) -> Option<(i32, String, String)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t245-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut out = String::new();
    let mut err = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut out);
    let _ = child.stderr.take().unwrap().read_to_string(&mut err);
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| (s.code().unwrap_or(-1), out, err))
}

/// `None` when the run printed exactly `want` and exited 0; otherwise what it did instead.
fn miss(src: &str, threads: &str, want: &str) -> Option<String> {
    match run(src, threads) {
        None => Some("TIMEOUT".to_string()),
        Some((0, out, _)) if out.trim() == want => None,
        Some((code, out, err)) => Some(format!("rc={code} stdout={out:?} stderr={err:?}")),
    }
}

/// The blocking operation inside the callback: the body of `fn body(x: int, q: RwShared[int])`.
/// Every one returns `x + 4`.
const OPS: [(&str, &str); 5] = [
    (
        "nursery",
        "    c := AtomicInt(0)\n    parallel:\n        for _ in 0..4:\n            spawn one(c)\n    return x + c.load()\n",
    ),
    (
        "child_recv",
        "    ch := Channel[int]()\n    v := 0\n    parallel:\n        spawn give(ch)\n        v = ch.recv()\n    return x + v\n",
    ),
    (
        "submit_result",
        "    ex := Executor()\n    v := must(ex.submit_result(four).recv())\n    ex.shutdown()\n    return x + v\n",
    ),
    ("sleep", "    time.sleep_ms(1)\n    return x + 4\n"),
    ("nested_write", "    q.write(add4)\n    return x + 4\n"),
];

/// The native callback that holds the operation.
struct Kind {
    name: &'static str,
    box_ty: &'static str,
    box_ctor: &'static str,
    /// one round of `caller`, at 8 spaces
    call: &'static str,
    result: &'static str,
    /// how many times one round runs `body`
    per_round: usize,
}

const KINDS: [Kind; 6] = [
    Kind {
        name: "update",
        box_ty: "Shared[int]",
        box_ctor: "Shared(0)",
        call: "        b.update(fn(x: int) -> int: body(x, q))\n",
        result: "b.get()",
        per_round: 1,
    },
    Kind {
        name: "write",
        box_ty: "RwShared[int]",
        box_ctor: "RwShared(0)",
        call: "        b.write(fn(x: int) -> int: body(x, q))\n",
        result: "b.get()",
        per_round: 1,
    },
    Kind {
        name: "read",
        box_ty: "RwShared[int]",
        box_ctor: "RwShared(0)",
        call: "        t.add(b.read(fn(x: int) -> int: body(x, q)))\n",
        result: "t.load()",
        per_round: 1,
    },
    Kind {
        name: "map",
        box_ty: "RwShared[int]",
        box_ctor: "RwShared(0)",
        call: "        t.add([0].map(fn(x: int) -> int: body(x, q))[0])\n",
        result: "t.load()",
        per_round: 1,
    },
    Kind {
        name: "defer",
        box_ty: "RwShared[int]",
        box_ctor: "RwShared(0)",
        call: "        via_defer(t, q)\n",
        result: "t.load()",
        per_round: 1,
    },
    Kind {
        name: "sort_key",
        box_ty: "RwShared[int]",
        box_ctor: "RwShared(0)",
        call: "        ys := [0, 0]\n        ys.sort_by_key(fn(x: int) -> int: t.add(body(x, q)))\n",
        result: "t.load()",
        per_round: 2,
    },
];

const CALLERS: [usize; 3] = [1, 3, 8];

const PRELUDE: &str = "import std.concurrency
import std.time
fn one(c: AtomicInt):
    c.add(1)
fn give(ch: Channel[int]):
    ch.send(4)
fn four() -> int:
    return 4
fn add4(y: int) -> int:
    return y + 4
fn must(r: int!) -> int:
    match r:
        ?v: return v
        !e: panic(e.message())
";

/// One grid program and the value it must print.
fn program(op: &str, kind: &Kind, callers: usize) -> (String, String) {
    let driver = if callers == 1 {
        "caller(b, t, q)\n".to_string()
    } else {
        format!("parallel:\n    for _ in 0..{callers}:\n        spawn caller(b, t, q)\n")
    };
    let src = format!(
        "{PRELUDE}fn body(x: int, q: RwShared[int]) -> int:\n{op}\
         fn via_defer(t: AtomicInt, q: RwShared[int]):\n    defer:\n        t.add(body(0, q))\n\
         fn caller(b: {ty}, t: AtomicInt, q: RwShared[int]):\n    for _ in 0..{ROUNDS}:\n{call}\
         b := {ctor}\nt := AtomicInt(0)\nq := RwShared(0)\n{driver}print({result})\n",
        ty = kind.box_ty,
        call = kind.call,
        ctor = kind.box_ctor,
        result = kind.result,
    );
    (src, (callers * ROUNDS * kind.per_round * 4).to_string())
}

/// Runs every `(name, src, want)` cell at every worker count, four cells at a time, and returns
/// the cells that missed.
fn misses(cells: &[(String, String, String)]) -> Vec<String> {
    let next = AtomicUsize::new(0);
    let red = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((name, src, want)) = cells.get(i / THREADS.len()) else {
                        return;
                    };
                    let t = THREADS[i % THREADS.len()];
                    if let Some(got) = miss(src, t, want) {
                        red.lock()
                            .unwrap()
                            .push(format!("{name}/T={t}: {got}; expected {want:?}"));
                    }
                }
            });
        }
    });
    let mut red = red.into_inner().unwrap();
    red.sort();
    red
}

#[test]
fn grid_every_op_in_every_callback_from_every_caller_shape() {
    let mut cells = Vec::new();
    for (op_name, op) in OPS {
        for kind in &KINDS {
            for callers in CALLERS {
                let (src, want) = program(op, kind, callers);
                cells.push((format!("{op_name}/{}/{callers}", kind.name), src, want));
            }
        }
    }
    let red = misses(&cells);
    assert!(
        red.is_empty(),
        "{} red cell runs:\n{}",
        red.len(),
        red.join("\n")
    );
}

const NEST: &str = "import std.concurrency
fn one(c: AtomicInt):
    c.add(1)
fn body(x: int) -> int:
    c := AtomicInt(0)
    parallel:
        for _ in 0..4:
            spawn one(c)
    return x + c.load()
fn caller(r: RwShared[int]):
    for _ in 0..20:
        r.write(body)
";

/// `p1b`: `main` buries itself. The outer `parallel:` body calls `r.write(body)` beside 40 tasks.
fn p1b() -> String {
    format!(
        "{NEST}r := RwShared(0)\nparallel:\n    for _ in 0..40:\n        spawn caller(r)\n    \
         for _ in 0..20:\n        r.write(body)\nprint(r.get())\n"
    )
}

/// `p3`: no box. `b` holds a nursery inside `map`, then sends; `a` receives inside `map`.
fn p3() -> String {
    format!(
        "{NEST}fn a(ch: Channel[int], t: AtomicInt):\n    \
         t.store([1].map(fn(x: int) -> int: x + ch.recv())[0])\n\
         fn b(ch: Channel[int]):\n    ch.send([7].map(body)[0])\n\
         ch := Channel[int]()\nt := AtomicInt(0)\nparallel:\n    spawn b(ch)\n    spawn a(ch, t)\n\
         print(t.load())\n"
    )
}

/// `p5`: two tasks, each the owner of a nursery of 3 callers, each writing beside its children.
fn p5() -> String {
    format!(
        "{NEST}fn outer(r: RwShared[int]):\n    parallel:\n        for _ in 0..3:\n            \
         spawn caller(r)\n        for _ in 0..20:\n            r.write(body)\n\
         r := RwShared(0)\nparallel:\n    for _ in 0..2:\n        spawn outer(r)\nprint(r.get())\n"
    )
}

#[test]
fn named_cells_p1b_p3_p5() {
    let cells = vec![
        ("p1b".to_string(), p1b(), "3280".to_string()),
        ("p3".to_string(), p3(), "12".to_string()),
        ("p5".to_string(), p5(), "640".to_string()),
    ];
    let red = misses(&cells);
    assert!(
        red.is_empty(),
        "{} red cell runs:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// `i6n2` (W13-6), the program TICKET-125 and TICKET-127 false-faulted with a replacement worker.
const I6N2: &str = "fn main():
    out := Channel[int](0)
    parallel:
        for i in range(2):
            spawn:
                r := recover:
                    parallel:
                        spawn:
                            never := Channel[int](0)
                            never.recv()
                match r:
                    ?_: out.send(0)
                    !e: out.send(1)
        t := 0
        for i in range(2):
            t = t + out.recv()
        print(\"t {t}\")
main()
";

/// `i6n2` with each stuck nursery inside a `map` callback: its joiner cannot park.
const I6N2_IN_CALLBACK: &str = "fn inner(x: int) -> int:
    parallel:
        spawn:
            never := Channel[int](0)
            never.recv()
    return x
fn main():
    out := Channel[int](0)
    parallel:
        for i in range(2):
            spawn:
                r := recover: [1].map(inner)
                match r:
                    ?_: out.send(0)
                    !e: out.send(1)
        t := 0
        for i in range(2):
            t = t + out.recv()
        print(\"t {t}\")
main()
";

/// A genuine deadlock: the child writes the box its parent holds across the join.
const CHILD_WRITES_HELD_BOX: &str = "import std.concurrency
fn child(r: RwShared[int]):
    r.write(fn(y: int) -> int: y + 1)
fn hold(x: int, r: RwShared[int]) -> int:
    parallel:
        spawn child(r)
    return x
fn caller(r: RwShared[int]):
    r.write(fn(x: int) -> int: hold(x, r))
r := RwShared(0)
parallel:
    for _ in 0..3:
        spawn caller(r)
print(r.get())
";

/// What a sampled run did: `ok`, `deadlock`, `TIMEOUT`, or the raw result.
fn outcome(src: &str, threads: &str, want: &str) -> String {
    match run(src, threads) {
        None => "TIMEOUT".to_string(),
        Some((0, out, _)) if out.trim() == want => "ok".to_string(),
        Some((code, out, err)) if code != 0 && err.contains("deadlock") && !out.contains("t 2") => {
            "deadlock".to_string()
        }
        Some((code, out, err)) => format!("rc={code} stdout={out:?} stderr={err:?}"),
    }
}

/// The verdict sampling pass the owner asked for: every program below, `T245_RUNS` runs (default
/// 20) at T=1, 2, 4 and the default worker count. A finishing program must never report
/// `deadlock`; a deadlocked one must never hang or finish. Run it on the debug AND the release
/// binary: `cargo test --test callback_nursery_grid -- --ignored sample_` with and without
/// `--release`.
#[test]
#[ignore = "sampling pass: run with -- --ignored sample_"]
fn sample_verdicts_at_every_worker_count() {
    let runs: usize = std::env::var("T245_RUNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let (grid_src, grid_want) = program(OPS[0].1, &KINDS[1], 3);
    let programs = [
        ("i6n2", I6N2.to_string(), "deadlock", ""),
        (
            "i6n2_in_callback",
            I6N2_IN_CALLBACK.to_string(),
            "deadlock",
            "",
        ),
        (
            "child_writes_held_box",
            CHILD_WRITES_HELD_BOX.to_string(),
            "deadlock",
            "",
        ),
        ("nursery/write/3", grid_src, "ok", grid_want.as_str()),
        ("p1b", p1b(), "ok", "3280"),
        ("p3", p3(), "ok", "12"),
        ("p5", p5(), "ok", "640"),
    ];
    let mut red = Vec::new();
    for (name, src, verdict, want) in &programs {
        for t in ["1", "2", "4", "0"] {
            let mut bad = Vec::new();
            for _ in 0..runs {
                let got = outcome(src, t, want);
                if got != *verdict {
                    bad.push(got);
                }
            }
            println!(
                "{name} T={t}: {} of {runs} as expected ({verdict})",
                runs - bad.len()
            );
            if let Some(first) = bad.first() {
                red.push(format!(
                    "{name} T={t}: {} of {runs} runs missed {verdict:?}; first: {first}",
                    bad.len()
                ));
            }
        }
    }
    assert!(red.is_empty(), "{}", red.join("\n"));
}
