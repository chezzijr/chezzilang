//! TICKET-112 (W12-4 addendum, `cousin_fed`) — a nested GENUINE deadlock is never detected at
//! `CHEZZI_THREADS>=2` when the joining task's cousins are parked on channels only that task can
//! feed after its join. Expected (Go's model, confirmed by `nested_nursery_owner_blocked...` at
//! T=1 once TICKET-103 landed): `inner err`, `F got 2`, `done`, rc=0. Measured on this branch at
//! `af156f81`-equivalent: T=1 now passes (TICKET-103), but T=2/4 hang with NO output at all
//! (rc=124), because the inner nursery's genuine deadlock on `never.recv()` is never faulted.
//! TICKET-135 (D1): the verdict is now FATAL through `recover:`, so the expected outcome is a
//! `deadlock` abort at rc!=0 within the deadline, with no `inner err` / `F got 2` / `done`.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it.

use std::process::Command;

#[test]
fn cousin_fed_recovered_deadlock_is_fatal_not_a_hang_at_two_and_four_workers() {
    let program = "fn main():\n    \
        never := Channel[int](0)\n    \
        x := Channel[int](0)\n    \
        y := Channel[int](0)\n    \
        parallel:\n        \
            spawn:\n            \
                r := recover:\n                \
                    parallel:\n                        \
                        spawn:\n                            \
                            never.recv()\n            \
                match r:\n                \
                    Ok(_): print(\"inner ok\")\n                \
                    Err(e): print(\"inner err\")\n            \
                x.send(1)\n        \
            spawn:\n            \
                parallel:\n                \
                    spawn:\n                    \
                        y.send(x.recv() + 1)\n                \
                    print(\"F got {y.recv()}\")\n    \
        print(\"done\")\n\
        main()\n";

    for threads in ["2", "4"] {
        let dir = std::env::temp_dir().join(format!(
            "chz-ticket112-cousin_fed-{}-{}",
            std::process::id(),
            threads
        ));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("cousin_fed.chz");
        std::fs::write(&path, program).expect("write cousin_fed fixture");

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
                "CHEZZI_THREADS={threads}: a task joining a nested nursery whose only child is \
                 genuinely deadlocked must abort with `deadlock`, not hang forever with no \
                 output (no exit within 10s)"
            );
        };

        let mut stdout = String::new();
        if let Some(mut o) = child.stdout.take() {
            use std::io::Read as _;
            let _ = o.read_to_string(&mut stdout);
        }
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read as _;
            let _ = e.read_to_string(&mut stderr);
        }
        assert!(
            !status.success() && stderr.contains("deadlock"),
            "CHEZZI_THREADS={threads}: expected a fatal `deadlock`, got {status} — stdout: {stdout} stderr: {stderr}"
        );
        assert!(
            !stdout.contains("inner err")
                && !stdout.contains("F got 2")
                && !stdout.contains("done"),
            "CHEZZI_THREADS={threads}: `recover:` must not catch the verdict, got stdout: \
             {stdout} stderr: {stderr}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
