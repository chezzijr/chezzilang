//! M1.5 — every blocking call a VM thread can reach goes through the one GC door, `Vm::gc_blocking`.
//!
//! A thread that holds a GC token (`gc_coord::Attached`) must reach a safepoint or detach in bounded
//! time: a stop of the world waits for it. So a wait of an attached thread runs inside the closure
//! of a `gc_blocking(..)` call, which detaches the thread for the wait. This scan reads the
//! production code of `src/vm` (comments, string contents and inline `#[cfg(test)] mod NAME { .. }`
//! blocks removed; `tests/`, `golden_tests/` and `*tests.rs` skipped) and finds each blocking call:
//! a condvar or barrier wait, a sleep, a park, a channel receive, a thread join, a `poll(2)`. Each
//! hit must sit inside the parentheses of a `gc_blocking(` call, or inside a function on
//! [`ALLOWED`] with its reason.
//!
//! A row with a call spelling is a WRAPPER: a function that blocks for its caller. Each call of it
//! is then a hit too, so the door moves to its callers and the scan follows it there.
//!
//! The scan also finds the raw blocking syscalls of `src/vm` (Go runs `entersyscall` around each):
//! a file open, read, write and flush, a name resolution (`connect_nonblocking`,
//! `listen_nonblocking`), and an extern C call (`cffi.call`).
//!
//! What the scan cannot see, so do not trust it as complete:
//! - a syscall behind a spelling it does not list, or one inside `src/native` that a native reaches
//!   (an in-place `Kind::Blocking`/`Kind::HostWait` native runs inside the door at `call.rs`; a
//!   `Kind::Inline` native's syscalls must stay short, such as `getcwd` or `getrandom`);
//! - a `Mutex::lock` held by a thread that waits detached (the rule is in `Vm::gc_blocking`'s doc);
//! - a `Drop` that makes a syscall (`WriterCore::drop` flushes a buffered writer);
//! - a row is keyed by file and function name, so a new wait in an allowed function passes.

use std::fs;
use std::path::{Path, PathBuf};

/// The spellings of a blocking call.
const WAITS: &[&str] = &[
    ".wait(",
    ".wait_while(",
    ".wait_timeout(",
    ".wait_timeout_while(",
    "thread::sleep(",
    "thread::park",
    ".recv()",
    ".recv_timeout(",
    ".join()",
    "libc::poll(",
    "File::open(",
    "File::create(",
    "OpenOptions::new(",
    ".write_all(",
    ".flush(",
    "read_until(",
    "read_to_end(",
    "Read::read(",
    "connect_nonblocking(",
    "listen_nonblocking(",
    "cffi.call(",
];

