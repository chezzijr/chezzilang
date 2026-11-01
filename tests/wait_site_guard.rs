//! M1.5 — every blocking call a VM thread can reach goes through the one wait door, `Vm::wait_door`.
//!
//! A thread that holds a GC token (`gc_coord::Attached`) must reach a safepoint or detach in bounded
//! time: a stop of the world waits for it. So a wait of an attached thread runs inside the closure
//! of a `wait_door(..)` call, which detaches the thread for the wait and, for a listed `WaitSpec`,
//! holds no run permit. The steps come from the wait's kind (`block::wait_steps`). This scan reads the
//! production code of `src/vm` (comments, string contents and inline `#[cfg(test)] mod NAME { .. }`
//! blocks removed; `tests/`, `golden_tests/` and `*tests.rs` skipped) and finds each blocking call:
//! a condvar or barrier wait, a sleep, a park, a channel receive, a thread join, a `poll(2)`. Each
//! hit must sit inside the parentheses of a door call ([`DOORS`]: `wait_door(` or a wrapper of
//! it), or inside a function on [`ALLOWED`] with its reason.
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
//! - a wait passed the wrong `WaitSpec` (`tests/wait_steps_grid.rs` runs the cells);
//! - a `Drop` that makes a syscall (M5d: no `Drop` in `src/vm` writes to an fd; a dropped core
//!   closes its fd);
//! - a row is keyed by file and function name, so a new wait in an allowed function passes;
//! - a value the door must carry: a `GcRef` held in a Rust local across a door must be in the
//!   door's `pins` or on the VM stack. `tests/gc_blocking_pins.rs` checks that under GC stress,
//!   one program per door kind; this scan does not.

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
        "sysmon.rs",
        "watch_loop",
        "",
        "the watcher thread of long C calls never attaches",
    ),
    (
        "sysmon.rs",
        "new_slot",
        "",
        "the first C call waits only for the watcher thread's start, a bounded wait",
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
        "worker_loop.rs",
        "sched_seed_perturb",
        "",
        "seed jitter, bounded 50 us; ops hold popped operands in Rust locals",
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
        "take",
        "RunPermit::take(",
        "takes the run's first permit; each call is in a door",
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

/// The doors: `Vm::wait_door`, and the functions that run their closure through it: `Vm::wait_once`,
/// a wait of one tick, `Vm::wait_tick`, a tick of a gated loop, and `Vm::wait_call`, a native or C
/// call that holds its thread (`wait_once` plus the halt at the return). `doors_reach_the_wait_door`
/// checks that each wrapper's body calls a door before it in this list.
const DOORS: [&str; 4] = ["wait_door", "wait_once", "wait_tick", "wait_call"];

/// The byte ranges of `code` inside the parentheses of each call of a door.
fn door_spans(code: &str) -> Vec<(usize, usize)> {
    DOORS
        .iter()
        .flat_map(|door| door_spans_of(code, door))
        .collect()
}

/// The byte ranges of `code` inside the parentheses of each `door(` call.
fn door_spans_of(code: &str, door: &str) -> Vec<(usize, usize)> {
    let bytes = code.as_bytes();
    let mut spans = Vec::new();
    let mut from = 0;
    let call = format!("{door}(");
    while let Some(at) = code[from..].find(&call) {
        let open = from + at + door.len();
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
                            "src/vm/{rel}:{}: `{s}` in fn `{f}` is outside `wait_door(..)`",
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
         waits for it forever. Wrap it in `Vm::wait_door`, or add a row to ALLOWED with the \
         reason no token is held:\n{}",
        bad.join("\n")
    );
    assert!(
        stale.is_empty(),
        "an ALLOWED row matches no wait any more; delete it:\n{}",
        stale.join("\n")
    );
}

/// Each door but `wait_door` is a wrapper: its own body must open the wait door.
#[test]
fn doors_reach_the_wait_door() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/vm");
    let mut files = Vec::new();
    for e in std::fs::read_dir(&root).expect("read src/vm") {
        let p = e.expect("dir entry").path();
        if p.extension().is_some_and(|x| x == "rs") {
            files.push(production_code(&std::fs::read_to_string(&p).expect("read")));
        }
    }
    for (k, door) in DOORS.iter().enumerate().skip(1) {
        let def = format!("fn {door}");
        let body = files
            .iter()
            .find_map(|code| {
                let at = code.find(&def)?;
                let open = at + code[at..].find('{')?;
                let mut depth = 0usize;
                for (k, b) in code.bytes().enumerate().skip(open) {
                    match b {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(code[open..k].to_string());
                            }
                        }
                        _ => {}
                    }
                }
                None
            })
            .unwrap_or_else(|| panic!("door `{door}` has no definition in src/vm"));
        assert!(
            DOORS[..k].iter().any(|d| body.contains(&format!("{d}("))),
            "door `{door}` calls no earlier door in its body"
        );
    }
}

