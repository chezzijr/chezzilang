//! TICKET-235 (wave 16 Family 2, "Blocking"): every spawned task starts in a frame, so a native
//! call that waits parks the same way whichever form spawned it. The grid is spawn form x waiting
//! op x outcome. Each cell runs a fresh process under a hard timeout at T=1, T=2 and T=0
//! (default), unseeded and under two scheduler seeds. A `.chz` test cannot assert that a Rust
//! panic is absent, so this runs the built binary and reads its stderr.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(10);

/// Runs `src` at `threads` workers with an optional `CHEZZI_SCHED_SEED`; `None` = still running
/// after `limit` (killed).
fn run_with(
    name: &str,
    src: &str,
    threads: &str,
    seed: Option<u32>,
    limit: Duration,
) -> Option<(i32, String, String)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t235-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
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

/// One native op that can wait.
struct WaitOp {
    name: &'static str,
    /// `w`'s parameter list; empty when the call needs no handle.
    param: &'static str,
    /// The argument that fills `param`.
    arg: &'static str,
    setup: &'static [&'static str],
    call: &'static str,
    /// The `parallel:` body lines that let the `done` outcome finish.
    driver: &'static [&'static str],
    done_stdout: &'static str,
}

const OPS: &[WaitOp] = &[
    WaitOp {
        name: "send_full",
        param: "ch: Channel[int]",
        arg: "ch",
        setup: &["ch := Channel[int](1)", "ch.send(0)"],
        call: "ch.send(1)",
        driver: &["time.sleep_ms(30)", "print(ch.recv() + ch.recv())"],
        done_stdout: "1\nend\n",
    },
    WaitOp {
        name: "send_rdv",
        param: "ch: Channel[int]",
        arg: "ch",
        setup: &["ch := Channel[int](0)"],
        call: "ch.send(1)",
        driver: &["time.sleep_ms(30)", "print(ch.recv())"],
        done_stdout: "1\nend\n",
    },
    WaitOp {
        name: "recv_empty",
        param: "ch: Channel[int]",
        arg: "ch",
        setup: &["ch := Channel[int](1)"],
        call: "ch.recv()",
        driver: &["time.sleep_ms(30)", "ch.send(1)", "print(\"sent\")"],
        done_stdout: "sent\nend\n",
    },
    WaitOp {
        name: "shared_update",
        param: "s: Shared[int]",
        arg: "s",
        setup: &["s := Shared[int](0)"],
        call: "s.update(slow)",
        driver: &["s.update(slow)", "print(\"upd\")"],
        done_stdout: "upd\nend\n",
    },
    WaitOp {
        name: "rw_write",
        param: "s: RwShared[int]",
        arg: "s",
        setup: &["s := RwShared[int](0)"],
        call: "s.write(slow)",
        driver: &["s.write(slow)", "print(\"wr\")"],
        done_stdout: "wr\nend\n",
    },
    WaitOp {
        name: "ex_shutdown",
        param: "ex: Executor",
        arg: "ex",
        setup: &["ex := Executor(1)", "ex.submit(fn(): time.sleep_ms(60))"],
        call: "ex.shutdown()",
        driver: &["print(\"body\")"],
        done_stdout: "body\nend\n",
    },
    WaitOp {
        name: "sleep",
        param: "",
        arg: "",
        setup: &[],
        call: "time.sleep_ms(40)",
        driver: &["print(\"body\")"],
        done_stdout: "body\nend\n",
    },
    WaitOp {
        name: "accept",
        param: "l: net.Listener",
        arg: "l",
        setup: &["l := net.listen(\"127.0.0.1:0\")?", "addr := l.addr()?"],
        call: "l.accept()",
        driver: &[
            "time.sleep_ms(30)",
            "c := net.connect(addr)?",
            "c.close()",
            "print(\"conn\")",
        ],
        done_stdout: "conn\nend\n",
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Form {
    /// `spawn <native call>`
    Native,
    /// `spawn w(<arg>)`, `w` a Chezzi fn whose body is the call
    Func,
    /// `spawn:` with the call on the next line
    Block,
}

const FORMS: &[(Form, &str)] = &[
    (Form::Native, "native"),
    (Form::Func, "fn"),
    (Form::Block, "block"),
];

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Done,
    Sibling,
    Cut,
}

/// The spawn statement of `form`, as lines indented by `pad`.
fn spawn_lines(op: &WaitOp, form: Form, pad: &str) -> String {
    match form {
        Form::Native => format!("{pad}spawn {}\n", op.call),
        Form::Func => format!("{pad}spawn w({})\n", op.arg),
        Form::Block => format!("{pad}spawn:\n{pad}    {}\n", op.call),
    }
}

fn program(op: &WaitOp, form: Form, outcome: Outcome) -> String {
    let mut s = String::from("import std.time\nimport std.concurrency\nimport std.net\n");
    s += "fn slow(x: int) -> int:\n    time.sleep_ms(30)\n    return x + 1\n";
    s += "fn boom():\n    time.sleep_ms(30)\n    panic(\"boom\")\n";
    s += &format!("fn w({}):\n    {}\n", op.param, op.call);
    if outcome == Outcome::Cut {
        s += &format!("fn job({}):\n    parallel:\n", op.param);
        s += &spawn_lines(op, form, "        ");
    }
    s += "fn main() -> None!:\n";
    for line in op.setup {
        s += &format!("    {line}\n");
    }
    match outcome {
        Outcome::Done => {
            s += "    parallel:\n";
            s += &spawn_lines(op, form, "        ");
            for line in op.driver {
                s += &format!("        {line}\n");
            }
            s += "    print(\"end\")\n";
        }
        Outcome::Sibling => {
            s += "    parallel:\n";
            s += &spawn_lines(op, form, "        ");
            s += "        spawn boom()\n";
            s += "    print(\"end\")\n";
        }
        Outcome::Cut => {
            s += "    ex2 := Executor(1)\n";
            s += &format!("    ex2.submit(fn(): job({}))\n", op.arg);
            s += "    time.sleep_ms(40)\n    ex2.shutdown_now()\n    print(\"cut\")\n";
        }
    }
    s += "r := main()\n";
    s
}

/// Why this run is wrong, or `None`.
fn judge(op: &WaitOp, outcome: Outcome, got: &Option<(i32, String, String)>) -> Option<String> {
    let Some((code, out, err)) = got else {
        return Some("hang (killed after 10s)".to_string());
    };
    let ok = !err.contains("panicked")
        && match outcome {
            Outcome::Done => *code == 0 && out == op.done_stdout,
            Outcome::Sibling => *code == 1 && err.contains("runtime error") && err.contains("boom"),
            Outcome::Cut => *code == 0 && out == "cut\n",
        };
    (!ok).then(|| format!("rc={code} stdout={out:?} stderr={err:?}"))
}

/// Run every (op, form) cell of `outcome` at three worker counts, unseeded and under seeds 1 and
/// 2, and fail once with every wrong run listed.
fn grid(outcome: Outcome, tag: &str) {
    let mut bad = Vec::new();
    for op in OPS {
        for &(form, form_name) in FORMS {
            let src = program(op, form, outcome);
            let name = format!("{}-{form_name}-{tag}", op.name);
            for t in ["1", "2", "0"] {
                for seed in [None, Some(1), Some(2)] {
                    let got = run_with(&name, &src, t, seed, LIMIT);
                    if let Some(why) = judge(op, outcome, &got) {
                        bad.push(format!(
                            "cell {name} at CHEZZI_THREADS={t} seed={seed:?}: {why}"
                        ));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} wrong run(s):\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn grid_done() {
    grid(Outcome::Done, "done");
}

#[test]
fn grid_cancelled_by_a_sibling_fault() {
    grid(Outcome::Sibling, "sibling");
}

#[test]
fn grid_cut_by_shutdown_now() {
    grid(Outcome::Cut, "cut");
}

/// A native fn VALUE as the head: `ex.submit(f)`, a local, and an indexed element.
/// `ex.submit(ch.recv)` (a native METHOD value) is not expressible: the checker rejects it.
#[test]
fn fn_value_heads_complete() {
    let src = "import std.time\nimport std.concurrency\nfn main():\n    ex := Executor(1)\n    ex.submit(time.now_ms)\n    ex.shutdown()\n    f := time.sleep_ms\n    fs := [time.sleep_ms]\n    parallel:\n        spawn f(20)\n        spawn fs[0](20)\n    print(\"end\")\nmain()\n";
    let mut bad = Vec::new();
    for t in ["1", "2", "0"] {
        for seed in [None, Some(1), Some(2)] {
            match run_with("fn-value", src, t, seed, LIMIT) {
                Some((0, out, err)) if out == "end\n" && !err.contains("panicked") => {}
                got => bad.push(format!("CHEZZI_THREADS={t} seed={seed:?}: {got:?}")),
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
