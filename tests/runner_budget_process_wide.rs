//! TICKET-230 (wave 21 CHAN1): `--threads=N` caps Chezzi runners at N process-wide, main included,
//! like Go's `GOMAXPROCS`. Before the fix each `Executor` got its own N (measured at T=2: 3.96 cores
//! for two Executors with 4 CPU-burning jobs each).

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

/// The cells measure CPU against wall, so two of them at once would load each other's cores.
#[cfg(unix)]
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(unix)]
const BURN: &str = r#"import std.concurrency
fn burn(n: int) -> int:
    s := 0
    for i in 0..n:
        s += i % 7
    return s
fn burn_send(n: int, done: Channel[int]):
    burn(n)
    done.send(1)
"#;

#[cfg(unix)]
fn assert_cores_at_most(name: &str, program: &str, threads: &str, bound: f64) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 4 {
        eprintln!("SKIP: {cores} CPU — the overshoot needs at least 4 cores to show");
        return;
    }
    let dir = std::env::temp_dir().join(format!("chz-t230-budget-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{name}.chz"));
    std::fs::write(&path, format!("{BURN}{program}")).expect("write program");

    let (wall, user, sys, status, stdout) =
        child_rusage::run_timed(&["run", path.to_str().unwrap()], threads);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(status.success(), "chezzi run must exit 0: {stdout}");
    assert!(
        user > std::time::Duration::from_millis(500),
        "program finished too fast (user={user:?}) to be a meaningful measurement"
    );
    let cpu = (user + sys).as_secs_f64();
    let cores_used = cpu / wall.as_secs_f64();
    eprintln!("{name}: used {cores_used:.2} cores");
    assert!(
        cores_used <= bound,
        "--threads={threads} must cap runners at {threads} process-wide: used {cores_used:.2} cores (user={user:?} sys={sys:?} wall={wall:?})"
    );
}

#[cfg(unix)]
#[test]
fn two_executors_share_one_runner_budget_at_t2() {
    assert_cores_at_most(
        "two_executors",
        r#"fn main():
    a := Executor()
    b := Executor()
    for i in 0..4:
        a.submit(fn(): burn(3000000))
        b.submit(fn(): burn(3000000))
    a.shutdown()
    b.shutdown()
    print("done")
main()
"#,
        "2",
        2.6,
    );
}

#[cfg(unix)]
#[test]
fn executor_and_parallel_share_one_runner_budget_at_t2() {
    assert_cores_at_most(
        "exec_and_parallel",
        r#"fn main():
    ex := Executor()
    for i in 0..4:
        ex.submit(fn(): burn(3000000))
    parallel:
        for i in 0..4:
            spawn burn(3000000)
    ex.shutdown()
    print("done")
main()
"#,
        "2",
        2.6,
    );
}

#[cfg(unix)]
#[test]
fn executor_and_busy_main_share_one_runner_budget_at_t2() {
    assert_cores_at_most(
        "exec_busy_main",
        r#"fn main():
    ex := Executor()
    for i in 0..4:
        ex.submit(fn(): burn(3000000))
    burn(6000000)
    ex.shutdown()
    print("done")
main()
"#,
        "2",
        2.6,
    );
}

/// W15-9: a nursery body that blocks once and then burns CPU beside burning siblings was one runner
/// beyond N. The sender is short so the body wakes while the siblings still burn.
#[cfg(unix)]
#[test]
fn blocked_then_burning_body_shares_one_runner_budget_at_t2() {
    assert_cores_at_most(
        "blocked_body",
        r#"fn main():
    done := Channel[int](1)
    parallel:
        spawn burn_send(300000, done)
        for i in 0..3:
            spawn burn(4000000)
        done.recv()
        burn(6000000)
    print("done")
main()
"#,
        "2",
        2.6,
    );
}