/// (file under `src/vm`, enclosing function or `*` for the whole file, call spelling of a wrapper
/// or `""`, why a wait there needs no door)
const ALLOWED: &[(&str, &str, &str, &str)] = &[
    (
        "gc_coord.rs",
        "*",
        "",
        "the coordinator itself: a thread waits there stopped, detached or not yet attached",
    ),
    (
        "pool.rs",
        "worker_loop",
        "",
        "a pool thread between jobs drives no Vm, so it holds no token",
    ),
    (
        "runner_cache.rs",
        "serve",
        "",
        "a runner thread parks only after it dropped its shell, and the shell's token with it",
    ),
    (
        "blocking_pool.rs",
        "worker_loop",
        "",
        "a blocking-pool thread runs off-heap natives and never attaches",
    ),
    (
        "poller.rs",
        "poll_loop",
        "",
        "the netpoller thread never attaches",
    ),
    (
        "stream.rs",
        "flush_stream",
        "",
        "the CLI thread calls it after the run, with no Vm",
    ),
    (
        "mod.rs",
        "on_vm_stack",
        "",
        "the caller of the VM thread waits for it and runs no VM code",
    ),
    (
        "width.rs",
        "acquire_slot",
        "acquire_slot(",
        "the runner gate's wait; its callers in width.rs are rows",
    ),
    (
        "width.rs",
        "acquire",
        "width::acquire(",
        "takes a runner permit; each call is in a door",
    ),
    (
        "width.rs",
        "released",
        "width::released(",
        "waits with the permit released; each call is in a door",
    ),
    (
        "width.rs",
        "take",
        "RunPermit::take(",
        "takes the run's first permit; each call is in a door",
    ),
    (
        "core.rs",
        "wait_update_guard",
        "wait_update_guard(",
        "the update-guard wait; its callers in core.rs are rows",
    ),
    (
        "core.rs",
        "acquire_update_guard",
        "acquire_update_guard(",
        "takes an update guard; each call is in a door",
    ),
    (
        "core.rs",
        "acquire_update_guard_within",
        "acquire_update_guard_within(",
        "takes an update guard; each call is in a door",
    ),
    (
        "core.rs",
        "await_update_guard_free",
        "await_update_guard_free(",
        "waits for an update guard; each call is in a door",
    ),
    (
        "sched_types.rs",
        "wait_fd_ready",
        "wait_fd_ready(",
        "waits for a socket; each call is in a door",
    ),
    (
        "mnsched.rs",
        "joiner_wait",
        "joiner_wait(",
        "a joiner's tick; its callers in mnsched_verdict.rs are rows",
    ),
    (
        "mnsched_verdict.rs",
        "wait_for_scope",
        ".wait_for_scope(",
        "a nursery join's wait; each call is in a door",
    ),
    (
        "mnsched_verdict.rs",
        "wait_for_completion",
        ".wait_for_completion(",
        "a nursery join's wait; each call is in a door",
    ),
    (
        "mnsched.rs",
        "join_tick",
        ".join_tick(",
        "an Executor join's tick; each call is in a door",
    ),
    (
        "mnsched.rs",
        "wait_helpers",
        ".wait_helpers(",
        "waits for a sched's runner leases; its caller is a row",
    ),
    (
        "sched.rs",
        "join_helpers",
        "join_helpers(",
        "waits for a sched's runner leases; each call is in a door",
    ),
    (
        "quiesce.rs",
        "await_exit_origins",
        ".await_exit_origins(",
        "waits for exit output in flight; its caller is a row",
    ),
    (
        "mnsched.rs",
        "take_finished_streams",
        ".take_finished_streams(",
        "waits for exit output in flight; each call is in a door",
    ),
    (
        "quiesce.rs",
        "hold_run_end_window",
        "",
        "a test-only 300 ms hold of a fault window: it ends on its own, so a stop waits at most that",
    ),
    (
        "fileio.rs",
        "write_locked",
        "",
        "runs only in `with_writer_lock`: in the door, or attached when `writes_are_buffered` says it makes no syscall",
    ),
    (
        "fileio.rs",
        "flush_locked",
        "",
        "runs only in `with_writer_lock`: in the door, or attached when `flush_is_local` says it makes no syscall",
    ),
    (
        "fileio.rs",
        "read_line_locked",
        "",
        "runs only in `with_reader_locks`: in the door, or attached when `line_is_buffered` says it makes no syscall",
    ),
    (
        "fileio.rs",
        "read_bytes_locked",
        "",
        "runs only in `with_reader_locks`: in the door, or attached when `bytes_are_buffered` says it makes no syscall",
    ),
    (
        "socket.rs",
        "socket_read_bytes",
        "",
        "a non-blocking socket fd: a read that would block returns WouldBlock and parks",
    ),
    (
        "socket.rs",
        "socket_read",
        "",
        "a non-blocking socket fd: a read that would block returns WouldBlock and parks",
    ),
    (
        "stream.rs",
        "spawn_writer",
        "",
        "the stdout/stderr writer thread never attaches",
    ),
    (
        "core.rs",
        "drop",
        "",
        "`WriterCore::drop` flushes a buffered writer where the last handle drops, with no Vm (a sweep can drop it); open for M1.7",
    ),
    (
        "call.rs",
        "extern_call_attached",
        "",
        "a C call with a callback param re-enters the VM through its trampoline, which needs the token; open (fix round 1 STOP)",
    ),
    (
        "core.rs",
        "try_update_guard",
        "",
        "a zero budget: it returns at once",
    ),
    (
        "entry.rs",
        "run_program_bytes",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_capture_counting",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_file_counting_runners",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_file_counting",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_with_cfg",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_capture_on_stack",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
    (
        "entry.rs",
        "run_capture_nursery_len",
        "",
        "the test thread that spawned the VM thread joins it and runs no VM code",
    ),
];

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" && name != "golden_tests" {
                    stack.push(path);
                }
            } else if name.ends_with(".rs") && !name.ends_with("tests.rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// `text` without `//` comments, string-literal contents, char literals and inline
/// `#[cfg(test)] mod NAME { .. }` blocks. Every removed line stays as an empty line, so line
/// numbers still match the file.
fn production_code(text: &str) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut skipping = false;
    while let Some(line) = lines.next() {
        if skipping {
            if line.starts_with('}') {
                skipping = false;
            }
            out.push('\n');
            continue;
        }
        if line == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|n| n.starts_with("mod ") && n.trim_end().ends_with('{'))
        {
            lines.next();
            skipping = true;
            out.push_str("\n\n");
            continue;
        }
        out.push_str(&strip_line(line));
        out.push('\n');
    }
    out
}

