//! TICKET-193: contended `Shared.update` must not be pathologically slow at small worker counts.
//! 6 tasks x 2000 `s.update(inc)` measured 0.03 s at T=1 but 15.6-19.6 s at T=2 on the release
//! binary; Go (`sync.Mutex`, GOMAXPROCS=2) does the same work in 0.007 s.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SRC: &str = "import std.concurrency
fn inc(n: int) -> int:
    return n + 1
fn main():
    s := Shared[int](0)
    parallel:
        for _ in range(6):
            spawn:
                for _ in range(2000):
                    s.update(inc)
    print(s.get())
main()
";

/// Runs `SRC` at `threads` workers; returns (stdout, wall time).
fn run_at(threads: &str) -> (String, Duration) {
    run_src(SRC, "su", threads, None)
}

/// Runs `src` at `threads` workers, with `CHEZZI_SCHED_SEED` set to `seed` (removed on `None`);
/// returns (stdout, wall time). Kills the run after 40 s.
fn run_src(src: &str, tag: &str, threads: &str, seed: Option<u32>) -> (String, Duration) {
    let dir = std::env::temp_dir().join(format!("chz-t193-{tag}-{threads}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("su.chz");
    std::fs::write(&path, src).expect("write program");
    let start = Instant::now();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path).env("CHEZZI_THREADS", threads);
    match seed {
        Some(n) => cmd.env("CHEZZI_SCHED_SEED", n.to_string()),
        None => cmd.env_remove("CHEZZI_SCHED_SEED"),
    };
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    while child.try_wait().expect("wait").is_none() && start.elapsed() < Duration::from_secs(40) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let out = child.wait_with_output().expect("output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        start.elapsed(),
    )
}

#[test]
fn contended_shared_update_is_fast_at_every_worker_count() {
    for threads in ["1", "2", "4", "0"] {
        let (out, took) = run_at(threads);
        assert_eq!(
            out.trim(),
            "12000",
            "CHEZZI_THREADS={threads}: wrong final value"
        );
        assert!(
            took < Duration::from_secs(3),
            "CHEZZI_THREADS={threads}: 12000 contended Shared.update took {took:?}, ceiling 3s"
        );
    }
}

/// TICKET-193 grid: op x contention x worker count. `Shared.update` and `RwShared.write` take the
/// update guard and run a closure (a callback preempt there gates the workers, DEC-141);
/// `RwShared.read` takes no guard; `Atomic.add` is a Mutex; `ConcurrentMap.set` takes the guard with
/// no closure. Every cell does 12000 ops in total and must print `12000`.
const GRID_OPS: [(&str, &str, &str, &str); 5] = [
    (
        "Shared.update",
        "s := Shared[int](0)",
        "s.update(inc)",
        "print(s.get())",
    ),
    (
        "RwShared.write",
        "s := RwShared[int](0)",
        "s.write(inc)",
        "print(s.get())",
    ),
    (
        "RwShared.read",
        "s := RwShared[int](7)\n    c := AtomicInt(0)",
        "c.add(s.read(peek) - 6)",
        "print(c.load())",
    ),
    (
        "Atomic.add",
        "s := Atomic[int](0)",
        "s.add(1)",
        "print(s.load())",
    ),
    (
        "ConcurrentMap.set",
        "m: ConcurrentMap[int, int] = ConcurrentMap(RwShared({}))\n    c := AtomicInt(0)",
        "m.set(c.add(1), 1)",
        "print(m.len())",
    ),
];

/// Measured 2026-10-01 with the fix: release max 0.06 s, debug max 0.40 s. Base release: 16-22 s at
/// T=2 for the two closure ops with 6 tasks.
const GRID_CEILING: Duration = if cfg!(debug_assertions) {
    Duration::from_secs(3)
} else {
    Duration::from_millis(500)
};

fn grid_src(init: &str, body: &str, fin: &str, tasks: usize) -> String {
    let per = 12000 / tasks;
    [
        "import std.concurrency".to_string(),
        "import ConcurrentMap from std.concurrency.collection".to_string(),
        "fn inc(n: int) -> int:".to_string(),
        "    return n + 1".to_string(),
        "fn peek(n: int) -> int:".to_string(),
        "    return n".to_string(),
        "fn main():".to_string(),
        format!("    {init}"),
        "    parallel:".to_string(),
        format!("        for _ in range({tasks}):"),
        "            spawn:".to_string(),
        format!("                for _ in range({per}):"),
        format!("                    {body}"),
        format!("    {fin}"),
        "main()".to_string(),
        String::new(),
    ]
    .join("\n")
}

#[test]
fn contended_guard_ops_grid_is_fast_at_every_worker_count() {
    let mut red = Vec::new();
    for (op, init, body, fin) in GRID_OPS {
        for tasks in [6, 2] {
            let src = grid_src(init, body, fin, tasks);
            let tag = format!("{}-{tasks}", op.replace('.', "_"));
            for threads in ["1", "2", "4", "0"] {
                let (out, took) = run_src(&src, &tag, threads, None);
                if out.trim() != "12000" || took >= GRID_CEILING {
                    red.push(format!(
                        "{op} x{tasks} T={threads}: printed {:?} in {took:?}",
                        out.trim()
                    ));
                }
            }
        }
    }
    assert!(
        red.is_empty(),
        "cells over {GRID_CEILING:?} or with a wrong value:\n{}",
        red.join("\n")
    );
}

/// TICKET-194 (G1): two `Shared` boxes, 6 tasks x 500 iterations of `a.update` then `b.update`.
/// Seeded mode forces a `slice_end_in_place` inside about 25% of the update closures; a waiter whose
/// first guard wait held its width permit then sat out the full 5 ms stage 1. Base debug at T=1:
/// 9.24-9.76 s (0.04 s unseeded).
const TWO_BOX_SRC: &str = "import std.concurrency
a := Shared[int](0)
b := Shared[int](0)
parallel:
    for _ in 0..6:
        spawn:
            for _ in 0..500:
                a.update(fn(x: int) -> int: x + 1)
                b.update(fn(x: int) -> int: x + 1)
print(a.get(), b.get())
";

#[test]
fn seeded_two_box_update_is_fast_at_every_worker_count() {
    let mut red = Vec::new();
    for seed in 1..=4 {
        for threads in ["1", "2", "4", "0"] {
            let tag = format!("two-box-{seed}");
            let (out, took) = run_src(TWO_BOX_SRC, &tag, threads, Some(seed));
            if out.trim() != "3000 3000" || took >= GRID_CEILING {
                red.push(format!(
                    "seed={seed} T={threads}: printed {:?} in {took:?}",
                    out.trim()
                ));
            }
        }
    }
    assert!(
        red.is_empty(),
        "cells over {GRID_CEILING:?} or with a wrong value:\n{}",
        red.join("\n")
    );
}
