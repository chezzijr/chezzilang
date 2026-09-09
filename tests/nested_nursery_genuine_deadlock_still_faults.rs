//! TICKET-101 — TICKET-099's fix for a sibling send not waking a deeper-nursery receiver
//! (`MnSched::wake_run_wide` / `peer_can_move`) traded that false `deadlock` for a hang on a
//! GENUINE nested deadlock: a receiver parked in a nursery two levels deep, on a channel with no
//! possible sender anywhere in the run, now never faults `deadlock:` — it hangs forever instead.
//! Every other worker count hangs the same way (measured: rc=124 at `--threads=1`, `=2`, `=4`,
//! `=8`, all under a 10s deadline, 0/4 exit).
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it (same pattern as
//! `nested_nursery_deadlock_threads_one.rs`).

use std::process::Command;

#[test]
fn genuine_deadlock_two_nurseries_deep_faults_instead_of_hanging() {
    for threads in ["1", "2", "4", "8"] {
        let dir =
            std::env::temp_dir().join(format!("chz-ticket101-{}-{}", std::process::id(), threads));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("dd101.chz");
        std::fs::write(
            &path,
            "ch := Channel[int](0)\n\
             parallel:\n    \
                 spawn:\n        \
                     parallel:\n            \
                         spawn:\n                \
                             print(\"inner got \" + str(ch.recv()))\n",
        )
        .expect("write genuine two-deep-nursery deadlock fixture");

        let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", threads)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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
                "CHEZZI_THREADS={threads}: a receiver parked two nurseries deep on a channel with no \
                 possible sender must fault `deadlock: …` at rc=1, not hang forever with no output \
                 (no exit within 10s)"
            );
        };

        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read as _;
            let _ = e.read_to_string(&mut stderr);
        }
        assert!(
            !status.success(),
            "CHEZZI_THREADS={threads}: expected a nonzero exit for an undetected deadlock, got \
             {status} (stderr: {stderr})"
        );
        assert!(
            stderr.contains("deadlock:"),
            "CHEZZI_THREADS={threads}: expected a `deadlock: …` fault, got: {stderr}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
