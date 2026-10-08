//! TICKET-188 (W17 Family B): a child task's fault must reach its blocked owner at every wait, the
//! way cancellation does. One row per `WaitSpec` row of `TABLE` in `src/vm/block.rs` (plus a CPU
//! loop), times every owner kind, times every context. `vm::block::tests::
//! every_wait_spec_has_an_owner_fault_grid_row` fails when a spec has no row here.
//!
//! "Cut" means no line after the op prints; a self-ending op's own satisfier fires at
//! `DEADLINE_MS`, well past the child's 50 ms fault. CPython `asyncio.TaskGroup` cancels the body at
//! once; Go's `panic` ends the process at 50 ms. A `defer` body is never cut (it is uncancellable).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Runs `src` at `threads` workers; `None` = still running after `limit` (killed). With `stdin`,
/// stdin is a pipe that receives one line, [`FED_LINE`], at `DEADLINE_MS` and is then held open.
fn run_with(
    name: &str,
    src: &str,
    threads: &str,
    seed: Option<u32>,
    stdin: bool,
    limit: Duration,
) -> Option<(i32, String, String)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t188-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    } else {
        cmd.env_remove("CHEZZI_SCHED_SEED");
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    // A blocking stdin read has no checkpoint until it returns (`docs/stdlib.md` "Blocking calls
    // cannot be interrupted"; Go's `os.Stdin.Read` and CPython's `input()` are not cancelled either),
    // so the Stdin row is satisfied at `DEADLINE_MS` and the cut is judged AT the return.
    let mut held_stdin = child.stdin.take();
    let mut fed = false;
    let feed_at = Duration::from_millis(DEADLINE_MS.parse().expect("DEADLINE_MS is an integer"));
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if !fed
            && start.elapsed() >= feed_at
            && let Some(w) = held_stdin.as_mut()
        {
            // The child may already be gone (a cut), so a broken pipe is not a failure.
            let _ = w.write_all(format!("{FED_LINE}\n").as_bytes());
            let _ = w.flush();
            fed = true;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(held_stdin);
    let mut out = String::new();
    let mut err = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| (s.code().unwrap_or(-1), out, err))
}

/// The deadline of every self-ending op, in ms: it only has to outlast the child's 50 ms fault.
const DEADLINE_MS: &str = "600";

/// The one line the Stdin row's pipe receives at `DEADLINE_MS`. A cut owner never prints it.
const FED_LINE: &str = "fedline";

/// One blocking op: `spec` is its `TABLE` name in `src/vm/block.rs` (or `"Cpu"`), `body` the text
/// of `fn op`, `prelude` top-level setup, `self_ends` = the op returns on its own deadline.
struct Row {
    spec: &'static str,
    body: &'static str,
    prelude: &'static str,
    self_ends: bool,
    stdin: bool,
}

const SOCK_PRELUDE: &str = "ln := must(net.listen(\"127.0.0.1:0\"))
c := must(net.connect(must(ln.addr())))
peer := must(ln.accept())";
const HTTP_PRELUDE: &str = "hl := must(net.listen(\"127.0.0.1:0\"))
url := \"http://\" + must(hl.addr()) + \"/\"";
const GUARD_PRELUDE: &str = "hx := Executor()
hx.submit(holder)
time.sleep_ms(20)";

/// The defaults every row overrides.
const R: Row = Row {
    spec: "",
    body: "",
    prelude: "",
    self_ends: false,
    stdin: false,
};

