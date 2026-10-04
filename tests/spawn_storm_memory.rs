//! TICKET-213 — a spawn storm over a big aggregate global must not hold one copy of the global per
//! task.
//!
//! TICKET-208 rebuilt the module snapshot at EVERY `spawn` whenever a global held a mutable
//! aggregate, and each task keeps its snapshot until it finishes. At one worker every spawned task
//! is alive at the join, so 2000 spawns over a 100000-item `List[int]` held 2000 snapshots: the
//! release binary was killed at a 6 GB cap (base `fcbe2596`: max RSS 39 MB). The view is now rebuilt
//! only when something a global reaches was mutated since the cached build (`Heap::view_epoch`),
//! so the storm shares ONE snapshot.
//!
//! The gate is max RSS, read from `wait4`'s rusage for this one child: an absolute bound on memory,
//! no clock sample. Sized so the pre-fix binary is far over the bound in release and in debug:
//! 120 tasks each holding a snapshot of a 100000-item list (measured pre-fix, debug: 11.7 MB per
//! task).

#![cfg(unix)]

use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

const STORM: &str = "g: List[int] = []
for i in range(100000):
    g.push(i)
fn work(i: int) -> int:
    return i + g.len()
parallel:
    for i in range(120):
        spawn work(i)
print(\"done\")
";

/// The bound. Measured in `docs/benchmarks.md` (TICKET-213): one snapshot shared by every task
/// stays under a tenth of this; one snapshot per task is several times over it.
const MAX_RSS_KB: i64 = 300 * 1024;

/// Run `chezzi run <entry>` at `threads` workers; return (exit status, stdout, max RSS in KB).
// `wait4` IS the reap (`waitpid` + rusage in one syscall).
#[allow(clippy::zombie_processes)]
fn run_max_rss(entry: &std::path::Path, threads: &str) -> (std::process::ExitStatus, String, i64) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(entry)
        .env("CHEZZI_THREADS", threads)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    let pid = child.id() as libc::pid_t;
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout piped")
        .read_to_string(&mut out)
        .expect("read stdout");
    let mut status: libc::c_int = 0;
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` is a child this test spawned and has not waited on; both out-params are valid.
    let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
    assert_eq!(ret, pid, "wait4({pid}) failed");
    (
        std::process::ExitStatus::from_raw(status),
        out,
        rusage.ru_maxrss as i64,
    )
}

#[test]
fn a_spawn_storm_over_a_big_global_shares_one_snapshot() {
    let dir = std::env::temp_dir().join(format!("chezzi_t213_storm_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("storm.chz");
    std::fs::write(&entry, STORM).unwrap();
    // One worker is the worst case (every task is alive at the join); two workers must hold the
    // same bound. The default count is not gated: every running worker rebuilds its own copy of
    // the global, so max RSS there scales with the core count (base `fcbe2596`, release, 28 cores:
    // 521 MB), which is not this regression.
    for threads in ["1", "2"] {
        let (status, out, max_rss_kb) = run_max_rss(&entry, threads);
        assert!(
            status.success(),
            "T={threads}: the storm must exit 0 (status {status:?}, max RSS {max_rss_kb} KB)"
        );
        assert_eq!(out, "done\n", "T={threads}: wrong output");
        assert!(
            max_rss_kb < MAX_RSS_KB,
            "T={threads}: max RSS {max_rss_kb} KB, bound {MAX_RSS_KB} KB: each task holds its own \
             copy of the global"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
