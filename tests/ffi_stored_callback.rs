//! Real-PROCESS tests for gaps.md **W6-8**: a STORED FFI callback (one C keeps past the extern call
//! that received it — `signal`, `atexit`, GLib, …) used to dangle into a SIGSEGV from checker-clean
//! code, because the libffi trampoline was `ffi_closure_free`d when the extern call returned while C
//! still held its code pointer.
//!
//! Stored/cross-thread callbacks stay DEFERRED — but the deferral is now a LOUD, defined abort
//! instead of undefined behavior: an ARMED trampoline is leaked and POISONED (its atomic armed flag
//! cleared), so a later invocation from C — or one on any thread other than the caller — writes a
//! named message to stderr, drains the program's queued stdout, and `abort()`s.
//!
//! The leak that buys that guarantee has to stay inside its stated ceiling, which is what the other
//! tests here pin:
//! - it is per callback-passing extern CALL, never per *attempt* — a trampoline that was never armed
//!   (the extern call bailed during arg marshalling, so C never saw the code pointer) is still freed;
//! - exhausting libffi's exec-closure pool is a clean, recoverable Chezzi error, never a crash.
//!
//! Must be subprocess tests: the first program dies on SIGABRT, so it can never be an `examples/*.chz`
//! stdout golden, and FFI UB is memory-layout dependent (that is exactly how the `Box<Cif>` bug
//! slipped past the goldens — see `boxed_callback_cif_address_is_stable_across_moves`).
#![cfg(target_os = "linux")]

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_w68_{}_{}", std::process::id(), n));
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

/// `chezzi run [--serial] <entry>` with **core dumps disabled** in the child. One of these tests
/// aborts the child on purpose; without this the suite deposits a core dump per run per engine on
/// any host with `kernel.core_pattern=|…systemd-coredump` (i.e. a stock distro).
fn chezzi(serial: bool, entry: &PathBuf) -> std::process::Output {
    chezzi_cmd(serial, entry).output().expect("spawn chezzi")
}

/// The `Command` behind [`chezzi`], for the one test that needs to control WHEN the child's stdout
/// pipe is drained.
fn chezzi_cmd(serial: bool, entry: &PathBuf) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    cmd.arg(entry);
    // `RLIMIT_CORE = 1`, not 0: a stock distro pipes `kernel.core_pattern` into `systemd-coredump`,
    // and for a PIPED pattern the kernel ignores the size limit — except for the special value 1,
    // which is the documented "abort this dump" switch (core(5)). For a plain-file pattern 1 is
    // below `min_coredump` and skips the dump too.
    // SAFETY: `setrlimit` is async-signal-safe and touches only the child's own rlimits, which is
    // exactly what `pre_exec` allows between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            let one = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &one);
            Ok(())
        });
    }
    cmd
}

/// Reads one `/proc/self/status` field (in kB) from inside the program under test — the only way to
/// see a child's own memory growth without a `wait4` rusage hook.
const PROC_FIELD: &str = r#"import std.io
import std.string

fn field(key: str) -> int:
    match io.read_file("/proc/self/status"):
        Ok(s):
            for line in s.split("\n"):
                if line.starts_with(key):
                    v := line.replace(key, "").replace("kB", "").strip()
                    match v.to_int():
                        Some(n): return n
                        None: return -3
            return -2
        Err(e):
            return -1
"#;

/// The gaps.md W6-8 repro verbatim: `signal(10, handler)` STORES the trampoline, then `raise(10)`
/// makes C invoke it after the `signal` extern call has already returned.
const REPRO: &str = r#"import std.ffi

extern "libc.so.6":
    fn signal(sig: int, h: fn(int) -> int) -> ptr
    fn raise(sig: int) -> int

fn handler(sig: int) -> int:
    print("handler", sig)
    return 0

h := signal(10, handler)
print(raise(10))
"#;

#[test]
fn stored_callback_aborts_loudly_on_both_engines() {
    for serial in [false, true] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", REPRO);
        let out = chezzi(serial, &entry);
        let engine = if serial { "--serial" } else { "M:N" };
        let stderr = String::from_utf8_lossy(&out.stderr);

        // SIGABRT (6), never SIGSEGV (11) and never a silent success: the poison stub must abort,
        // not unwind (unwinding out of Rust into a C frame is itself UB).
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "[{engine}] a stored FFI callback must abort loudly, got status {:?} signal {:?}; \
             stderr: {stderr}",
            out.status.code(),
            out.status.signal(),
        );
        assert!(
            stderr.contains("stored/cross-thread callbacks are not supported"),
            "[{engine}] the abort must name the unsupported feature; stderr: {stderr}"
        );
        // An intentional abort must not litter the host with core dumps (see `chezzi()`).
        assert!(
            !out.status.core_dumped(),
            "[{engine}] the deliberately-aborting child must not dump core"
        );
    }
}