fn rows() -> Vec<Row> {
    vec![
        Row {
            spec: "Recv",
            body: "    print(ch.recv())",
            ..R
        },
        Row {
            spec: "Timer",
            body: "    _ := time.timer(DL).recv()",
            self_ends: true,
            ..R
        },
        Row {
            spec: "Send",
            body: "    fc.send(1)\n    fc.send(2)",
            ..R
        },
        Row {
            spec: "Wait",
            body: "    wait:\n        v := ch.recv(): print(v)",
            ..R
        },
        Row {
            spec: "Wait+send",
            body: "    fc.send(1)\n    wait:\n        fc.send(2): print(\"sent\")",
            ..R
        },
        Row {
            spec: "Wait+deadline",
            body: "    wait:\n        v := ch.recv(): print(v)\n        _ := time.timer(DL).recv(): print(\"t\")",
            self_ends: true,
            ..R
        },
        Row {
            spec: "Wait+deadline+send",
            body: "    fc.send(1)\n    wait:\n        fc.send(2): print(\"sent\")\n        _ := time.timer(DL).recv(): print(\"t\")",
            self_ends: true,
            ..R
        },
        Row {
            spec: "Sleep",
            body: "    time.sleep_ms(DL)",
            self_ends: true,
            ..R
        },
        Row {
            spec: "Offload",
            body: "    print(show(request.get(url, DL)))",
            prelude: HTTP_PRELUDE,
            self_ends: true,
            ..R
        },
        Row {
            spec: "Stdin",
            body: "    print(io.input(\"\"))",
            self_ends: true,
            stdin: true,
            ..R
        },
        Row {
            spec: "Socket",
            body: "    print(show(peer.read(10, DL)))",
            prelude: SOCK_PRELUDE,
            self_ends: true,
            ..R
        },
        Row {
            spec: "Socket",
            body: "    print(show(ln.accept(DL)))",
            prelude: SOCK_PRELUDE,
            self_ends: true,
            ..R
        },
        Row {
            spec: "Socket",
            body: "    print(show(c.write(\"x\".repeat(8388608))))",
            prelude: SOCK_PRELUDE,
            ..R
        },
        Row {
            spec: "Connect",
            body: "    print(show(net.connect(\"10.255.255.1:9\")))",
            ..R
        },
        Row {
            spec: "Guard",
            body: "    g.update(inc)",
            prelude: GUARD_PRELUDE,
            ..R
        },
        Row {
            spec: "Join",
            body: "    ex2 := Executor()\n    ex2.submit(recv_job)\n    ex2.shutdown()",
            ..R
        },
        Row {
            spec: "Nursery",
            body: "    parallel:\n        spawn:\n            print(ch.recv())",
            ..R
        },
        Row {
            spec: "Cpu",
            body: "    while true:\n        pass",
            ..R
        },
    ]
}

const PROGRAM: &str = "import std.concurrency
import std.time
import std.io
import std.net
import std.request
ch := Channel[int](0)
fc := Channel[int](1)
s := Shared[int](0)
g := Shared[int](0)
sink := Channel[int](0)
{PRELUDE}
fn must[T](r: Result[T]) -> T:
    match r:
        Ok(v): return v
        Err(e): panic(e.message())
fn show[T](r: Result[T]) -> str:
    match r:
        Ok(_): return \"ok\"
        Err(e): return \"err \" + e.message()
fn op(n: int) -> int:
{OP}
    return 0
fn gen() -> Iterator[int]:
    yield op(0)
fn viadefer():
    defer:
        _ := op(0)
    pass
fn bump(v: int) -> int:
    return v + op(0)
fn inc(v: int) -> int:
    return v + 1
fn hold3s(v: int) -> int:
    time.sleep_ms(3000)
    return v
fn holder() -> None:
    g.update(hold3s)
fn recv_job() -> None:
    print(ch.recv())
{OWNER}";

/// The child every owner spawns: it faults at 50 ms.
const CHILD: &str = "spawn:\n    time.sleep_ms(50)\n    panic(\"boom\")";

/// Contexts: the `{CTX}` text that reaches `op`.
const CONTEXTS: [(&str, &str); 5] = [
    ("plain", "_ := op(0)"),
    ("callback", "_ := [0].map(op)"),
    ("generator", "for _ in gen():\n    pass"),
    ("update", "s.update(bump)"),
    ("defer", "viadefer()"),
];

const OWNERS: [&str; 5] = [
    "main_body",
    "fn_in_main",
    "spawned_owner",
    "fiber_nested",
    "executor_job",
];

/// `text` with every line indented by `n` spaces.
fn indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines().map(|l| format!("{pad}{l}\n")).collect()
}

/// The owner text: spawn the faulting child, run the op in its context, then print `after`.
fn owner_text(ctx: &str) -> String {
    format!("{CHILD}\n{ctx}\nprint(\"after\")")
}

