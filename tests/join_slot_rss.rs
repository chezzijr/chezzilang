//! A nursery join reduces its task slots where they lie (`MnSched::drain_scopes`): a join of 100k
//! tasks builds no copy of its slots. Each cell compares one 100k-task `parallel:` with 100
//! nurseries of 1000 tasks each (the same tasks, slots retired every 1000), at `CHEZZI_THREADS=4`,
//! in `main` and nested one nursery down. The slots themselves cost the difference (about 11 MiB
//! on the debug binary); a join that copies them costs about 21 MiB more.

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

/// The bound on (one 100k nursery) - (100 nurseries of 1k), in KiB. Measured 2026-11-18, debug
/// binary: about 11 MiB with the in-place reduce, about 32 MiB with the copying join.
#[cfg(unix)]
const SLACK_KB: i64 = 16 * 1024;

#[cfg(unix)]
fn program(name: &str, body: &str, nested: bool) -> std::path::PathBuf {
    let call = if nested {
        "parallel:\n    spawn main()\n"
    } else {
        "main()\n"
    };
    let src = format!(
        "import Atomic from std.concurrency\n\nfn main():\n    a := Atomic[int](0)\n{body}    print(a.load())\n\n{call}"
    );
    let dir = std::env::temp_dir().join(format!("chezzi-join-rss-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.chz");
    std::fs::write(&path, src).unwrap();
    path
}

/// The lowest peak RSS of three runs, so one noisy run cannot fail the cell.
#[cfg(unix)]
fn min_rss(path: &std::path::Path) -> i64 {
    (0..3)
        .map(|_| {
            let (kb, status, out) =
                child_rusage::run_max_rss(&["run", path.to_str().unwrap()], "4");
            assert!(
                status.success() && out == "100000\n",
                "{path:?}: {status:?} {out:?}"
            );
            kb
        })
        .min()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn a_hundred_thousand_task_join_copies_no_slots() {
    const ONE: &str = "    parallel:\n        for _ in 0..100000:\n            spawn a.add(1)\n";
    const CHUNKED: &str = "    for _ in 0..100:\n        parallel:\n            for _ in 0..1000:\n                spawn a.add(1)\n";
    for nested in [false, true] {
        let tag = if nested { "nested" } else { "main" };
        let one = min_rss(&program(&format!("one-{tag}"), ONE, nested));
        let chunked = min_rss(&program(&format!("chunked-{tag}"), CHUNKED, nested));
        assert!(
            one - chunked <= SLACK_KB,
            "{tag}: a 100k-task join peaks at {one} KiB, {} KiB above 100 joins of 1k ({chunked} KiB); bound {SLACK_KB}",
            one - chunked
        );
    }
}