/// The abort is a DIAGNOSTIC, so it must not destroy the context it is diagnosing. Under `chezzi run`
/// every `print` is a queue push drained by a background writer thread (`src/vm/stream.rs`), whose
/// stated invariant is "a killed program keeps every byte it produced" — a bare `abort()` from the
/// poison stub truncates the program's own stdout at a nondeterministic point (different per run and
/// per engine). The stub therefore drains the streamed sink before aborting.
#[test]
fn abort_path_keeps_the_programs_queued_stdout() {
    // 20k numbered lines is ~108 kB — well past a 64 kB pipe buffer — and this test deliberately
    // does NOT read the child's stdout until the child is long past its last `print`. The writer
    // thread therefore blocks on a full pipe with the tail of the program still sitting in the
    // queue: exactly the state a bare `abort()` discards.
    const SRC: &str = r#"import std.ffi

extern "libc.so.6":
    fn signal(sig: int, h: fn(int) -> int) -> ptr
    fn raise(sig: int) -> int

fn handler(sig: int) -> int:
    return 0

for i in range(20000):
    print(i)
print("about to raise")
h := signal(10, handler)
r := raise(10)
"#;
    for serial in [false, true] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", SRC);
        let child = chezzi_cmd(serial, &entry)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");
        // Let the child run to (and past) its `raise` with its stdout pipe unread and full.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let out = child.wait_with_output().expect("wait chezzi");
        let engine = if serial { "--serial" } else { "M:N" };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "[{engine}] expected the poison abort; stderr: {stderr}"
        );
        assert!(
            stderr.contains("stored/cross-thread callbacks are not supported"),
            "[{engine}] the abort must name the unsupported feature; stderr: {stderr}"
        );
        assert!(
            stdout.ends_with("about to raise\n") && stdout.starts_with("0\n1\n"),
            "[{engine}] the abort lost the program's queued stdout: {} bytes, tail {:?}",
            stdout.len(),
            &stdout[stdout.len().saturating_sub(40)..]
        );
    }
}

/// A callback C stored and invokes from ITS OWN thread must abort just as loudly — never re-enter the
/// VM off-thread. Two guards cover it: the `armed` flag is atomic (so the trampoline's load and the
/// poison store are not a data race), and the owner-thread check aborts unconditionally even if a
/// foreign thread observes a stale `armed`. Which of the two fires here is a scheduling race; both
/// print the same `stored/cross-thread callbacks are not supported` tail, and neither may run the
/// Chezzi body.
///
/// This is a REGRESSION PIN, not a red-first repro: `pthread_create` returns microseconds before the
/// child thread gets going, so the poison store almost always wins and the `armed` guard is what
/// fires. The owner-thread guard was shown necessary by widening the armed window (a 300 ms sleep
/// before `drop(callback_closures)`): with it, the pre-fix build ran the Chezzi callback body on the
/// C-spawned thread and exited 0, while the fixed build aborts on the cross-thread message.
#[test]
fn cross_thread_stored_callback_aborts_without_entering_the_vm() {
    const SRC: &str = r#"import std.ffi

extern "libc.so.6":
    fn pthread_create(t: ptr, attr: ptr, f: fn(ptr) -> ptr, arg: ptr) -> int
    fn usleep(us: int) -> int

fn worker(a: ptr) -> ptr:
    print("VM RE-ENTERED OFF-THREAD")
    return ffi.null()

tid := ffi.alloc(8)
rc := pthread_create(tid, ffi.null(), worker, ffi.null())
print("created {rc}")
u := usleep(2000000)
print("NO ABORT")
"#;
    for serial in [false, true] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", SRC);
        let out = chezzi(serial, &entry);
        let engine = if serial { "--serial" } else { "M:N" };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "[{engine}] a callback invoked from a C-spawned thread must abort, got status {:?} \
             signal {:?}; stdout: {stdout} stderr: {stderr}",
            out.status.code(),
            out.status.signal(),
        );
        assert!(
            stderr.contains("stored/cross-thread callbacks are not supported"),
            "[{engine}] the abort must name the unsupported feature; stderr: {stderr}"
        );
        assert!(
            !stdout.contains("VM RE-ENTERED OFF-THREAD") && !stdout.contains("NO ABORT"),
            "[{engine}] the foreign thread must never run the Chezzi callback body; stdout: {stdout}"
        );
    }
}