/// The top-level program text for an owner kind.
fn owner_src(kind: &str, ctx: &str) -> String {
    let body = owner_text(ctx);
    match kind {
        "main_body" => format!("parallel:\n{}", indent(&body, 4)),
        "fn_in_main" => format!("fn owner():\n{}owner()\n", indent(&body, 4)),
        "spawned_owner" => format!(
            "fn owner():\n{}{}parallel:\n    spawn:\n        owner()\n    spawn:\n        \
             time.sleep_ms(300)\n        sink.send(1)\n        print(\"sibling sent\")\n",
            indent(&body, 4),
            indent("print(sink.recv())", 4),
        ),
        "fiber_nested" => format!(
            "parallel:\n    spawn:\n        parallel:\n{}",
            indent(&body, 12)
        ),
        "executor_job" => format!(
            "fn owner_job() -> None:\n{}ex := Executor()\nex.submit(owner_job)\nex.shutdown()\n",
            indent(&body, 4)
        ),
        _ => unreachable!("owner kind {kind}"),
    }
}

/// What a cell must do.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// non-zero exit, `boom` on stderr, neither `after` nor `sibling sent` on stdout
    Cut,
    /// non-zero exit, `boom` on stderr, `after` on stdout (the op was never cut)
    NotCut,
}

struct Cell {
    name: String,
    src: String,
    stdin: bool,
    /// the `plain` context: also runs at T=1, T=2 and seeds
    plain: bool,
    expect: Expect,
}

fn cells() -> Vec<Cell> {
    let mut v = Vec::new();
    for (i, row) in rows().into_iter().enumerate() {
        for (cname, ctx) in CONTEXTS {
            // A `defer` is never cut, so only an op that ends on its own can finish there.
            if cname == "defer" && !row.self_ends {
                continue;
            }
            for kind in OWNERS {
                let expect = if cname == "defer" {
                    Expect::NotCut
                } else {
                    Expect::Cut
                };
                let src = PROGRAM
                    .replace("{PRELUDE}", row.prelude)
                    .replace("{OP}", &row.body.replace("DL", DEADLINE_MS))
                    .replace("{OWNER}", &owner_src(kind, ctx));
                v.push(Cell {
                    name: format!("{}#{i}/{kind}/{cname}", row.spec),
                    src,
                    stdin: row.stdin,
                    plain: cname == "plain",
                    expect,
                });
            }
        }
    }
    v
}

/// Runs one cell at T=0, and a `plain` cell also at T=1, T=2 and seeds 1..2 at T=1/T=2; returns
/// one line per miss.
fn check_cell(c: &Cell) -> Vec<String> {
    let mut runs: Vec<(&str, Option<u32>)> = vec![("0", None)];
    if c.plain {
        runs.extend([("1", None), ("2", None)]);
        for t in ["1", "2"] {
            for s in 1..=2 {
                runs.push((t, Some(s)));
            }
        }
    }
    let tag: String = c
        .name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut misses = Vec::new();
    for (t, seed) in runs {
        let start = Instant::now();
        let got = run_with(&tag, &c.src, t, seed, c.stdin, Duration::from_secs(10));
        let ms = start.elapsed().as_millis();
        let ok = match (&got, c.expect) {
            (Some((code, out, err)), Expect::Cut) => {
                *code != 0
                    && err.contains("boom")
                    && !out.contains("after")
                    && !out.contains("sibling sent")
                    && !out.contains(FED_LINE)
            }
            (Some((code, out, err)), Expect::NotCut) => {
                *code != 0 && err.contains("boom") && out.contains("after")
            }
            (None, _) => false,
        };
        if !ok {
            let what = match got {
                None => "hang (killed after 10s)".to_string(),
                Some((code, out, err)) => format!(
                    "rc={code} stdout={out:?} stderr={:?} after {ms} ms",
                    err.chars().take(160).collect::<String>()
                ),
            };
            misses.push(format!(
                "{} T={t} seed={seed:?}: {what}; expected {:?}",
                c.name, c.expect
            ));
        }
    }
    misses
}

/// Every blocking op × every owner kind × every context, at T=0; the `plain` context also at T=1,
/// T=2 and seeds. A child's fault must cut its owner wherever it waits.
#[test]
fn owner_fault_grid_every_op_owner_and_context() {
    let cells = cells();
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cells.get(i) else { break };
                    let m = check_cell(c);
                    misses.lock().unwrap().extend(m);
                }
            });
        }
    });
    let mut misses = misses.into_inner().unwrap();
    misses.sort();
    assert!(
        misses.is_empty(),
        "{} misses over {} cells:\n{}",
        misses.len(),
        cells.len(),
        misses.join("\n")
    );
}