/// One line without its `//` comment; a string literal keeps its quotes and loses its contents, and
/// a char literal becomes `' '` (so a `'('` cannot unbalance the paren scan).
fn strip_line(line: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            break;
        }
        if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.push_str("' '");
            i += 3;
            continue;
        }
        if c == '\'' && chars.get(i + 1) == Some(&'\\') && chars.get(i + 3) == Some(&'\'') {
            out.push_str("' '");
            i += 4;
            continue;
        }
        if c == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            out.push_str("\"\"");
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// The byte ranges of `code` inside the parentheses of each `gc_blocking(` call.
fn door_spans(code: &str) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(at) = code[from..].find("gc_blocking(") {
        let open = from + at + "gc_blocking".len();
        let mut depth = 0usize;
        let mut close = bytes.len();
        for (k, b) in bytes.iter().enumerate().skip(open) {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = k;
                        break;
                    }
                }
                _ => {}
            }
        }
        spans.push((open, close));
        from = open + 1;
    }
    spans
}

/// For each line of `code`, the name of the innermost function whose body holds it (`""` outside
/// every function). A function nested in another (a `Drop` impl local to a body) ends at its own
/// closing brace, so the lines after it belong to the outer function again.
fn enclosing_fns(code: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut open: Vec<(String, usize)> = Vec::new(); // (name, brace depth inside its body)
    let mut pending: Option<String> = None;
    let mut depth = 0usize;
    for line in code.lines() {
        let words: Vec<&str> = line
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .collect();
        let mut seen_fn = None;
        if let Some(k) = words.iter().position(|w| *w == "fn")
            && let Some(name) = words.get(k + 1)
            && !name.is_empty()
            && line.contains(&format!("fn {name}"))
        {
            seen_fn = Some((*name).to_string());
        }
        // The name a line reports is the function open when the line starts, or the one it opens.
        let mut here = open.last().map(|(n, _)| n.clone()).unwrap_or_default();
        if seen_fn.is_some() {
            pending = seen_fn;
        }
        for c in line.chars() {
            match c {
                '{' => {
                    depth += 1;
                    if let Some(name) = pending.take() {
                        open.push((name.clone(), depth));
                        here = name;
                    }
                }
                '}' => {
                    if open.last().is_some_and(|(_, d)| *d == depth) {
                        open.pop();
                    }
                    depth = depth.saturating_sub(1);
                }
                ';' => pending = None,
                _ => {}
            }
        }
        out.push(here);
    }
    out
}

