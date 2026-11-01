//! An `extern` C call takes its permit steps from the wait table (`WaitSpec::Extern`): it keeps
//! its permit, the `chezzi-sysmon` watcher hands over the permit of a long call, and a halt that
//! arrives during the call is delivered at its return. One test per (C call, context); each runs
//! five programs at `CHEZZI_THREADS` 1, 2 and the default.
//!
//! Expected results are Go 1.27 run output of the same shapes, with cgo, at `GOMAXPROCS=1` and 4:
//! - `order`, `order_job`: a sibling that waits for a send made just before a long call prints
//!   `tick` before the call returns (sysmon retakes the P of a cgo call); after a short `labs` it
//!   may print either side. `order` puts the sibling in the caller's own nursery where there is
//!   one, so a worker in C at T=1 needs a replacement; `order_job` puts it in an Executor job, a
//!   thread that waits for the permit.
//! - `halt`: a nursery child panics at 50 ms. The owner in a long call prints no `after`; the
//!   halt comes at the call's return (Go ends the process at the panic, and no ancestor
//!   interrupts a running C call). An Executor job's fault ends in `t.get()`, by design.
//! - `dead`: a receive that nothing can satisfy after the call faults `deadlock`. Go with cgo
//!   linked never reports a deadlock, so this answer comes from the pure-Go twin.
//! - `live`: a party blocked on the caller during the call gets no false verdict.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;
#[path = "support/hang_deadline.rs"]
mod hang_deadline;

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const LIB_C: &str = "#include <unistd.h>
long slow_cb(long us, long (*f)(long)) { usleep((useconds_t)us); return f(us) + 1; }
long now_cb(long x, long (*f)(long)) { return f(x) + 1; }
";