/// The poison-and-leak is for trampolines C might have STORED. A call that faults while marshalling a
/// LATER argument never reaches `ffi_call`, so C provably never saw the code pointer — freeing it is
/// the only correct choice, and leaking it would grow the exec-closure pool on a pure `recover:` retry
/// loop that never enters C at all.
#[test]
fn unarmed_callback_trampoline_is_freed_not_leaked() {
    // `puts` resolves, but the second arg has an interior NUL — `CString::new` rejects it, so the
    // extern call bails AFTER the callback trampoline for arg 0 was allocated and BEFORE `ffi_call`.
    // Peak RSS is compared against the SAME program at zero iterations, so the check is a growth
    // measurement, not a hard-coded footprint.
    let src = |iters: usize| {
        format!(
            r#"{PROC_FIELD}
extern "libc.so.6":
    fn puts(h: fn(int) -> int, s: str) -> int

fn cb(x: int) -> int:
    return x

bad := "a" + chr(0) + "b"
errs := 0
for i in range({iters}):
    r := recover: puts(cb, bad)
    match r:
        Ok(v): pass
        Err(e): errs = errs + 1
hw := field("VmHWM:")
print("errs={{errs}} hwm={{hw}}")
"#
        )
    };
    const ITERS: usize = 50_000;
    for serial in [false, true] {
        let engine = if serial { "--serial" } else { "M:N" };
        let peak = |iters: usize| {
            let t = TmpDir::new();
            let entry = t.write("main.chz", &src(iters));
            let out = chezzi(serial, &entry);
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(
                stdout.starts_with(&format!("errs={iters} hwm=")),
                "[{engine}] every attempt must fault recoverably; stdout: {stdout} stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            stdout
                .trim()
                .rsplit_once('=')
                .unwrap()
                .1
                .parse::<i64>()
                .unwrap()
        };
        // The probe must have WORKED. `field()` answers with the sentinels -1 (unreadable
        // /proc/self/status), -2 (no such field) or -3 (unparsable value) rather than faulting, and
        // both runs would then return the SAME sentinel — `looped - base == 0` passes the growth
        // assertion below while measuring nothing at all. A green suite is not coverage.
        let assert_probed = |kb: i64, iters: usize| {
            assert!(
                kb > 0,
                "[{engine}] the VmHWM probe failed (sentinel {kb}) at {iters} iterations — the leak \
                 assertion below would be vacuous"
            );
            kb
        };
        // Leaking all 50k unarmed trampolines cost ~21 MB of peak RSS (200k cost 72 MB); freeing
        // them keeps the peak flat, well inside 8 MB of run-to-run slack.
        let (base, looped) = (assert_probed(peak(0), 0), assert_probed(peak(ITERS), ITERS));
        assert!(
            looped - base < 8_000,
            "[{engine}] an unarmed callback trampoline must be freed, not leaked: peak RSS grew \
             {base} kB -> {looped} kB over {ITERS} never-armed attempts"
        );
    }
}

/// The accepted per-call leak makes libffi's exec-closure pool exhaustible. That must surface as a
/// clean, recoverable Chezzi error — `libffi::low::closure_alloc()` would instead `assume_init()` an
/// uninitialised code pointer and hand `ffi_prep_closure_loc` a NULL handle to write through
/// (SIGSEGV), i.e. the leak would have traded a crash on an UNSUPPORTED stored callback for a crash
/// on the SUPPORTED during-the-call `qsort` one.
#[test]
fn exhausted_closure_pool_faults_cleanly_instead_of_crashing() {
    // The program caps its OWN address space (via libc `setrlimit`) once the interpreter is up —
    // `ulimit -v` from outside can't, because startup transiently reserves ~1.2 GB for the front-end
    // stack thread. 8 MB of headroom is ~20k leaked trampolines, i.e. under a second.
    let src = format!(
        r#"{PROC_FIELD}
import std.ffi

extern "libc.so.6":
    fn qsort(base: ptr, n: int, size: int, cmp: fn(ptr, ptr) -> int)
    fn setrlimit(res: int, lim: ptr) -> int

fn cmp(a: ptr, b: ptr) -> int:
    x := ffi.load_int64(a)
    y := ffi.load_int64(b)
    if x < y:
        return -1
    if x > y:
        return 1
    return 0

# Warm up every lazily-spawned runtime thread (the M:N output pump starts on the first `print`)
# BEFORE capping: a thread stack is an 8 MB reservation, which the cap must not have to cover.
print("warm")
lim := (field("VmSize:") + 8192) * 1024
rl := ffi.alloc(16)
ffi.store_int64_at(rl, 0, lim)
ffi.store_int64_at(rl, 8, lim)
rc := setrlimit({rlimit_as}, rl)
ffi.free(rl)
if rc != 0:
    print("setrlimit failed")

buf := ffi.alloc(16)
ffi.store_int64_at(buf, 0, 9)
ffi.store_int64_at(buf, 8, 4)
for i in range(2000000):
    r := recover: qsort(buf, 2, 8, cmp)
    match r:
        Ok(v): pass
        Err(e):
            print("clean: {{e.message()}}")
            break
print("survived")
"#,
        rlimit_as = libc::RLIMIT_AS,
    );
    for serial in [false, true] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let out = chezzi(serial, &entry);
        let engine = if serial { "--serial" } else { "M:N" };
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.signal(),
            None,
            "[{engine}] running the closure pool dry must not kill the process; stdout: {stdout} \
             stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("FFI closure pool is exhausted") && stdout.ends_with("survived\n"),
            "[{engine}] exhaustion must be a recoverable fault; stdout: {stdout} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