/// An `extern` C call takes its permit steps, its counts and its halt from the wait table
/// (`WaitSpec::Extern`): each `cffi.call(` sits inside a `wait_call(..)`, never in a bare door.
#[test]
fn every_extern_call_takes_its_steps_from_the_wait_table() {
    let src = Path::new("src/vm");
    let mut calls = 0;
    let mut bad = Vec::new();
    for path in rust_files(src) {
        let code = production_code(&fs::read_to_string(&path).unwrap());
        let spans = door_spans_of(&code, "wait_call");
        let mut from = 0;
        while let Some(at) = code[from..].find("cffi.call(") {
            let pos = from + at;
            from = pos + 1;
            calls += 1;
            if !spans.iter().any(|&(a, b)| a < pos && pos < b) {
                let line = code[..pos].lines().count();
                bad.push(format!("{}:{line}", path.display()));
            }
        }
    }
    assert!(calls > 0, "the scan found no `cffi.call(` in src/vm");
    assert!(
        bad.is_empty(),
        "an extern call outside `Vm::wait_call(Some(WaitSpec::Extern), ..)` keeps its permit with \
         no watcher and misses the halt at its return:\n{}",
        bad.join("\n")
    );
}

/// The scan itself: a wait inside a door passes, one outside fails, a wrapper's call is a hit.
#[test]
fn the_scan_sees_doors_and_wrappers() {
    let code = production_code(
        "fn a(&mut self) {\n    self.wait_door(None, &[], |_| {\n        cv.wait(g);\n    });\n    cv.wait(g); // \"x.wait(\"\n}\n",
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

/// The spellings that take a step of a wait by hand: the run permit and the demote bracket.
/// The hand-over mechanisms (`yield_pool_slot`, `demote_once`, `hand_joiner_slot`) are not listed:
/// a wait takes them through `Vm::block_enter` or `Vm::joiner_shell`, whose decision is the table's
/// `Handover`; a preempted runner (`worker_loop.rs`) and an in-place tick's pool yield
/// (`blocking.rs`) are not waits.
const STEPS: &[&str] = &[
    "width_release(",
    "width_acquire(",
    "width::release(",
    "width::acquire(",
    "width::hand_off(",
    "width::forfeit(",
    "block_enter(",
];

/// The files that own a step: the table and its door (`block.rs`, `gc_door.rs`), the permit gate
/// (`width.rs`), the runner loop that hands its permit over between fibers (`worker_loop.rs`) and
/// the watcher that hands over the permit of a long C call (`sysmon.rs`, `Permit::Watched`).
const STEP_OWNERS: &[&str] = &[
    "block.rs",
    "gc_door.rs",
    "width.rs",
    "worker_loop.rs",
    "sysmon.rs",
];

/// (file, function, why) for a call of the permit owner's own outside [`STEP_OWNERS`].
const STEP_ALLOWED: &[(&str, &str, &str)] = &[
    (
        "mnsched_take.rs",
        "take_runnable_inner",
        "an idle worker releases its permit at each pass and takes one only for work (TICKET-230)",
    ),
    (
        "exec.rs",
        "reentered",
        "a panic out of a callback re-takes the permit before an FFI trampoline resumes Chezzi code",
    ),
    (
        "pool.rs",
        "worker_loop",
        "a pool thread returns its permit at the end of a job",
    ),
];

/// A wait takes its steps from its `WaitSpec` through the one door (`Vm::wait_begin`,
/// `Vm::wait_door`): no other site releases the permit or demotes by hand.
#[test]
fn every_wait_step_comes_from_the_table() {
    let src = Path::new("src/vm");
    let mut used = vec![false; STEP_ALLOWED.len()];
    let mut bad = Vec::new();
    for path in rust_files(src) {
        let rel = path
            .strip_prefix(src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if STEP_OWNERS.contains(&rel.as_str()) {
            continue;
        }
        let code = production_code(&fs::read_to_string(&path).unwrap());
        let fns = enclosing_fns(&code);
        for (n, l) in code.lines().enumerate() {
            for s in STEPS {
                if !l.contains(s) {
                    continue;
                }
                match STEP_ALLOWED
                    .iter()
                    .position(|(file, func, _)| *file == rel && *func == fns[n])
                {
                    Some(r) => used[r] = true,
                    None => bad.push(format!("src/vm/{rel}:{}: `{s}` in fn `{}`", n + 1, fns[n])),
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a site takes a wait step by hand. Name the wait's `WaitSpec` and use `Vm::wait_begin` / \
         `Vm::wait_door`:\n{}",
        bad.join("\n")
    );
    let stale: Vec<String> = STEP_ALLOWED
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|((file, func, why), _)| format!("src/vm/{file} fn `{func}` ({why})"))
        .collect();
    assert!(
        stale.is_empty(),
        "a STEP_ALLOWED row matches no call any more; delete it:\n{}",
        stale.join("\n")
    );
}

/// The spellings that register a wait's counts for the deadlock verdicts by hand, each with the
/// files that own it: `block.rs` takes the counts from `block::wait_steps`, `quiesce.rs` holds the
/// party registry, and `mnsched.rs` holds the waiter registry and the body marks it publishes.
/// A parked fiber counts through its park, so the park owners hold the rest: [`PARK_OWNERS`].
const COUNTS: &[(&str, &[&str])] = &[
    ("block_shared(", &["block.rs", "quiesce.rs"]),
    ("register_waiter(", &["block.rs", "mnsched.rs"]),
    ("set_body_wait(", &["block.rs", "mnsched.rs"]),
    ("owner_blocked(", &["block.rs", "mnsched.rs"]),
    ("owners_blocked +=", &["block.rs", "mnsched_take.rs"]),
    ("joins_blocked +=", &["block.rs", "mnsched_take.rs"]),
    ("inflight.fetch_add(", PARK_OWNERS),
    ("block_party_guard(", &[]),
    ("nursery_party_guard(", &[]),
    ("blocked_bodies_guard(", &[]),
    ("blocked_owner_guard(", &["block.rs"]),
];

/// The files whose park paths count a parked fiber themselves: `block.rs` (a demoted wait's
/// `inflight`), `mnsched_take.rs` (`MnSched::park_join`'s owner and join counts),
/// `mnsched_verdict.rs` (the offload and netpoller parks' `inflight`), and `channel.rs` and
/// `blocking.rs` (a timer park's pending `inflight`).
const PARK_OWNERS: &[&str] = &[
    "block.rs",
    "mnsched_take.rs",
    "mnsched_verdict.rs",
    "channel.rs",
    "blocking.rs",
];

/// A wait takes its verdict counts from its `WaitSpec` through `Vm::wait_begin` or
/// `Vm::nursery_counts`: no other site registers a party, a waiter, a body mark or a blocked owner.
#[test]
fn every_wait_count_comes_from_the_table() {
    let src = Path::new("src/vm");
    let mut bad = Vec::new();
    for path in rust_files(src) {
        let rel = path
            .strip_prefix(src)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let code = production_code(&fs::read_to_string(&path).unwrap());
        for (n, l) in code.lines().enumerate() {
            for (s, owners) in COUNTS {
                if l.contains(s) && !owners.contains(&rel.as_str()) {
                    bad.push(format!("src/vm/{rel}:{}: `{s}`", n + 1));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a site registers a deadlock-verdict count by hand. Name the wait's `WaitSpec` and use \
         `Vm::wait_begin` (or `Vm::nursery_counts` for a nursery join):\n{}",
        bad.join("\n")
    );
}
