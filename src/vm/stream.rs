// vm::stream — the CLI's STREAMING stdout/stderr sink (`HostConfig::stream`, `chezzi run` only).
// The buffered sink (`Vm.out`/`Vm.stderr`) is untouched and stays the parity oracle.
//
// A fiber must never block in `write(2)`: an M:N core worker stuck in a full-pipe write runs no
// other fiber (the D5 invariant — see `native::is_blocking`), and the serial engine's single thread
// would stop dead. So a streamed write is a queue push (never blocks, never syscalls) and ONE
// background thread per stream owns the real handle: one message = one `write_all` = a `print` is
// line-atomic across tasks, and stdout/stderr keep separate locks.
//
// ponytail: the queue is unbounded — a consumer that stalls forever grows memory instead of stalling
// the VM. Swap for a bounded `sync_channel` if a memory ceiling ever matters more than liveness.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

enum Msg {
    Write(String),
    /// Flush the real handle, then ack — the seams (`read_line`, `io.flush()`, process exit) that
    /// must see the bytes land before they proceed. `Stdout` is a `LineWriter`, so this is what makes
    /// a `print("name? ", end="")` prompt appear before the blocking read.
    Flush(Sender<()>),
}

static OUT: OnceLock<Sender<Msg>> = OnceLock::new();
static ERR: OnceLock<Sender<Msg>> = OnceLock::new();

/// A streamed write failed. `print` returns nil — the language has nowhere to report this — and
/// carrying on is strictly worse than stopping: a closed reader (`chezzi run x.chz | head -1`, with
/// SIGPIPE ignored by Rust) would spin forever on a dead pipe, and a full disk would truncate the
/// output while the process still exited 0. So halt, with the CLI's existing policy (`chezzi docs` /
/// `main.rs::write_stdout`): BrokenPipe = the reader went away = clean exit; any other errno = a
/// diagnostic on stderr + FAILURE.
fn fatal(what: &str, e: &std::io::Error) -> ! {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    let _ = writeln!(std::io::stderr(), "chezzi run: cannot write {what}: {e}");
    std::process::exit(1);
}

fn spawn_writer<W: Write + Send + 'static>(mut w: W, what: &'static str) -> Sender<Msg> {
    let (tx, rx) = mpsc::channel::<Msg>();
    std::thread::spawn(move || {
        for msg in rx {
            match msg {
                Msg::Write(s) => {
                    if let Err(e) = w.write_all(s.as_bytes()) {
                        fatal(what, &e);
                    }
                }
                Msg::Flush(ack) => {
                    if let Err(e) = w.flush() {
                        fatal(what, &e);
                    }
                    let _ = ack.send(());
                }
            }
        }
    });
    tx
}

/// Queue `s` for the process's real stdout (the writer thread does the syscall).
pub(super) fn write_out(s: &str) {
    let tx = OUT.get_or_init(|| spawn_writer(std::io::stdout(), "stdout"));
    let _ = tx.send(Msg::Write(s.to_string()));
}

/// Queue `s` for the process's real stderr.
pub(super) fn write_err(s: &str) {
    let tx = ERR.get_or_init(|| spawn_writer(std::io::stderr(), "stderr"));
    let _ = tx.send(Msg::Write(s.to_string()));
}

/// Drain + flush both streamed handles, blocking until the writer threads confirm. A no-op under the
/// buffered sink (no writer was ever spawned) and for a stream that was never written.
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
