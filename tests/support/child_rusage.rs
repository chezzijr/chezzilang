//! Shared per-child wall/user/sys measurement, moved out of `tests/chezzi_threads_sys_time.rs`
//! (TICKET-059) so `tests/chezzi_threads_cli.rs` can reuse it for its two W8-8 serialization gates.
//! Included via `#[path]` rather than as a crate dependency: cargo auto-discovers `tests/*.rs` and
//! `tests/*/main.rs` as test targets, and a module under `tests/support/` is neither.

#[cfg(unix)]
use std::process::Command;

/// Per-child wall/user/sys via `libc::wait4` on the spawned pid, rather than `Child::wait()` (which
/// surfaces no rusage) or `getrusage(RUSAGE_CHILDREN)` (which would aggregate every child this test
/// binary has ever reaped — contaminated by any other subprocess-spawning test sharing the process).
/// stdout/stderr are drained on background threads before the blocking `wait4` call, so a chatty
/// child can't deadlock on a full pipe buffer while we wait. `threads` sets `CHEZZI_THREADS`
/// explicitly for the child (callers used to hardcode `HERD_WORKERS`; TICKET-059's callers need
/// `1`).
// `wait4` IS the reap (it's `waitpid` + rusage in one syscall) — clippy can't see that, only that
// `Child::wait()`/`.output()` was never called on `child`.
#[allow(clippy::zombie_processes)]
#[cfg(unix)]
pub fn run_timed(
    args: &[&str],
    threads: &str,
) -> (
    std::time::Duration,
    std::time::Duration,
    std::time::Duration,
    std::process::ExitStatus,
    String,
) {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args);
    cmd.env("CHEZZI_THREADS", threads);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let wall_start = std::time::Instant::now();
    let mut child = cmd.spawn().expect("spawn chezzi");
    let pid = child.id() as libc::pid_t;

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout_pipe.read_to_string(&mut s);
        s
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let mut status: libc::c_int = 0;
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` was just returned by `child.id()` for a process we own and have not yet waited
    // on; `&mut status`/`&mut rusage` are valid, appropriately-sized out-params for the call.
    let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
    assert_eq!(ret, pid, "wait4({pid}) failed");
    let wall = wall_start.elapsed();

    let stdout = stdout_reader.join().expect("stdout reader thread");
    let _stderr = stderr_reader.join().expect("stderr reader thread");

    let user = std::time::Duration::new(
        rusage.ru_utime.tv_sec as u64,
        (rusage.ru_utime.tv_usec as u32) * 1000,
    );
    let sys = std::time::Duration::new(
        rusage.ru_stime.tv_sec as u64,
        (rusage.ru_stime.tv_usec as u32) * 1000,
    );
    (
        wall,
        user,
        sys,
        std::process::ExitStatus::from_raw(status),
        stdout,
    )
}

/// The CPU cost the CPU-measuring tests' burn sizes and measured bands were taken at: one
/// `burn(750000)` of [`CALIBRATION_BURN`] cost about 1 s on the debug binary while chezzi-vm built
/// at opt-level 0 (`burn(1500000)`: 1975-1995 ms on main 772e0bb8). Since 36e54ff6 the debug VM is
/// about 4x faster, so the same sizes finished under the tests' "finished too fast" floors.
#[cfg(unix)]
const REFERENCE_BURN_CPU: std::time::Duration = std::time::Duration::from_millis(1000);

#[cfg(unix)]
const CALIBRATION_BURN: &str = "fn burn(n: int) -> int:\n    \
                                x := 0\n    \
                                i := 0\n    \
                                while i < n:\n        \
                                x = x + i * i - i\n        \
                                i += 1\n    \
                                return x\n\
                                burn(750000)\n";

/// The factor (at least 1) that makes a burn cost on this binary at least what it cost on the binary the
/// tests were calibrated on ([`REFERENCE_BURN_CPU`]). One `burn(750000)` at `CHEZZI_THREADS=1`,
/// measured in CPU time (rusage), which load on other cores does not inflate. Measured once per
/// test process.
#[cfg(unix)]
#[allow(dead_code)]
pub fn burn_scale() -> u64 {
    static SCALE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("chz-burn-scale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("burn.chz");
        std::fs::write(&path, CALIBRATION_BURN).expect("write calibration program");
        let (_, user, sys, status, stdout) = run_timed(&["run", path.to_str().unwrap()], "1");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(status.success(), "the calibration burn failed: {stdout}");
        let cpu = (user + sys).as_secs_f64();
        ((REFERENCE_BURN_CPU.as_secs_f64() / cpu).ceil() as u64).max(1)
    })
}

/// `program` with the size of every `burn(<int literal>)` call multiplied by [`burn_scale`], so a
/// program keeps the CPU cost its test's bands were measured at.
#[cfg(unix)]
#[allow(dead_code)]
pub fn scale_burns(program: &str) -> String {
    let scale = burn_scale();
    let mut out = String::with_capacity(program.len());
    let mut rest = program;
    while let Some(at) = rest.find("burn(") {
        let (head, tail) = rest.split_at(at + "burn(".len());
        out.push_str(head);
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && tail[digits..].starts_with(')') {
            let n: u64 = tail[..digits].parse().expect("a burn size");
            out.push_str(&(n * scale).to_string());
            rest = &tail[digits..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}
