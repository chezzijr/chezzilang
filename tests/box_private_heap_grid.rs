//! TICKET-246 / M3.3 — a `Shared` / `RwShared` box holds one value by handle, behind one guard.
//!
//! 1. The four Chezzi grids run through the built (debug) binary at `CHEZZI_THREADS` 1, 2 and
//!    default: the box grid, the nested-call grid, the collection walk grid and the rule tests.
//! 2. The rules that need a process run one program each, with its stdout, stderr and exit
//!    status: `os.exit` inside a closure, a fault message that renders a box cycle, a write
//!    through `get()`, a guard + channel cycle.
//!
//! Every child runs under a timeout, so a hang is a failure, not a pass.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GRIDS: [&str; 4] = [
    "tests/chz/suites/box_grid_test.chz",
    "tests/chz/suites/box_nested_grid_test.chz",
    "tests/chz/suites/concurrent_collection_walk_grid_test.chz",
    "tests/chz/suites/box_rules_test.chz",
];

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// Run `chezzi <args>` at `threads` workers (`None` = default); kill it after `secs`.
fn chezzi(args: &[&str], threads: Option<&str>, secs: u64) -> Run {
    chezzi_env(args, threads, &[], secs)
}

/// [`chezzi`] with the extra environment `envs`.
fn chezzi_env(args: &[&str], threads: Option<&str>, envs: &[(&str, &str)], secs: u64) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CHEZZI_THREADS")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = threads {
        cmd.env("CHEZZI_THREADS", t);
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    // Drain both pipes on their own threads so a chatty child cannot block on a full pipe.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = out.read_to_string(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Run {
        stdout: out_t.join().unwrap(),
        stderr: err_t.join().unwrap(),
        code,
    }
}

