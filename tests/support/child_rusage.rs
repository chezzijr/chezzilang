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
