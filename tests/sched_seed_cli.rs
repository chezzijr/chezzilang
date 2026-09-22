//! TICKET-167 -- the seeded-scheduler oracle (`docs/future.md` §2b) does not exist yet. There is no
//! `CHEZZI_SCHED_SEED` env var anywhere in `src/` (confirmed by grep), so it is silently ignored: a
//! failing run gives no way to know, let alone replay, the schedule that produced the failure. Per
//! the ticket's part 1 requirement ("A failing run prints its seed, and rerunning with that seed
//! reproduces the failure"), the seed must appear in the failing run's own diagnostics.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("chezzi_sched_seed_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A failing run under `CHEZZI_SCHED_SEED` must report the seed it used, so the failure can be
/// replayed. Today the env var is read nowhere in the engine, so nothing prints it.
#[test]
fn a_failing_run_under_sched_seed_reports_its_seed() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "panic(\"boom\")\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .env("CHEZZI_SCHED_SEED", "12345")
        .arg("run")
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("12345"),
        "a failing run under CHEZZI_SCHED_SEED=12345 must report the seed \
         somewhere in its output, so the failure can be replayed; got {combined:?}"
    );
}