/// Every hit that is neither in a door nor in an allowed function, and every row no hit used.
fn scan() -> (Vec<String>, Vec<String>) {
    let src = Path::new("src/vm");
    let mut spellings: Vec<&str> = WAITS.to_vec();
    spellings.extend(
        ALLOWED
            .iter()
            .map(|(_, _, s, _)| *s)
            .filter(|s| !s.is_empty()),
    );
    let mut used = vec![false; ALLOWED.len()];
    let mut bad = Vec::new();
    for path in rust_files(src) {
        let rel = path
            .strip_prefix(src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let code = production_code(&fs::read_to_string(&path).unwrap());
        let lines: Vec<&str> = code.lines().collect();
        let fns = enclosing_fns(&code);
        let doors = door_spans(&code);
        let mut line_start = Vec::with_capacity(lines.len());
        let mut off = 0;
        for l in &lines {
            line_start.push(off);
            off += l.len() + 1;
        }
        for (n, l) in lines.iter().enumerate() {
            for s in &spellings {
                let mut from = 0;
                while let Some(at) = l[from..].find(s) {
                    let col = from + at;
                    from = col + s.len();
                    let name = s.trim_start_matches('.').trim_end_matches('(');
                    let name = name.rsplit("::").next().unwrap_or(name);
                    if l.contains(&format!("fn {name}")) {
                        continue; // the wrapper's own definition
                    }
                    // A wrapper row is in use while any call of it is left.
                    if let Some(r) = ALLOWED.iter().position(|(_, _, w, _)| w == s) {
                        used[r] = true;
                    }
                    let pos = line_start[n] + col;
                    if doors.iter().any(|&(a, b)| a < pos && pos < b) {
                        continue;
                    }
                    let f = &fns[n];
                    let row = ALLOWED
                        .iter()
                        .position(|(file, func, _, _)| *file == rel && (*func == "*" || func == f));
                    match row {
                        Some(r) => used[r] = true,
                        None => bad.push(format!(
                            "src/vm/{rel}:{}: `{s}` in fn `{f}` is outside `gc_blocking(..)`",
                            n + 1
                        )),
                    }
                }
            }
        }
    }
    let stale = ALLOWED
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|((file, func, _, why), _)| format!("src/vm/{file} fn `{func}` ({why})"))
        .collect();
    (bad, stale)
}

#[test]
fn every_wait_of_a_vm_thread_goes_through_the_gc_door() {
    let (bad, stale) = scan();
    assert!(
        bad.is_empty(),
        "a blocking call an attached thread can reach holds its GC token, so a stop of the world \
         waits for it forever. Wrap it in `Vm::gc_blocking`, or add a row to ALLOWED with the \
         reason no token is held:\n{}",
        bad.join("\n")
    );
    assert!(
        stale.is_empty(),
        "an ALLOWED row matches no wait any more; delete it:\n{}",
        stale.join("\n")
    );
}

/// The scan itself: a wait inside a door passes, one outside fails, a wrapper's call is a hit.
#[test]
fn the_scan_sees_doors_and_wrappers() {
    let code = production_code(
        "fn a(&mut self) {\n    self.gc_blocking(&[], |_| {\n        cv.wait(g);\n    });\n    cv.wait(g); // \"x.wait(\"\n}\n",
    );
    let doors = door_spans(&code);
    let first = code.find("cv.wait").unwrap();
    let second = code.rfind("cv.wait").unwrap();
    assert!(doors.iter().any(|&(a, b)| a < first && first < b));
    assert!(!doors.iter().any(|&(a, b)| a < second && second < b));
    assert_eq!(code.matches(".wait(").count(), 2, "{code}");
    assert_eq!(enclosing_fns(&code)[4], "a");
    let nested = production_code(
        "fn outer() {\n    struct G;\n    impl Drop for G {\n        fn drop(&mut self) {}\n    }\n    cv.wait(g);\n}\n",
    );
    assert_eq!(
        enclosing_fns(&nested)[5],
        "outer",
        "a nested fn ends at its own brace"
    );
    let gated =
        production_code("#[cfg(test)]\nmod tests {\n    fn f() { cv.wait(g); }\n}\nfn g() {}\n");
    assert!(!gated.contains(".wait("), "{gated}");
    assert_eq!(gated.lines().count(), 5, "line numbers are kept");
}
