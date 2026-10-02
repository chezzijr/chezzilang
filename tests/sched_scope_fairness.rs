//! TICKET-199 (family S1): scheduler scope identity and fairness.
//! H1: a fiber-owned nursery's scope id is popped/reused while a worker still holds it
//! (`cancel_drain` after the owner joined) -> index out of bounds / wrong nursery drained.
//! H2: at `CHEZZI_THREADS=1` a cap-0 ping-pong keeps refilling `runnext`, so the ring never runs.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(src: &str, threads: &str, tag: &str) -> Option<(String, String, Option<i32>)> {
    run_env(src, threads, None, tag)
}

/// `run` with `CHEZZI_SCHED_SEED` set to `seed`, or removed on `None`.
fn run_env(
    src: &str,
    threads: &str,
    seed: Option<&str>,
    tag: &str,
) -> Option<(String, String, Option<i32>)> {
    let dir = std::env::temp_dir().join(format!("chz-t199-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("p.chz");
    std::fs::write(&path, src).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path).env("CHEZZI_THREADS", threads);
    match seed {
        Some(s) => cmd.env("CHEZZI_SCHED_SEED", s),
        None => cmd.env_remove("CHEZZI_SCHED_SEED"),
    };
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            let out = child.wait_with_output().unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            return Some((
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
                out.status.code(),
            ));
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

const PINGPONG_STARVES_INNER_SIBLING: &str = r#"import std.concurrency
fn main():
    c := Channel[int](0)
    stop := AtomicInt(0)
    parallel:
        spawn:
            while stop.load() == 0:
                c.send(1)
            c.close()
        spawn:
            parallel:
                spawn:
                    for _v in c:
                        pass
                spawn:
                    print("inner sibling ran")
                    stop.store(1)
    print("done")
main()
"#;

#[test]
fn runnext_pingpong_does_not_starve_the_ring_at_threads_1() {
    let r = run(PINGPONG_STARVES_INNER_SIBLING, "1", "h2");
    let (out, err, _) = r.expect("hang: no exit within 20s (inner sibling starved)");
    assert!(
        out.contains("inner sibling ran") && out.contains("done"),
        "stdout={out:?} stderr={err:?}"
    );
}

const NURSERY_IN_SPAWN_CHILD_FAULT: &str = r#"fn run():
    c := Channel[int](0)
    parallel:
        spawn:
            _r := recover:
                parallel:
                    spawn:
                        while true:
                            c.send(1)
                    spawn: panic("boom")
            c.close()
        spawn:
            for _v in c:
                pass
fn main():
    for _k in range(0, 50):
        run()
    print("done")
main()
"#;

#[test]
fn fiber_owned_nursery_scope_id_is_stable_under_child_fault_at_threads_4() {
    for round in 0..10 {
        let r = run(NURSERY_IN_SPAWN_CHILD_FAULT, "4", "h1");
        let (out, err, code) = r.expect("hang: no exit within 20s");
        assert!(
            out == "done\n" && code == Some(0),
            "round {round}: code={code:?} stdout={out:?} stderr={err:?}"
        );
    }
}

const TWO_DEEP_CHILD_FAULT: &str = r#"import std.concurrency
fn core(c: Channel[int], started: Channel[int]):
    parallel:
        spawn:
            while true:
                c.send(1)
        spawn:
            panic("boom")
fn outer(c: Channel[int], started: Channel[int]):
    parallel:
        spawn:
            parallel:
                spawn:
                    core(c, started)
        spawn:
            for _v in c:
                pass
fn run():
    c := Channel[int](0)
    started := Channel[int](1)
    _r := recover: outer(c, started)
fn main():
    for _k in range(0, 400):
        run()
    print("done")
main()
"#;

#[test]
fn fiber_owned_nursery_scope_id_is_stable_two_deep_child_fault_at_default_threads() {
    for round in 0..20 {
        let r = run(TWO_DEEP_CHILD_FAULT, "0", "h1d");
        let (out, err, code) = r.expect("hang: no exit within 20s");
        assert!(
            out == "done\n" && code == Some(0) && !err.contains("panicked at"),
            "round {round}: code={code:?} stdout={out:?} stderr={err:?}"
        );
    }
}

/// Indent every line of `piece` by `n` spaces.
fn indent(piece: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    piece.lines().map(|l| format!("{pad}{l}\n")).collect()
}

/// The H1 grid program for one (nesting, cancel source) cell, and its expected stdout.
fn h1_program(nest: &str, cancel: &str) -> (String, &'static str) {
    let second = match cancel {
        "child" => "panic(\"boom\")",
        "sibling" => "pass",
        "shutdown_now" => "started.send(1)\nwhile true:\n    pass",
        "exit" => "os.exit(0)",
        _ => unreachable!(),
    };
    let owner = match nest {
        "spawn" | "job" => "core(c, started)",
        "recover" => "_r := recover: core(c, started)\nc.close()",
        "two_deep" => "parallel:\n    spawn:\n        core(c, started)",
        _ => unreachable!(),
    };
    let recv = if cancel == "sibling" {
        "for _i in range(0, 20):\n    _v := c.recv()\npanic(\"boom\")"
    } else {
        "for _v in c:\n    pass"
    };
    let rounds = if cancel == "exit" { 1 } else { 50 };
    let run_body = if nest == "job" || cancel == "shutdown_now" {
        let stop = if cancel == "shutdown_now" {
            "_ := started.recv()\n_r2 := recover: ex.shutdown_now()"
        } else {
            "_r2 := recover: ex.shutdown()"
        };
        format!(
            "c := Channel[int](0)\nstarted := Channel[int](1)\nex := Executor()\nex.submit(fn(): outer(c, started))\n{stop}"
        )
    } else {
        "c := Channel[int](0)\nstarted := Channel[int](1)\n_r := recover: outer(c, started)"
            .to_string()
    };
    let mut src = String::from("import std.concurrency\nimport std.os\n");
    src += "fn core(c: Channel[int], started: Channel[int]):\n    parallel:\n        spawn:\n";
    src += "            while true:\n                c.send(1)\n        spawn:\n";
    src += &indent(second, 12);
    src += "fn outer(c: Channel[int], started: Channel[int]):\n    parallel:\n        spawn:\n";
    src += &indent(owner, 12);
    src += "        spawn:\n";
    src += &indent(recv, 12);
    src += "fn run():\n";
    src += &indent(&run_body, 4);
    src += &format!("fn main():\n    for _k in range(0, {rounds}):\n        run()\n");
    src += "    print(\"done\")\nmain()\n";
    let expected = if cancel == "exit" { "" } else { "done\n" };
    (src, expected)
}

#[test]
fn h1_scope_identity_grid_every_cell_exits_clean() {
    let mut failures = Vec::new();
    for nest in ["spawn", "recover", "job", "two_deep"] {
        for cancel in ["child", "sibling", "shutdown_now", "exit"] {
            let (src, expected) = h1_program(nest, cancel);
            for t in ["2", "4", "0"] {
                let cell = format!("{nest}-{cancel}-T{t}");
                for i in 0..20 {
                    match run(&src, t, &format!("h1g-{nest}-{cancel}-{t}")) {
                        None => {
                            failures.push(format!("{cell} run {i}: hang"));
                            break;
                        }
                        Some((out, err, code)) => {
                            if code != Some(0) || err.contains("panicked at") || out != expected {
                                failures.push(format!(
                                    "{cell} run {i}: code={code:?} stdout={out:?} stderr={err:?}"
                                ));
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} grid cells failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// `parallel:` with one `spawn:` per item.
fn par(items: &[&str]) -> String {
    let mut s = String::from("parallel:\n");
    for item in items {
        s += "    spawn:\n";
        s += &indent(item, 8);
    }
    s
}

/// The H2 grid program for one (placement, channel cap) cell: a sender ping-pongs a receiver
/// until the sibling `X` runs and stops it.
fn h2_program(place: &str, cap: &str) -> String {
    let send = "while stop.load() == 0:\n    c.send(1)\nc.close()";
    let recv = "for _v in c:\n    pass";
    let sib = "print(\"sibling ran\")\nstop.store(1)";
    let body = match place {
        "same" => par(&[send, recv, sib]),
        "outer_inner" => par(&[send, &par(&[recv, sib])]),
        "inner_outer" => par(&[&par(&[send, sib]), recv]),
        "two_inner" => par(&[&par(&[send]), &par(&[recv, sib])]),
        _ => unreachable!(),
    };
    let chan = match cap {
        "0" => "Channel[int](0)",
        "1" => "Channel[int](1)",
        "unb" => "Channel[int]()",
        _ => unreachable!(),
    };
    let mut src = String::from("import std.concurrency\nfn main():\n");
    src += &format!("    c := {chan}\n    stop := AtomicInt(0)\n");
    src += &indent(&body, 4);
    src += "    print(\"done\")\nmain()\n";
    src
}

#[test]
fn h2_fairness_grid_every_cell_finishes() {
    let mut failures = Vec::new();
    for place in ["same", "outer_inner", "inner_outer", "two_inner"] {
        for cap in ["0", "1", "unb"] {
            let src = h2_program(place, cap);
            for t in ["1", "2"] {
                for seed in [None, Some("1"), Some("2"), Some("3")] {
                    let s = seed.unwrap_or("none");
                    let cell = format!("{place}-cap{cap}-T{t}-s{s}");
                    for i in 0..3 {
                        match run_env(&src, t, seed, &format!("h2g-{place}-{cap}-{t}-{s}")) {
                            None => {
                                failures.push(format!("{cell} run {i}: hang"));
                                break;
                            }
                            Some((out, err, code)) => {
                                if code != Some(0) || out != "sibling ran\ndone\n" {
                                    failures.push(format!(
                                        "{cell} run {i}: code={code:?} stdout={out:?} stderr={err:?}"
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} grid cells failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
