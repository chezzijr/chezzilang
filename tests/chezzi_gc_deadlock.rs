//! GC core-graph deadlock gates — **deliberately their own integration target**.
//!
//! These spawn 40 subprocesses each, at `CHEZZI_THREADS=4`/`8`. Co-located in
//! `tests/chezzi_threads_cli.rs` they starve that file's wall-clock RATIO tests, which run
//! concurrently under `RUST_TEST_THREADS`: measured on merged `main`,
//! `threads_one_serializes_nested_eager_parallel_tasks` failed at ratio **5.28 against its 5.5 floor**
//! with these two present, and passes 3/3 alone. Cargo runs separate integration targets
//! sequentially, so splitting them is the fix — the same remedy, for the same reason, that put
//! `tests/chezzi_threads_sys_time.rs` in its own target.

use std::process::Command;

/// The GC's mark walk must never hold two core locks at once.
///
/// `collect_core_gcrefs` used to lock a core's payload and recurse straight into a NESTED core's lock
/// while still holding the first, and `Heap::children` held the outer core's guard across the whole
/// call. With a CYCLIC core graph two workers marking concurrently then acquired the same two locks in
/// opposite orders — a textbook ABBA deadlock. Every thread parked in `futex_do_wait` at 0% CPU, with
/// no deadlock report and no way for `--timeout` to reach it, so it presents as a silent total hang.
///
/// Measured on the release binary at `CHEZZI_THREADS=4`: **8/40 runs hung** before the fix (and 2/80
/// even before the mark walk was sped up — the speedup widened a pre-existing window rather than
/// creating it), **0/40 after**. It needs all three of: a cycle in the core graph, GC pressure, and
/// `CHEZZI_THREADS >= 2` — removing any one gives 0 hangs, which is what identified the mechanism.
///
/// This drives the built binary because the hang is a whole-process stall; an in-process test would
/// wedge the test binary. The `--timeout` flag deliberately is NOT used: it cannot interrupt a thread
/// blocked on a `Mutex`, which is the point. The bound is wall-clock on the subprocess instead.
///
/// 40 rounds. The per-run hang rate is profile-dependent — measured pre-fix at 20% release (8/40)
/// and 12.5% debug (5/40) — and `cargo test` runs DEBUG, so size the round count off the debug
/// number: 1 - 0.875^40 ~ 99.5% RED. (24 rounds would be only ~96%, and the first RED check of this
/// test passed by luck at that count.) ~2s when green.
#[test]
fn gc_mark_walk_does_not_deadlock_on_a_cyclic_core_graph() {
    let dir = std::env::temp_dir().join(format!("chz-gc-abba-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("cyclic_cores.chz");
    std::fs::write(
        &path,
        "import std.concurrency\n\
         \n\
         struct Link:\n    \
             flag: Shared[bool]\n    \
             dl: Channel[bool]\n    \
             kids: Channel[Link]\n\
         \n\
         fn mk() -> Link:\n    \
             return Link(flag=Shared(false), dl=Channel[bool](), kids=Channel[Link]())\n\
         \n\
         root := mk()\n\
         cur := root\n\
         i := 0\n\
         while i < 2:\n    \
             nxt := mk()\n    \
             cur.kids.send(nxt)\n    \
             cur = nxt\n    \
             i = i + 1\n\
         cur.kids.send(root)\n\
         root.kids.send(root)\n\
         \n\
         parallel:\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     root.flag.set(k % 2 == 0)\n            \
                     junk := [k, k + 1, k + 2]\n            \
                     k = k + 1\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     cur.flag.update(fn(b: bool) -> bool: not b)\n            \
                     junk := {\"a\": k, \"b\": k}\n            \
                     k = k + 1\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     junk := [[k], [k + 1]]\n            \
                     k = k + 1\n\
         print(\"alive\")\n",
    )
    .expect("write cyclic-core fixture");

    // SPAWN + poll + kill, never `output()`: a deadlocked child never closes its pipes, so
    // `output()` blocks forever and the RED case wedges the whole test binary instead of failing.
    // (Measured: it did exactly that on the first draft of this test.)
    for round in 0..40 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", "4")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(st) => break Some(st),
                None if std::time::Instant::now() >= deadline => break None,
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "round {round}: the cyclic-core program DEADLOCKED (no exit within 20s) - the GC \
                 mark walk is holding two core locks at once again"
            );
        };
        let mut stdout = String::new();
        if let Some(mut o) = child.stdout.take() {
            use std::io::Read as _;
            let _ = o.read_to_string(&mut stdout);
        }
        assert!(status.success(), "round {round}: program exited {status}");
        assert!(
            stdout.contains("alive"),
            "round {round}: program did not reach its final print"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same rule for the BYTE walk: `--max-heap`'s accounting must not hold two core locks either.
///
/// The rooting walk was split first (`gc_mark_walk_does_not_deadlock_on_a_cyclic_core_graph` above);
/// its byte-accounting twin `core::nested_core_bytes` was left holding a parent core's guard while
/// locking children, so the identical ABBA deadlock survived on any run with a memory cap. Measured
/// pre-fix, `chezzi test --max-heap=100000000` at `CHEZZI_THREADS=4` on a cyclic core graph: **1/40
/// hangs**, against 0/20 for the same program without the cap. Post-fix: 0/60.
///
/// This is the repo's "a guard must cover every site of its class" lesson applied to the fix itself —
/// the two walks are documented as needing to stay in lockstep, and only one of them was converted.
///
/// Spawns and polls rather than calling `output()`, for the reason given on the sibling test.
///
/// **Honest about its strength:** unlike the sibling gate, this one is NOT ~99% RED. The pre-fix hang
/// rate on this path is much lower and load-dependent — measured 1/40 release, and **0/120 debug when
/// run standalone**, yet it did fail 1 of 3 pre-fix runs under `cargo test`, where the harness adds
/// contention. A harsher fixture (8-node cycle with chords, 6 spawns, 4 000 iterations) did not raise
/// the standalone debug rate either. So treat this as a cheap opportunistic catch, ~60% RED at 40
/// rounds for 1.6 s; the real protection is structural — no site holds a core guard across another
/// core's lock, which `grep` over the `_deep`/`_structural` entry points verifies statically, and the
/// sibling rooting-walk gate covers the same class at a rate that IS reliable.
#[test]
fn max_heap_byte_walk_does_not_deadlock_on_a_cyclic_core_graph() {
    let dir = std::env::temp_dir().join(format!("chz-gc-abba-bytes-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("cyclic_cores_test.chz");
    std::fs::write(
        &path,
        "import std.concurrency\n\
         \n\
         struct Link:\n    \
             flag: Shared[bool]\n    \
             dl: Channel[bool]\n    \
             kids: Channel[Link]\n\
         \n\
         fn mk() -> Link:\n    \
             return Link(flag=Shared(false), dl=Channel[bool](), kids=Channel[Link]())\n\
         \n\
         fn bump(b: bool) -> bool:\n    \
             return not b\n\
         \n\
         test fn cyclic_cores_under_max_heap():\n    \
             root := mk()\n    \
             cur := root\n    \
             i := 0\n    \
             while i < 2:\n        \
                 nxt := mk()\n        \
                 cur.kids.send(nxt)\n        \
                 cur = nxt\n        \
                 i = i + 1\n    \
             cur.kids.send(root)\n    \
             root.kids.send(root)\n    \
             parallel:\n        \
                 spawn:\n            \
                     k := 0\n            \
                     while k < 2000:\n                \
                         root.flag.set(k % 2 == 0)\n                \
                         junk := [k, k + 1, k + 2]\n                \
                         k = k + 1\n        \
                 spawn:\n            \
                     k := 0\n            \
                     while k < 2000:\n                \
                         cur.flag.update(bump)\n                \
                         junk := {\"a\": k, \"b\": k}\n                \
                         k = k + 1\n        \
                 spawn:\n            \
                     k := 0\n            \
                     while k < 2000:\n                \
                         junk := [[k], [k + 1]]\n                \
                         k = k + 1\n    \
             assert root.kids.len() >= 0, \"alive\"\n",
    )
    .expect("write cyclic-core test fixture");

    for round in 0..40 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("test")
            .arg("--max-heap=100000000")
            .arg(&path)
            .env("CHEZZI_THREADS", "4")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(st) => break Some(st),
                None if std::time::Instant::now() >= deadline => break None,
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "round {round}: --max-heap on a cyclic core graph DEADLOCKED (no exit within 20s) - \
                 the byte walk is holding two core locks at once again"
            );
        };
        assert!(
            status.success(),
            "round {round}: chezzi test exited {status}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
