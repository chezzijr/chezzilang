//! TICKET-230 (wave 21 CHAN1): `--threads=N` caps Chezzi runners at N process-wide, main included,
//! like Go's `GOMAXPROCS`. Today each `Executor` gets its own N (measured at T=2: 3.96 cores for two
//! Executors with 4 CPU-burning jobs each).

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

#[cfg(unix)]
#[test]
fn two_executors_share_one_runner_budget_at_t2() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 4 {
        eprintln!("SKIP: {cores} CPU — the overshoot needs at least 4 cores to show");
        return;
    }
    let dir = std::env::temp_dir().join(format!("chz-t230-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("two_executors.chz");
    std::fs::write(
        &path,
        "import std.concurrency
fn burn(n: int) -> int:
    s := 0
    for i in 0..n:
        s += i % 7
    return s
fn main():
    a := Executor()
    b := Executor()
    for i in 0..4:
        a.submit(fn(): burn(3000000))
        b.submit(fn(): burn(3000000))
    a.shutdown()
    b.shutdown()
    print(\"done\")
main()
",
    )
    .expect("write program");

    let (wall, user, sys, status, stdout) =
        child_rusage::run_timed(&["run", path.to_str().unwrap()], "2");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "chezzi run must exit 0: {stdout}");
    assert!(
        user > std::time::Duration::from_millis(500),
        "program finished too fast (user={user:?}) to be a meaningful measurement"
    );
    let cpu = (user + sys).as_secs_f64();
    let cores_used = cpu / wall.as_secs_f64();
    assert!(
        cores_used <= 2.6,
        "--threads=2 must cap runners at 2 process-wide: used {cores_used:.2} cores (user={user:?} sys={sys:?} wall={wall:?})"
    );
}