/// The fixture library, built once per test process; `None` without a C compiler.
fn lib() -> Option<&'static PathBuf> {
    static LIB: OnceLock<Option<PathBuf>> = OnceLock::new();
    LIB.get_or_init(|| {
        if Command::new("cc").arg("--version").output().is_err() {
            eprintln!("SKIP extern_watch_grid: cc not found");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("chz-extern-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = dir.join("cbx.c");
        std::fs::write(&c, LIB_C).unwrap();
        let so = dir.join("libcbx.so");
        let st = Command::new("cc")
            .args(["-shared", "-fPIC", "-O2", "-o"])
            .arg(&so)
            .arg(&c)
            .status()
            .expect("spawn cc");
        assert!(st.success(), "cc failed to build the fixture .so");
        Some(so)
    })
    .as_ref()
}

const PRELUDE: &str = "import std.time
import Executor from std.concurrency

extern \"libc\":
    fn usleep(us: int) -> int
    fn labs(x: int) -> int

extern \"{LIB}\":
    fn slow_cb(us: int, f: fn(int) -> int) -> int
    fn now_cb(x: int, f: fn(int) -> int) -> int

fn boom():
    time.sleep_ms(50)
    panic(\"boom\")

fn ticker(g: Channel[int]):
    _ := g.recv()
    print(\"tick\")

fn late(h: Channel[int]):
    _ := h.recv()
    print(\"late\")

fn feed(d: Channel[int]):
    time.sleep_ms(50)
    d.send(1)

fn pass_():
    pass

fn runner(g: Channel[int], h: Channel[int], d: Channel[int]):
    _ := work(g, h, d)

fn runner_cb(g: Channel[int], h: Channel[int], d: Channel[int]):
    _ := [0].map(fn(i: int) -> int: work(g, h, d))

";

/// The body of `fn op` for each C call, and the value it returns.
fn op_body(call: &str) -> (&'static str, i64) {
    match call {
        "abs" => ("    return labs(-3)\n", 3),
        "usleep" => ("    _ := usleep(300000)\n    return 0\n", 0),
        "slow_cb" => (
            "    return slow_cb(300000, fn(n: int) -> int: n * 0 + 7)\n",
            8,
        ),
        "cb_long" => (
            "    return now_cb(1, fn(n: int) -> int: usleep(300000) + n)\n",
            2,
        ),
        other => panic!("no call {other}"),
    }
}

/// The body of `fn work`, which runs the call in its context, for each variant.
fn work_body(var: &str) -> &'static str {
    match var {
        "order" | "order_job" => {
            "    g.send(1)\n    v := op()\n    print(\"r {v}\")\n    return 0\n"
        }
        "halt" => {
            "    parallel:\n        spawn boom()\n        v := op()\n        print(\"after {v}\")\n    return 0\n"
        }
        "dead" => "    v := op()\n    print(\"r {v}\")\n    _ := d.recv()\n    return 0\n",
        "live" => {
            "    v := op()\n    print(\"r {v}\")\n    h.send(1)\n    _ := d.recv()\n    return 0\n"
        }
        other => panic!("no variant {other}"),
    }
}

/// The lines of `main` that reach `work`. `SIB` marks where a sibling in the caller's own nursery
/// goes.
fn ctx_lines(ctx: &str) -> &'static str {
    match ctx {
        "main" => "    _ := work(g, h, d)\n",
        "task" => "    parallel:\n        spawn runner(g, h, d)\nSIB",
        "body" => "    parallel:\nSIB        spawn pass_()\n        _ := work(g, h, d)\n",
        "job" => {
            "    jx := Executor(1)\n    t := jx.submit(fn() -> int: work(g, h, d))\n    match t.get():\n        ?_: pass\n        !e: print(\"e {e.message()}\")\n"
        }
        "cb" => "    parallel:\n        spawn runner_cb(g, h, d)\nSIB",
        other => panic!("no context {other}"),
    }
}

const VARIANTS: [&str; 5] = ["order", "order_job", "halt", "dead", "live"];

fn program(lib: &str, call: &str, ctx: &str, var: &str) -> String {
    let sib = var == "order" && matches!(ctx, "task" | "body" | "cb");
    let mut p = PRELUDE.replace("{LIB}", lib);
    p.push_str(&format!(
        "fn op() -> int:\n{}\nfn work(g: Channel[int], h: Channel[int], d: Channel[int]) -> int:\n{}\n",
        op_body(call).0,
        work_body(var)
    ));
    p.push_str("fn main():\n    g := Channel[int](1)\n    h := Channel[int](1)\n    d := Channel[int](1)\n    fx := Executor(1)\n");
    if var == "order_job" || (var == "order" && !sib) {
        p.push_str("    _ := fx.submit(fn(): ticker(g))\n");
    }
    if var == "live" {
        p.push_str("    _ := fx.submit(fn(): late(h))\n    _ := fx.submit(fn(): feed(d))\n");
    }
    p.push_str(&ctx_lines(ctx).replace("SIB", if sib { "        spawn ticker(g)\n" } else { "" }));
    p.push_str("    fx.shutdown()\n    print(\"end\")\nmain()\n");
    p
}

/// The stdouts a cell may print, its exit code, and a word its stderr must hold.
fn want(call: &str, ctx: &str, var: &str) -> (Vec<String>, i32, Option<&'static str>) {
    let v = op_body(call).1;
    let long = call != "abs";
    match var {
        "order" | "order_job" => {
            let mut outs = vec![format!("tick\nr {v}\nend\n")];
            if !long {
                outs.push(format!("r {v}\ntick\nend\n"));
            }
            (outs, 0, None)
        }
        "halt" => {
            let after = if long {
                String::new()
            } else {
                format!("after {v}\n")
            };
            if ctx == "job" {
                (vec![format!("{after}e boom\nend\n")], 0, None)
            } else {
                (vec![after], 1, Some("boom"))
            }
        }
        "dead" => (vec![format!("r {v}\n")], 1, Some("deadlock")),
        "live" => (vec![format!("r {v}\nlate\nend\n")], 0, None),
        other => panic!("no variant {other}"),
    }
}

fn cell(call: &str, ctx: &str) {
    let Some(lib) = lib() else { return };
    let dir = std::env::temp_dir().join(format!(
        "chz-extern-watch-{}-{call}-{ctx}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut misses = Vec::new();
    for var in VARIANTS {
        let path = dir.join(format!("{var}.chz"));
        std::fs::write(&path, program(lib.to_str().unwrap(), call, ctx, var)).unwrap();
        let (outs, code, word) = want(call, ctx, var);
        for threads in [Some("1"), Some("2"), None] {
            let got = hang_deadline::run_with_hang_deadline(&path, threads);
            let ok = got.as_ref().is_some_and(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                let err = String::from_utf8_lossy(&o.stderr);
                o.status.code() == Some(code)
                    && outs.iter().any(|w| *w == out)
                    && word.is_none_or(|w| err.contains(w))
            });
            if !ok {
                let shown = match &got {
                    None => "killed at the deadline".to_string(),
                    Some(o) => format!(
                        "status {:?}\n--stdout--\n{}--stderr--\n{}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    ),
                };
                misses.push(format!(
                    "{call} x {ctx} x {var} at CHEZZI_THREADS={threads:?}: want one of {outs:?} \
                     (exit {code}, stderr {word:?}); got {shown}"
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}

grid_tests!(CALLS x CTXS: [abs, usleep, slow_cb, cb_long]
    x [main, task, body, job, cb] => crate::cell);