#[test]
fn box_grids_pass_at_every_worker_count() {
    for threads in [Some("1"), Some("2"), None] {
        for grid in GRIDS {
            let r = chezzi(&["test", grid], threads, 240);
            let tail = r.stdout.lines().last().unwrap_or("").to_string();
            assert!(
                r.code == Some(0) && tail.contains(" 0 failed, 0 errored"),
                "{grid} at CHEZZI_THREADS={threads:?}: code {:?}, last line {tail:?}\n{}",
                r.code,
                r.stdout
                    .lines()
                    .filter(|l| !l.starts_with("PASS"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
    }
}

fn write_program(name: &str, src: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chezzi-t246-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.chz");
    std::fs::write(&path, src).unwrap();
    path
}

/// `os.exit` inside a box closure ends the process with that status; output before it is kept.
#[test]
fn os_exit_inside_a_box_closure_exits_with_its_status() {
    for (kind, call) in [("Shared", "update"), ("RwShared", "write")] {
        let src = format!(
            "import std.concurrency\nimport std.os\n\nfn leave(x: int) -> int:\n    print(\"inside\")\n    os.exit(3)\n    return x\n\ns := concurrency.{kind}(1)\nprint(\"before\")\ns.{call}(leave)\nprint(\"unreachable\")\n"
        );
        let path = write_program(&format!("exit-{kind}"), &src);
        for threads in [Some("1"), Some("2"), None] {
            let r = chezzi(&["run", path.to_str().unwrap()], threads, 20);
            assert_eq!(r.code, Some(3), "{kind} at {threads:?}: {}", r.stderr);
            assert_eq!(r.stdout, "before\ninside\n", "{kind} at {threads:?}");
        }
    }
}

/// A CHILD task of a nursery inside a box closure that reads the same box can never be served:
/// the closure holds the box until the nursery joins, and the nursery waits for the child. The
/// run reports a deadlock and exits 1; it never hangs. On the base binary (encoded store) the
/// child read the pre-update value and the program printed `ok 21`. The compiler cannot see this
/// shape (the box reaches the child through a parameter), so it is pinned and documented.
#[test]
fn a_child_task_reading_the_box_inside_its_own_closure_is_a_deadlock_fault_not_a_hang() {
    for (kind, call) in [("Shared", "update"), ("RwShared", "write")] {
        let src = format!(
            "import std.concurrency\n\nfn peek(w: concurrency.{kind}[int], out: concurrency.AtomicInt):\n    out.store(w.get())\n\nfn body(w: concurrency.{kind}[int], x: int) -> int:\n    out := concurrency.AtomicInt(0)\n    parallel:\n        spawn peek(w, out)\n    return x + out.load() + 1\n\nw := concurrency.{kind}(10)\nw.{call}(fn(x): body(w, x))\nprint(\"unreachable\")\n"
        );
        let path = write_program(&format!("child-{kind}"), &src);
        for threads in [Some("1"), Some("2"), None] {
            let r = chezzi(&["run", path.to_str().unwrap()], threads, 30);
            assert_eq!(r.code, Some(1), "{kind} at {threads:?}: {}", r.stderr);
            assert!(
                r.stderr.contains("update guard wait: deadlock")
                    && r.stderr.contains("main.chz:4:")
                    && r.stdout.is_empty(),
                "{kind} at {threads:?}: stdout {:?} stderr {}",
                r.stdout,
                r.stderr
            );
        }
    }
}

/// A child task's fault cuts its owner wherever the owner waits (TICKET-188), a box closure
/// included: the nurseries a task has open belong to the task, not to the heap its code runs in.
/// A `recover:` inside the closure sits inside the faulting nursery and must not catch (DEC-096).
/// With the caller's nurseries invisible inside the box, the plain wait hung and the `recover:`
/// cell printed `caught`.
#[test]
fn a_child_fault_cuts_its_owner_inside_a_box_closure() {
    let waits = [
        ("plain", "    print(ch.recv())"),
        (
            "recover",
            "    r := recover: ch.recv()\n    print(\"caught {r}\")",
        ),
        (
            "nursery",
            "    parallel:\n        spawn:\n            print(ch.recv())",
        ),
    ];
    let calls = [
        ("Shared", "w.update(park)"),
        ("Shared", "print(w.read(park))"),
        ("RwShared", "w.write(park)"),
        ("Shared", "w.update(fn(x): nested(x))"),
    ];
    for (wname, wait) in waits {
        for (i, (kind, call)) in calls.into_iter().enumerate() {
            let src = format!(
                "import std.concurrency\nch := Channel[int](0)\nstarted := Channel[int](1)\ninner := concurrency.Shared(0)\nfn park(x: int) -> int:\n    started.send(1)\n{wait}\n    return x\nfn nested(x: int) -> int:\n    inner.update(park)\n    return x\nw := concurrency.{kind}(0)\nparallel:\n    spawn:\n        _ := started.recv()\n        panic(\"boom\")\n    {call}\n    print(\"after\")\n"
            );
            let path = write_program(&format!("cut-{wname}-{i}"), &src);
            for threads in [Some("1"), Some("2"), None] {
                let r = chezzi(&["run", path.to_str().unwrap()], threads, 20);
                assert!(
                    r.code == Some(1) && r.stderr.contains("boom") && r.stdout.is_empty(),
                    "{wname} / {call} at {threads:?}: code {:?} stdout {:?} stderr {}",
                    r.code,
                    r.stdout,
                    r.stderr
                );
            }
        }
    }
}

/// The no-fault deadlock column: an outer `parallel:` whose sibling can never be fed, and a box
/// closure that waits forever. The expectation of each `update` / `write` cell is the BASE
/// binary's measured behaviour (`Shared.read` has no base; it follows `update`).
///
/// - `nursery`: the closure opens its own `parallel:` whose child waits. Base reports
///   `deadlock` and exits 1 at every worker count. The inner nursery must join the task's
///   enclosing sched (M3.3: the closure runs in the caller's context, so it is the caller's own).
/// - `direct`: the closure waits on the channel itself. Base hangs; the verdict declines. The
///   cell pins that no wrong verdict appears: the run never passes the call.
#[test]
fn a_deadlock_inside_a_box_closure_is_judged_as_on_the_caller() {
    let waits = [
        ("direct", "    _ := ch.recv()", false),
        (
            "nursery",
            "    parallel:\n        spawn:\n            _ := ch.recv()",
            true,
        ),
    ];
    let calls = [
        ("Shared", "w.update(park)"),
        ("Shared", "print(w.read(park))"),
        ("RwShared", "w.write(park)"),
        ("Shared", "w.update(fn(x): nested(x))"),
    ];
    for (wname, wait, faults) in waits {
        for (i, (kind, call)) in calls.into_iter().enumerate() {
            let src = format!(
                "import std.concurrency\nch := Channel[int](0)\nch2 := Channel[int](0)\ninner := concurrency.Shared(0)\nfn park(x: int) -> int:\n{wait}\n    return x\nfn nested(x: int) -> int:\n    inner.update(park)\n    return x\nw := concurrency.{kind}(0)\nparallel:\n    spawn:\n        _ := ch2.recv()\n    {call}\n    print(\"after\")\n"
            );
            let path = write_program(&format!("dl-{wname}-{i}"), &src);
            for threads in [Some("1"), Some("2"), None] {
                let r = chezzi(
                    &["run", path.to_str().unwrap()],
                    threads,
                    if faults { 20 } else { 2 },
                );
                let cell = format!(
                    "{wname} / {call} at {threads:?}: code {:?} stdout {:?} stderr {}",
                    r.code, r.stdout, r.stderr
                );
                assert!(r.stdout.is_empty(), "{cell}");
                if faults {
                    assert!(r.code == Some(1) && r.stderr.contains("deadlock"), "{cell}");
                } else {
                    assert_eq!(r.code, None, "{cell}");
                }
            }
        }
    }
}

/// A fault message is built by the `&self` renderer. An `Err` payload that holds a box holding
/// the payload must render with the cycle marker and exit 1; on the base binary it hung.
#[test]
fn a_fault_message_renders_a_box_cycle_and_terminates() {
    let decl = "import Shared from std.concurrency\nstruct N:\n    id: int\n    box: Shared[N?]\n    fn message(self) -> str:\n        return \"m\"\nfn f(n: N) -> int!N:\n    return !n\nb: Shared[N?] = Shared(None)\nn := N(1, b)\n";
    let cyc = write_program(
        "cycle",
        &format!("{decl}b.set(?n)\nprint(\"stored\")\nx := f(n)?\nprint(\"unreachable\")\n"),
    );
    let r = chezzi(&["run", cyc.to_str().unwrap()], None, 20);
    assert_eq!(r.code, Some(1), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "stored\n");
    assert!(
        r.stderr
            .contains("unhandled error: N(id=1, box=Shared(N(id=1, box=Shared(...))))"),
        "stderr: {}",
        r.stderr
    );
    // The no-cycle twin is unchanged from the base binary.
    let flat = write_program(
        "flat",
        &format!("{decl}print(\"stored\")\nx := f(n)?\nprint(\"unreachable\")\n"),
    );
    let r = chezzi(&["run", flat.to_str().unwrap()], None, 20);
    assert_eq!(r.code, Some(1));
    assert!(
        r.stderr
            .contains("unhandled error: N(id=1, box=Shared(None))"),
        "stderr: {}",
        r.stderr
    );
}

/// M3.3 (Q4): `get()` returns the live value, so a mutating call on it lands in the box and no
/// warning fires (Go `vet` has no such check; the TICKET-089 row is deleted).
#[test]
fn a_write_through_get_lands_and_does_not_warn() {
    let path = write_program(
        "temp",
        "import RwShared, Shared from std.concurrency\ns: Shared[List[int]] = Shared([1])\nw: RwShared[List[int]] = RwShared([1])\ns.get().push(1)\nw.get().push(1)\nprint(s.get(), w.get())\n",
    );
    let r = chezzi(&["run", path.to_str().unwrap()], None, 20);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "[1, 1] [1, 1]\n");
    assert!(!r.stderr.contains("warning"), "stderr: {}", r.stderr);
}

/// M3.3 — a real guard + channel cycle: task A holds the guard and waits on a channel that only
/// task B sends to, and B waits on the guard first. Go (`cycle/main.go`, go1.27.0) ends `fatal
/// error: all goroutines are asleep - deadlock!`; the run here ends with the deadlock verdict, exit
/// 1, at every worker count. The guard waiter counts as blocked only because every other party is
/// blocked too.
#[test]
fn a_guard_and_channel_cycle_gets_the_deadlock_verdict() {
    let path = write_program(
        "guard-chan-cycle",
        "import Shared from std.concurrency\n\nfn take(ch: Channel[int], held: Channel[bool], n: int) -> int:\n    held.send(true)\n    return n + ch.recv()\n\nfn holder(s: Shared[int], ch: Channel[int], held: Channel[bool]):\n    s.update(fn(n): take(ch, held, n))\n\nfn inc(n: int) -> int:\n    return n + 1\n\nfn sender(s: Shared[int], ch: Channel[int]):\n    s.update(inc)\n    ch.send(1)\n\ns := Shared(0)\nch := Channel[int](0)\nheld := Channel[bool](1)\nparallel:\n    spawn holder(s, ch, held)\n    spawn:\n        _ := held.recv()\n        sender(s, ch)\nprint(s.get())\n",
    );
    for threads in [Some("1"), Some("2"), None] {
        let r = chezzi(&["run", path.to_str().unwrap()], threads, 30);
        assert_eq!(r.code, Some(1), "at {threads:?}: stderr {}", r.stderr);
        assert!(
            r.stdout.is_empty() && r.stderr.contains("deadlock"),
            "at {threads:?}: stdout {:?} stderr {}",
            r.stdout,
            r.stderr
        );
    }
}

/// Run `chezzi run <entry>` at one worker; return (exit ok, stdout, max RSS in KB). `wait4` on
/// the pid, so no other child this test binary reaps contaminates the figure.
// `wait4` IS the reap (`waitpid` + rusage in one syscall).
#[allow(clippy::zombie_processes)]
fn run_max_rss(entry: &std::path::Path) -> (bool, String, i64) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(entry)
        .env("CHEZZI_THREADS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    let pid = child.id() as libc::pid_t;
    let mut out = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut out);
    let mut status: libc::c_int = 0;
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` is a child this test spawned and has not waited on; both out-params are valid.
    let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
    assert_eq!(ret, pid, "wait4({pid}) failed");
    (
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        out,
        rusage.ru_maxrss as i64,
    )
}

/// A box's value is freed when the last handle drops: 100000 boxes created and dropped peak at
/// most 2 MB above the same loop over `Atomic`. A box value that outlives its handle costs at least
/// 100000 values, far over the bound. Each side is the lowest of three
/// runs, so one noisy run cannot fail the cell.
#[test]
fn a_hundred_thousand_dropped_boxes_do_not_grow_memory() {
    const SLACK_KB: i64 = 2 * 1024;
    let rss = |kind: &str| -> i64 {
        let src = format!(
            "import std.concurrency\nfn churn():\n    for i in 0..100000:\n        b := concurrency.{kind}(i)\n        _ := b\nchurn()\nprint(\"done\")\n"
        );
        let path = write_program(&format!("rss-{kind}"), &src);
        (0..3)
            .map(|_| {
                let (ok, out, kb) = run_max_rss(&path);
                assert!(ok && out == "done\n", "{kind}: ok {ok} stdout {out:?}");
                kb
            })
            .min()
            .unwrap()
    };
    let base = rss("Atomic");
    for kind in ["Shared", "RwShared"] {
        let kb = rss(kind);
        eprintln!("maxrss: Atomic {base} KB, {kind} {kb} KB");
        assert!(
            kb <= base + SLACK_KB,
            "{kind}: {kb} KB against Atomic {base} KB (bound: +{SLACK_KB} KB)"
        );
    }
}

/// M1.7 — a box's state lives in the run's one heap, and the run's collector roots it while the
/// box is parked in its slot. Two tasks on another thread collect at every instruction
/// (`CHEZZI_GC_STRESS=1`) while `s` and `r` sit idle; the states read back intact. With the box
/// contexts left out of the root walk, a collection freed the states (a wrong value or a
/// `dangling GcRef` panic).
#[test]
fn box_state_survives_gc_from_another_thread() {
    let src = "import std.concurrency\n\nfn churn(n: int) -> int:\n    acc := 0\n    for i in 0..n:\n        xs := [i, i + 1, i + 2]\n        acc += xs.len()\n    return acc\n\ns := concurrency.Shared([[1, 2], [3, 4]])\nr := concurrency.RwShared({\"k\": [5, 6]})\ns.update(fn(x): x + [[7]])\nparallel:\n    spawn churn(3000)\n    spawn churn(3000)\nprint(s.get())\nprint(r.get())\n";
    let path = write_program("box-gc-stress", src);
    let r = chezzi_env(
        &["run", path.to_str().unwrap()],
        Some("2"),
        &[("CHEZZI_GC_STRESS", "1")],
        120,
    );
    assert_eq!(
        (r.code, r.stdout.as_str()),
        (Some(0), "[[1, 2], [3, 4], [7]]\n{'k': [5, 6]}\n"),
        "stderr: {}",
        r.stderr
    );
}
