//! TICKET-246 — a `Shared` / `RwShared` box keeps its value in a private heap.
//!
//! 1. The four Chezzi grids run through the built (debug) binary at `CHEZZI_THREADS` 1, 2 and
//!    default: the box grid, the nested-call grid, the collection walk grid and the rule tests.
//! 2. The rules that need a process run one program each, with its stdout, stderr and exit
//!    status: `os.exit` inside a closure, a fault message that renders a box cycle, the
//!    read-temporary warning.
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CHEZZI_THREADS")
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
            "import std.concurrency\nimport std.os\n\nfn leave(x: int) -> int:\n    print(\"inside\")\n    os.exit(3)\n    return x\n\ns := {kind}(1)\nprint(\"before\")\ns.{call}(leave)\nprint(\"unreachable\")\n"
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
            "import std.concurrency\n\nfn peek(w: {kind}[int], out: AtomicInt):\n    out.store(w.get())\n\nfn body(w: {kind}[int], x: int) -> int:\n    out := AtomicInt(0)\n    parallel:\n        spawn peek(w, out)\n    return x + out.load() + 1\n\nw := {kind}(10)\nw.{call}(fn(x): body(w, x))\nprint(\"unreachable\")\n"
        );
        let path = write_program(&format!("child-{kind}"), &src);
        for threads in [Some("1"), Some("2"), None] {
            let r = chezzi(&["run", path.to_str().unwrap()], threads, 30);
            assert_eq!(r.code, Some(1), "{kind} at {threads:?}: {}", r.stderr);
            assert!(
                r.stderr.contains("deadlock") && r.stdout.is_empty(),
                "{kind} at {threads:?}: stdout {:?} stderr {}",
                r.stdout,
                r.stderr
            );
        }
    }
}

/// A fault message is built by the `&self` renderer. An `Err` payload that holds a box holding
/// the payload must render with the cycle marker and exit 1; on the base binary it hung.
#[test]
fn a_fault_message_renders_a_box_cycle_and_terminates() {
    let decl = "import std.concurrency\nstruct N:\n    id: int\n    box: Shared[N?]\n    fn message(self) -> str:\n        return \"m\"\nfn f(n: N) -> int!N:\n    return !n\nb: Shared[N?] = Shared(None)\nn := N(1, b)\n";
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

/// The read-temporary warning is unchanged: a mutating call on the copy `get()` returns warns,
/// the program still runs, and the box is unchanged.
#[test]
fn the_read_temporary_warning_is_unchanged() {
    let path = write_program(
        "temp",
        "import std.concurrency\ns: Shared[List[int]] = Shared([1])\nw: RwShared[List[int]] = RwShared([1])\ns.get().push(1)\nw.get().push(1)\nprint(s.get(), w.get())\n",
    );
    let r = chezzi(&["run", path.to_str().unwrap()], None, 20);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "[1] [1]\n");
    for want in [
        "'push' mutates the copy returned by 's.get()', so the write is discarded — write through 's.update(…)' instead",
        "'push' mutates the copy returned by 'w.get()', so the write is discarded — write through 'w.write(…)' instead",
    ] {
        assert!(r.stderr.contains(want), "stderr: {}", r.stderr);
    }
}
