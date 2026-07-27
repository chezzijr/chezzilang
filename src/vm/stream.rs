// vm::stream — the CLI's STREAMING stdout/stderr sink (`HostConfig::stream`, `chezzi run` only).
// The buffered sink (`Vm.out`/`Vm.stderr`) is untouched and stays the parity oracle.
//
// A fiber must never block in `write(2)`: an M:N core worker stuck in a full-pipe write runs no
// other fiber (the D5 invariant — see `native::is_blocking`), and the serial engine's single thread
// would stop dead. So a streamed write is a queue push (never blocks, never syscalls) and ONE
// background thread per stream owns the real handle: one message = one `write_all` + `flush` = a
// `print` is line-atomic across tasks, the output is UNBUFFERED (a `print(x, end="")` progress marker
// appears immediately; a killed program keeps every byte it produced), and stdout/stderr keep
// separate locks. NOTHING in the VM ever waits on a writer thread — `io.flush()` / `read_line` /
// `input` only queue, so a stalled consumer can never pin a core worker.
//
// A writer thread NEVER decides the program's fate — it only records. On a failed write it marks its
// stream dead and drops the rest (a dead fd never recovers); the VM notices at its next `print`
// ([`super::Vm::stream_halt`]) and raises an ORDINARY runtime fault, so the run ends non-zero with a
// trace on the still-live stderr. It deliberately does NOT borrow the `std.os.exit` channel: that
// outranks a fault, and using it made `chezzi run x.chz | head -1` report SUCCESS for a program that
// crashed. Nothing here calls `std::process::exit` either: this is library code (`chezzi-lsp`,
// embedders), two writer threads racing libc `exit(3)` is UB, and a thread that kills the process
// discards the run's real outcome — its fault trace, its `os.exit(n)`, and the sibling's queued bytes.
//
// STDERR is a diagnostic channel: a write failure on it is swallowed (marked dead, further writes
// dropped). A dead stderr reader is not a reason to kill a healthy program, and there is nowhere left
// to report the failure to anyway.
//
// ponytail: the queue is unbounded — a consumer that stalls forever grows memory instead of stalling
// the VM. Swap for a bounded `sync_channel` if a memory ceiling ever matters more than liveness.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};

enum Msg {
    /// Raw BYTES, never `String`: `Writer.write_bytes(b"\xff\xfe")` must reach the real handle
    /// unchanged (W6-9 — a `String` hop round-tripped it through `from_utf8_lossy`).
    Write(Vec<u8>),
    /// Flush the real handle, then ack. Only [`flush_stream`] (i.e. `main`, after the VM has
    /// finished) waits on this — a fiber must never block on stdout's consumer.
    Flush(Sender<()>),
}

static OUT: OnceLock<Sender<Msg>> = OnceLock::new();
static ERR: OnceLock<Sender<Msg>> = OnceLock::new();
/// stdout's handle failed: further writes are dropped and the next `print` halts the VM cleanly.
static OUT_DEAD: AtomicBool = AtomicBool::new(false);
/// The stdout failure was NOT a closed reader (ENOSPC, EBADF…) — `main` turns this into a diagnostic
/// and FAILURE, so a truncated redirect can never report success. A closed reader (`| head -1`)
/// leaves it unset: that is a clean end, not an error.
static OUT_ERR: OnceLock<String> = OnceLock::new();

fn spawn_writer<W: Write + Send + 'static>(mut w: W, is_stdout: bool) -> Sender<Msg> {
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::spawn(move || {
        let mut dead = false;
        for msg in rx {
            if dead {
                continue; // a failed handle never recovers — drop the rest, keep draining the queue
            }
            let r = match msg {
                // write + flush: the streamed handles are UNBUFFERED (`Stdout` is a `LineWriter`,
                // which would sit on a `print(x, end="")` until the next newline).
                Msg::Write(v) => w.write_all(&v).and_then(|()| w.flush()),
                Msg::Flush(ack) => {
                    let r = w.flush();
                    let _ = ack.send(());
                    r
                }
            };
            if let Err(e) = r {
                dead = true;
                if is_stdout {
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        let _ = OUT_ERR.set(e.to_string());
                    }
                    OUT_DEAD.store(true, Ordering::Release);
                }
            }
        }
    });
    tx
}

/// Queue `b` for the process's real stdout (the writer thread does the syscall). Once stdout is dead
/// the bytes are dropped; the run is halted separately by [`super::Vm::stream_halt`] at the print site.
pub(super) fn write_out(b: &[u8]) {
    let tx = OUT.get_or_init(|| spawn_writer(std::io::stdout(), true));
    let _ = tx.send(Msg::Write(b.to_vec()));
}

/// Why the streamed stdout is dead, as the message of the fault a print site raises — `None` while it
/// is healthy. A closed reader (`| head -1`) is the common case and reads as such; any other failure
/// (ENOSPC, EBADF…) carries the OS error, so a truncated redirect can never look like a clean run.
pub fn out_dead_reason() -> Option<String> {
    if !OUT_DEAD.load(Ordering::Acquire) {
        return None;
    }
    Some(match OUT_ERR.get() {
        Some(e) => format!("stdout write failed: {e}"),
        None => "stdout closed (broken pipe)".to_string(),
    })
}

/// Queue `b` for the process's real stderr. A failure there is swallowed (diagnostic channel).
pub(super) fn write_err(b: &[u8]) {
    let tx = ERR.get_or_init(|| spawn_writer(std::io::stderr(), false));
    let _ = tx.send(Msg::Write(b.to_vec()));
}

/// Drain + flush both streamed handles, blocking until the writer threads confirm. Called by `main`
/// AFTER the VM has finished (never from a fiber), so no exit path — `os.exit`, a fatal trace, a
/// trailing partial line — can lose a queued byte. A no-op under the buffered sink (no writer was
/// ever spawned).
pub fn flush_stream() {
    for cell in [&OUT, &ERR] {
        if let Some(tx) = cell.get() {
            let (ack, rx) = mpsc::channel();
            if tx.send(Msg::Flush(ack)).is_ok() {
                let _ = rx.recv();
            }
        }
    }
}

/// The stdout write failure that was NOT a closed reader, if one happened — `main` reports it and
/// fails the run. Call after [`flush_stream`].
pub fn stream_error() -> Option<&'static str> {
    OUT_ERR.get().map(String::as_str)
}
