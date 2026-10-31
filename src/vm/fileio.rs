// vm::fileio — R2 `Writer` / file-handle methods + openers. `super::*` == the `vm` module.
// Mirrors vm::netio's Socket handle dispatch, but blocking-classified with NO netpoller/park
// (file writes are synchronous blocking syscalls; one on a FIFO, a pipe or a device runs in the
// `HostFd` row's wait, which hands its runner slot over). The stdout/stderr backings route through `Vm::emit_out`/`emit_err` (`Vm.out`, what the golden test
// suite captures), never a raw fd.

use super::*;
use crate::base::builtin_method::{ReaderMethod, WriterMethod};
use crate::native::strip_eol;

/// R2 — the outcome of a low-level write to a [`WriterCore`].
enum WriteErr {
    /// The backing was `close()`d (its `Option` is `None`) — a clean use-after-close fault.
    Closed,
    /// A real I/O error (ENOSPC, a broken pipe, …), rendered for the `Result::Err` payload.
    Io(String),
    /// The wait could not demote the worker: the op faults.
    Fault(RuntimeError),
}

/// What a write to a writer core leaves to do once its lock is released.
enum Wrote {
    Done,
    Stdout,
    Stderr,
    /// a `Buffered` core reached its cap: write the drained bytes to its inner core
    Drain(Arc<WriterCore>, Vec<u8>),
}

/// W6-1 — re-key an error raised by a core BENEATH the receiver. `writer_method` renders `Closed`
/// receiver-relatively ("write/flush on a closed writer"), which is a lie when it is the *inner*
/// handle a `buffered(...)` writer drains into that was closed — the receiver is wide open. Demote it
/// to `Io` so the message names the right handle AND `close()` (which masks `Closed` for idempotence)
/// cannot report success for a flush that provably persisted nothing.
fn from_inner(e: WriteErr) -> WriteErr {
    match e {
        WriteErr::Closed => {
            WriteErr::Io("the inner writer this buffer drains into is closed".into())
        }
        other @ (WriteErr::Io(_) | WriteErr::Fault(_)) => other,
    }
}

/// What a FIFO, pipe or device wait names in the OS-thread-limit fault.
const FD_WAIT: &str = "a FIFO or pipe syscall";

impl Vm {
    /// R2 — clone out the shared `Arc<WriterCore>` behind a `Writer` handle (refcount bump), mirroring
    /// [`Vm::socket_core`]. Held only for the calling method, so locking the backing does not borrow
    /// the heap.
    pub(super) fn writer_core(&self, h: GcRef) -> Arc<WriterCore> {
        match self.heap.get(h) {
            Obj::Writer(core) => Arc::clone(core),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => unreachable!("writer_core on non-writer"),
        }
    }

    /// M1.5 — run `op` under a writer core's lock. With the lock free and `local` true no
    /// syscall can happen, and `op` runs attached; else the lock and the write run through
    /// [`Vm::wait_once`] with the fd's row (Go `entersyscall` only around a real syscall).
    fn with_writer_lock<R>(
        &mut self,
        core: &WriterCore,
        span: Span,
        local: impl FnOnce(&Option<Backing>) -> bool,
        op: impl FnOnce(&mut Option<Backing>) -> R,
    ) -> Result<R, RuntimeError> {
        if let Ok(mut guard) = core.inner.try_lock()
            && local(&guard)
        {
            return Ok(op(&mut guard));
        }
        self.wait_once(WaitSpec::of_fd(core.waits), FD_WAIT, span, &[], |_| {
            op(&mut core.inner.lock().unwrap_or_else(|e| e.into_inner()))
        })
    }

    /// R2 — write `data` to a writer core. `File` → `write_all` on the fd (M5c: unbuffered, Go
    /// `os.File`); `Stdout`/`Stderr` →
    /// hand the RAW bytes to the [`Vm::emit_out_bytes`]/[`Vm::emit_err_bytes`] sink (`Vm.out` —
    /// NEVER a raw fd), which is byte-typed end to end, so `write_bytes(b"\xff\xfe")` is byte-exact
    /// on the console exactly as it already was on a file (W6-9); `Buffered` → append to the in-VM
    /// buffer and drain to the inner core once it reaches `cap`. Returns the byte count on success —
    /// no backing can short-write (`write_all` / in-memory / an unbounded queue).
    fn write_to_core(
        &mut self,
        core: &WriterCore,
        data: &[u8],
        span: Span,
    ) -> Result<usize, WriteErr> {
        // Take the drain decision under the lock, then route to the inner core with the lock RELEASED
        // (the inner core has its own Mutex; a Stdout inner needs `&mut self`).
        // M1.5 — a file write reaches the fd (a FIFO or a pipe blocks), so it runs with the lock
        // through the GC door; a write a `Buffered` core keeps in its buffer runs attached. The
        // console sinks are written after the lock is released.
        let next = self
            .with_writer_lock(core, span, writes_are_buffered, |g| write_locked(g, data))
            .map_err(WriteErr::Fault)??;
        match next {
            Wrote::Done => {}
            Wrote::Stdout => self.emit_out_bytes(data),
            Wrote::Stderr => self.emit_err_bytes(data),
            Wrote::Drain(inner, drained) => {
                self.write_to_core(&inner, &drained, span)
                    .map_err(from_inner)?;
            }
        }
        Ok(data.len())
    }

    /// R2 — flush a writer core. `File` and `Stdout`/`Stderr` → an honest no-op: each holds no
    /// buffer (M5c: a file writer is unbuffered, Go `os.File`; a flush is not `File.Sync`).
    /// `Buffered` → drain the in-VM buffer to the inner core, THEN flush the inner.
    fn flush_core(&mut self, core: &WriterCore, span: Span) -> Result<(), WriteErr> {
        // No backing's flush reaches the fd (a file holds no buffer; a `Buffered` drain is a write
        // on the inner core), so it runs attached unless the lock is busy.
        let drain = self
            .with_writer_lock(core, span, |_| true, flush_locked)
            .map_err(WriteErr::Fault)??;
        if let Some((inner, drained)) = drain {
            // Guard the WRITE, not the flush: an empty `write_to_core` on a `Stdout`/`Stderr` inner
            // would hand `emit_out("")` to the captured `Vm.out` buffer / stream queue.
            if !drained.is_empty() {
                self.write_to_core(&inner, &drained, span)
                    .map_err(from_inner)?;
            }
            self.flush_core(&inner, span).map_err(from_inner)?;
        }
        Ok(())
    }

    /// R2 — `Writer` methods: `write(s) -> Result[int]` / `write_bytes(b) -> Result[int]` (bytes
    /// written), `flush() -> Result[nil]`, `close() -> Result[nil]` (flush + take/drop the backing).
    /// A method on a closed writer is a clean `Result::Err`, never a panic (the `Mutex<Option>`
    /// guarantee, mirroring `Socket`). No netpoller park — file writes block synchronously.
    pub(super) fn writer_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = WriterMethod::from_name(method) else {
            return Err(self.err(format!("type Writer has no method '{method}'"), span));
        };
        match which {
            WriterMethod::Write | WriterMethod::WriteBytes => {
                self.arity_err(method, args, 1, span)?;
                let data = if which == WriterMethod::WriteBytes
                    && let Some(v) = args.first()
                {
                    self.collect_bytes_arg("write_bytes", *v, span)?
                } else if let Some(v) = args.first()
                    && let Some(sh) = v.as_obj()
                    && let Obj::Str(s) = self.heap.get(sh)
                {
                    s.as_bytes().to_vec()
                } else {
                    return Err(self.err("write expects a str".into(), span));
                };
                let core = self.writer_core(h);
                match self.write_to_core(&core, &data, span) {
                    Ok(n) => Ok(self.sock_ok(Value::int(n as i64))),
                    Err(WriteErr::Closed) => Ok(self.sock_err("write on a closed writer")),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                    Err(WriteErr::Fault(e)) => Err(e),
                }
            }
            WriterMethod::Flush => {
                self.arity_err("flush", args, 0, span)?;
                let core = self.writer_core(h);
                match self.flush_core(&core, span) {
                    Ok(()) => Ok(self.sock_ok(Value::nil())),
                    Err(WriteErr::Closed) => Ok(self.sock_err("flush on a closed writer")),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                    Err(WriteErr::Fault(e)) => Err(e),
                }
            }
            WriterMethod::Close => {
                self.arity_err("close", args, 0, span)?;
                let core = self.writer_core(h);
                // Flush the tail first, THEN take + drop the backing (closing the fd). A flush error on
                // close still surfaces, but the backing is dropped regardless (no leaked fd).
                let flushed = self.flush_core(&core, span);
                // M1.5 — the drop closes the fd, through the door (a FIFO close can wait on its lock).
                self.wait_once(WaitSpec::of_fd(core.waits), FD_WAIT, span, &[], |_| {
                    close_slot(&core.inner)
                })?;
                match flushed {
                    Ok(()) | Err(WriteErr::Closed) => Ok(self.sock_ok(Value::nil())),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                    Err(WriteErr::Fault(e)) => Err(e),
                }
            }
        }
    }

    /// R2 — dispatch the `std.io` openers/handles `invoke_native` intercepts on their
    /// [`crate::native::Kind::InterceptIo`] registry entry. Each
    /// allocates a heap `Writer` handle over an `Arc`'d core (a pure off-heap native cannot), mirroring
    /// `std.net`'s `connect`/`listen`.
    pub(super) fn io_native(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // W7-8 — the path-taking openers are the `_`-prefixed INTERNAL byte seam (`_create`/`_append`/
        // `_open`); the public names are `PathLike` wrappers in `std/io.chz`. `stdout`/`stderr`/
        // `buffered` take no path and kept their names.
        match name {
            "_create" | "_append" => self.io_open(&name[1..], &args, span),
            "_open" => self.io_open_reader(&args, span),
            "stdout" => Ok(self.io_std_handle(Backing::Stdout)),
            "stderr" => Ok(self.io_std_handle(Backing::Stderr)),
            "buffered" => self.io_buffered(&args, span),
            _ => unreachable!("io_native on '{name}'"),
        }
    }

    /// R2 — `io.create(path)` = truncate + create; `io.append(path)` = append, create-if-absent (never
    /// truncates). Returns `Ok(Writer)` or a clean `Err` (perms, missing dir).
    fn io_open(&mut self, verb: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // W7-8 — the opener's path arg is RAW `bytes` now (the public `PathLike` wrapper in
        // `std/io.chz` calls `p.as_path()` first), so a non-UTF-8 filename opens byte-exactly.
        let path = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Bytes(b) = self.heap.get(sh)
        {
            crate::native::fs::bytes_path(b)
        } else {
            return Err(self.err(format!("io.{verb} expects a path"), span));
        };
        // M1.5 — an open blocks (a FIFO waits for its reader): it runs through the door.
        let waits = self.wait_door(&[], |_| path_waits(&path));
        let opened =
            self.wait_once(WaitSpec::of_fd(waits), FD_WAIT, span, &[], |_| match verb {
                "create" => std::fs::File::create(&path),
                "append" => std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path),
                _ => unreachable!("io_open on '{verb}'"),
            })?;
        match opened {
            Ok(f) => {
                let core = Arc::new(WriterCore {
                    inner: Mutex::new(Some(Backing::File(f))),
                    key: core::next_poll_key(),
                    waits,
                });
                let v = Value::obj(self.heap.alloc(Obj::Writer(core)));
                Ok(self.sock_ok(v))
            }
            Err(e) => Ok(self.sock_err(format!("{}: {e}", path.display()))),
        }
    }

    /// R2 — `io.stdout()` / `io.stderr()`: a FRESH stream `Writer` per call, routing through the
    /// existing `Vm::emit_out`/`emit_err` sink. NOT a `Result` (opening a stream can't fail).
    fn io_std_handle(&mut self, backing: Backing) -> Value {
        let core = Arc::new(WriterCore {
            inner: Mutex::new(Some(backing)),
            key: core::next_poll_key(),
            waits: false,
        });
        Value::obj(self.heap.alloc(Obj::Writer(core)))
    }

    /// R2 — `io.buffered(w, size = 8192)`: wrap a `Writer` in a `Backing::Buffered` that accumulates in
    /// the VM and drains to `w` on flush / buffer-full / close (the Go `bufio.NewWriter` escape hatch).
    fn io_buffered(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let inner = if let Some(v) = args.first()
            && let Some(wh) = v.as_obj()
            && matches!(self.heap.get(wh), Obj::Writer(_))
        {
            self.writer_core(wh)
        } else {
            return Err(self.err("buffered expects a Writer".into(), span));
        };
        let cap = match args.get(1) {
            None => 8192,
            Some(v) if self.is_integral(*v) => self.int_of(*v).max(1) as usize,
            Some(_) => return Err(self.err("buffered size expects an int".into(), span)),
        };
        let core = Arc::new(WriterCore {
            inner: Mutex::new(Some(Backing::Buffered {
                inner,
                buf: Vec::new(),
                cap,
            })),
            key: core::next_poll_key(),
            waits: false,
        });
        Ok(Value::obj(self.heap.alloc(Obj::Writer(core))))
    }

    // ===== R2b — `Reader` / read-only file handle (the input twin of `Writer`) =====

    /// R2b — clone out the shared `Arc<ReaderCore>` behind a `Reader` handle (refcount bump), mirroring
    /// [`Vm::writer_core`]. Held only for the calling method, so locking the reader does not borrow the
    /// heap.
    pub(super) fn reader_core(&self, h: GcRef) -> Arc<ReaderCore> {
        match self.heap.get(h) {
            Obj::Reader(core) => Arc::clone(core),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Generator(_) => unreachable!("reader_core on non-reader"),
        }
    }

    /// M1.5 — run `op` under the reader core's two locks. With both locks free and `buffered`
    /// true no syscall can happen, and `op` runs attached; else the locks and the read run through
    /// [`Vm::wait_once`] with the fd's row (Go `entersyscall` only around a real syscall).
    fn with_reader_locks<R>(
        &mut self,
        core: &ReaderCore,
        span: Span,
        buffered: impl FnOnce(&core::ReaderCarry, &Option<std::io::BufReader<std::fs::File>>) -> bool,
        op: impl FnOnce(&mut core::ReaderCarry, &mut Option<std::io::BufReader<std::fs::File>>) -> R,
    ) -> Result<R, RuntimeError> {
        // LOCK ORDER: `carry` OUTER, `inner` INNER (see `ReaderCore::carry`). A `try_lock` never
        // waits, so the fast path cannot block attached.
        if let Ok(mut carry) = core.carry.try_lock()
            && let Ok(mut guard) = core.inner.try_lock()
            && buffered(&carry, &guard)
        {
            return Ok(op(&mut carry, &mut guard));
        }
        self.wait_once(WaitSpec::of_fd(core.waits), FD_WAIT, span, &[], |_| {
            let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
            let mut guard = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            op(&mut carry, &mut guard)
        })
    }

    /// R2b — `Reader` methods, the input twin of `writer_method`:
    /// * `read_line() -> Option[str]` — one line, trailing `\n` (and a preceding `\r`) stripped; `None`
    ///   at EOF. Matches the existing module-level `io.read_line()` shape (anti-drift). An IO error or a
    ///   non-UTF-8 file is a clean runtime FAULT pointing at `read_bytes` (Option can't carry an Err —
    ///   mirrors `read_file`'s non-UTF-8 fault); a read on a CLOSED reader is likewise a clean fault.
    ///   W7-9: the non-UTF-8 fault is NON-DESTRUCTIVE — the refused line's raw bytes stay in
    ///   [`ReaderCore::carry`], so it is STICKY (a re-read re-faults, never skips) until `read_bytes`
    ///   drains it or `close` discards it.
    /// * `read_bytes(n) -> Result[bytes]` — at-most-`n` bytes (exactly-`n` until a short final chunk);
    ///   empty bytes = EOF; `Err` on closed/IO. `n <= 0` clamps to `Ok(b"")`. The binary + error-
    ///   distinguishing escape hatch — and the carry drain: a pending carry is served first, without
    ///   touching the fd (a carry-only short read, the `socket_read_bytes` shape).
    /// * `close() -> Result[nil]` — idempotent: take + drop the `BufReader` (the fd closes on drop),
    ///   and discard any carry. Every read arm checks `inner.is_none()` BEFORE serving the carry, so
    ///   a carry can neither leak past `close` nor resurrect after EOF.
    ///
    /// These three are the WHOLE Reader dispatch surface (`call.rs` routes every `Reader` method
    /// here; an unknown one faults below). The fourth read path, `lines()`, is a BODIED pure-Chezzi
    /// generator over `read_line` (`std/io.chz`) — it inherits the carry and the stickiness for free.
    ///
    /// No netpoller park — file reads block synchronously. M1.5 — a read that can make a syscall
    /// runs detached through [`Vm::wait_once`] (`with_reader_locks`), so a slow FIFO or pipe read
    /// stalls no stop of the world, and it hands its runner slot over, so the FIFO's writer runs.
    pub(super) fn reader_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = ReaderMethod::from_name(method) else {
            return Err(self.err(format!("type Reader has no method '{method}'"), span));
        };
        match which {
            ReaderMethod::ReadLine => {
                self.arity_err("read_line", args, 0, span)?;
                let core = self.reader_core(h);
                let outcome =
                    self.with_reader_locks(&core, span, line_is_buffered, read_line_locked)?;
                match outcome {
                    Ok(Some(line)) => {
                        let sv = self.alloc_str(line);
                        Ok(self.alloc_enum("Option", "Some", &[sv]))
                    }
                    Ok(None) => Ok(self.alloc_enum("Option", "None", &[])),
                    Err(None) => Err(self.err("read_line on a closed reader".into(), span)),
                    Err(Some(e)) => Err(self.err(
                        format!("{e} — read binary files with Reader.read_bytes"),
                        span,
                    )),
                }
            }
            ReaderMethod::ReadBytes => {
                self.arity_err("read_bytes", args, 1, span)?;
                let n = match args.first() {
                    Some(v) if self.is_integral(*v) => self.int_of(*v).max(0) as u64,
                    _ => return Err(self.err("read_bytes expects an int byte count".into(), span)),
                };
                let core = self.reader_core(h);
                let outcome = self.with_reader_locks(
                    &core,
                    span,
                    |carry, guard| bytes_are_buffered(carry, guard, n),
                    |carry, guard| read_bytes_locked(carry, guard, n),
                )?;
                match outcome {
                    Ok(buf) => {
                        let bv = Value::obj(self.heap.alloc(Obj::Bytes(buf.into_boxed_slice())));
                        Ok(self.sock_ok(bv))
                    }
                    Err(None) => Ok(self.sock_err("read_bytes on a closed reader")),
                    Err(Some(e)) => Ok(self.sock_err(e)),
                }
            }
            ReaderMethod::Close => {
                self.arity_err("close", args, 0, span)?;
                let core = self.reader_core(h);
                // Take + drop the reader (closing the fd). Idempotent: an already-closed reader is Ok.
                // LOCK ORDER: `carry` OUTER, `inner` INNER — the one ordering `ReaderCore::carry`
                // forbids is inner-then-carry, so close takes the carry first. Closed is closed: the
                // carry is discarded with the fd (W7-9).
                // M1.5 — the locks can wait for a read in progress: through the GC door.
                self.wait_once(WaitSpec::of_fd(core.waits), FD_WAIT, span, &[], |_| {
                    let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                    close_slot(&core.inner);
                    carry.reset();
                })?;
                Ok(self.sock_ok(Value::nil()))
            }
        }
    }

    /// R2b — `io.open(path)` = open a file read-only. Returns `Ok(Reader)` or a clean `Err` (missing
    /// file, perms). The read twin of `io.create`/`io.append`.
    fn io_open_reader(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // W7-8 — raw `bytes` path (see `io_open`).
        let path = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Bytes(b) = self.heap.get(sh)
        {
            crate::native::fs::bytes_path(b)
        } else {
            return Err(self.err("io.open expects a path".into(), span));
        };
        // M1.5 — the open (a FIFO waits for its writer) and the directory probe run through the
        // door; the handle is allocated after it.
        let waits = self.wait_door(&[], |_| path_waits(&path));
        let opened = self.wait_once(
            WaitSpec::of_fd(waits),
            FD_WAIT,
            span,
            &[],
            |_| -> std::io::Result<Result<std::fs::File, String>> {
                let f = std::fs::File::open(&path)?;
                // `File::open` SUCCEEDS on a directory (Linux), which deferred the real failure to
                // every subsequent read — with `read_line` advising `read_bytes`, which fails too.
                // Reject at the call like `io.read_file` does (Python `open(dir)` → IsADirectoryError).
                // The error text comes from a real 1-byte read, so it is the OS's own wording — the
                // same string `io.read_file(dir)` already produces ("Is a directory (os error 21)"),
                // not a second spelling of the same condition.
                if f.metadata().is_ok_and(|m| m.is_dir()) {
                    let mut probe = [0u8; 1];
                    let msg = match std::io::Read::read(&mut &f, &mut probe) {
                        Err(e) => format!("{e}"),
                        Ok(_) => format!("{}", std::io::ErrorKind::IsADirectory),
                    };
                    return Ok(Err(msg));
                }
                Ok(Ok(f))
            },
        )?;
        match opened {
            Ok(Err(msg)) => Ok(self.sock_err(format!("{}: {msg}", path.display()))),
            Ok(Ok(f)) => {
                let core = Arc::new(ReaderCore {
                    inner: Mutex::new(Some(std::io::BufReader::new(f))),
                    key: core::next_poll_key(),
                    waits,
                    carry: Mutex::new(core::ReaderCarry::default()),
                });
                let v = Value::obj(self.heap.alloc(Obj::Reader(core)));
                Ok(self.sock_ok(v))
            }
            Err(e) => Ok(self.sock_err(format!("{}: {e}", path.display()))),
        }
    }
}

/// Can a syscall on the file at `path` wait without end: is it a FIFO, a socket or a device? A
/// regular file, a directory and a path that does not exist yet answer at once.
fn path_waits(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_file() && !m.is_dir())
}

/// Close an I/O handle's `inner` slot: take its backing out under the lock, then drop it (a file
/// closes) after the lock is released. M3.0a C7 — so a thread holds the
/// lock for long only for an op on an OPEN slot, which is what a render that cannot wait reads
/// (`io_state_now`, `vm/display.rs`): a drop's flush can block on a FIFO.
pub(super) fn close_slot<T>(inner: &std::sync::Mutex<Option<T>>) {
    let backing = inner.lock().unwrap_or_else(|e| e.into_inner()).take();
    drop(backing);
}

/// Is `read_line` served with no syscall: a closed reader, a carry, or a whole line in the buffer?
fn line_is_buffered(
    carry: &core::ReaderCarry,
    guard: &Option<std::io::BufReader<std::fs::File>>,
) -> bool {
    match guard {
        None => true,
        Some(br) => {
            carry.io_err.is_some() || !carry.bytes.is_empty() || br.buffer().contains(&b'\n')
        }
    }
}

/// Is `read_bytes(n)` served with no syscall: a closed reader, a carry, or `n` buffered bytes?
fn bytes_are_buffered(
    carry: &core::ReaderCarry,
    guard: &Option<std::io::BufReader<std::fs::File>>,
    n: u64,
) -> bool {
    match guard {
        None => true,
        Some(br) => !carry.bytes.is_empty() || br.buffer().len() as u64 >= n,
    }
}

/// `Reader.read_line` under the core's two locks. `Err(None)`: the reader is closed.
fn read_line_locked(
    carry: &mut core::ReaderCarry,
    guard: &mut Option<std::io::BufReader<std::fs::File>>,
) -> Result<Option<String>, Option<String>> {
    use std::io::BufRead;
    match guard.as_mut() {
        None => Err(None), // closed (checked BEFORE the carry is served)
        // W7-9: the carry left by a failed READ is the truncated head of a line, so
        // it re-faults until drained. Decoding it into a line would present a
        // fragment as a whole record — a silent lie in place of the silent loss.
        Some(_) if carry.io_err.is_some() => Err(Some(carry.io_err.clone().unwrap_or_default())),
        // A carry left by a failed DECODE is re-decoded IN PLACE — moving it out to
        // a `Vec` would memmove the whole (file-sized) buffer on every probe of the
        // documented drain-and-retry loop. `make_contiguous` pays that once.
        Some(_) if !carry.bytes.is_empty() => {
            match std::str::from_utf8(carry.bytes.make_contiguous()) {
                Ok(s) => {
                    let line = strip_eol(s).to_string();
                    carry.reset();
                    Ok(Some(line))
                }
                Err(_) => Err(Some("stream did not contain valid UTF-8".to_string())),
            }
        }
        Some(br) => {
            // W7-9: read RAW. `BufRead::read_line(&mut String)` consumes the line off
            // the reader and then errs with the bytes already dropped — the decode has
            // to happen where the bytes can still be retained.
            let mut buf: Vec<u8> = Vec::new();
            match br.read_until(b'\n', &mut buf) {
                Ok(0) => Ok(None), // EOF
                Ok(_) => match String::from_utf8(buf) {
                    Ok(line) => Ok(Some(strip_eol(&line).to_string())),
                    Err(e) => {
                        // Retain the raw line, terminator included: `read_bytes`
                        // hands it back byte-exactly.
                        carry.bytes = e.into_bytes().into();
                        Err(Some("stream did not contain valid UTF-8".to_string()))
                    }
                },
                Err(e) => {
                    // An IO error mid-line is NOT empty-handed: `read_until`
                    // documents that everything it read before the error is left in
                    // `buf`, and those bytes are already off the `BufReader`.
                    // Dropping them is the same silent loss W7-9 exists to kill, so
                    // they go in the carry too — flagged, so the truncated head is
                    // recoverable via `read_bytes` but never served as a line.
                    carry.bytes = buf.into();
                    carry.io_err = Some(e.to_string());
                    Err(Some(e.to_string()))
                }
            }
        }
    }
}

/// `Reader.read_bytes(n)` under the core's two locks. `Err(None)`: the reader is closed.
fn read_bytes_locked(
    carry: &mut core::ReaderCarry,
    guard: &mut Option<std::io::BufReader<std::fs::File>>,
    n: u64,
) -> Result<Vec<u8>, Option<String>> {
    use std::io::Read;
    match guard.as_mut() {
        None => Err(None), // closed (checked BEFORE the carry is served)
        // W7-9: a pending carry is drained FIRST and the fd is not touched — a
        // carry-only SHORT read ("at most n" licenses it), byte-identical to
        // `socket_read_bytes`. `n <= 0` still clamps to `Ok(b"")` here, exactly as it
        // does off the fd — that is the documented clamp, not an EOF claim.
        Some(_) if !carry.bytes.is_empty() => {
            let take = (n as usize).min(carry.bytes.len());
            let out = carry.bytes.drain(..take).collect::<Vec<u8>>();
            if carry.bytes.is_empty() {
                carry.reset(); // release the (file-sized) buffer, clear the IO flag
            }
            Ok(out)
        }
        Some(br) => {
            let mut buf = Vec::new();
            match br.take(n).read_to_end(&mut buf) {
                Ok(_) => Ok(buf),
                Err(e) => Err(Some(e.to_string())),
            }
        }
    }
}

/// A write of `data` to a writer core under its lock: what is left to do once it is released.
fn write_locked(guard: &mut Option<Backing>, data: &[u8]) -> Result<Wrote, WriteErr> {
    let Some(backing) = guard.as_mut() else {
        return Err(WriteErr::Closed);
    };
    Ok(match backing {
        Backing::File(file) => {
            use std::io::Write;
            file.write_all(data)
                .map_err(|e| WriteErr::Io(e.to_string()))?;
            Wrote::Done
        }
        Backing::Stdout => Wrote::Stdout,
        Backing::Stderr => Wrote::Stderr,
        Backing::Buffered { inner, buf, cap } => {
            buf.extend_from_slice(data);
            if buf.len() >= *cap {
                Wrote::Drain(Arc::clone(inner), std::mem::take(buf))
            } else {
                Wrote::Done
            }
        }
    })
}

/// Does a write stay off the fd: a closed core, or a console or in-VM sink? A file write always
/// reaches the fd.
fn writes_are_buffered(guard: &Option<Backing>) -> bool {
    match guard {
        Some(Backing::File(_)) => false,
        Some(Backing::Stdout | Backing::Stderr | Backing::Buffered { .. }) | None => true,
    }
}

/// What a flush leaves to drain into a `Buffered` core's inner core.
type Drained = Option<(Arc<WriterCore>, Vec<u8>)>;

/// A flush of a writer core under its lock: what is left to drain into the inner core.
fn flush_locked(guard: &mut Option<Backing>) -> Result<Drained, WriteErr> {
    let Some(backing) = guard.as_mut() else {
        return Err(WriteErr::Closed);
    };
    Ok(match backing {
        Backing::File(_) | Backing::Stdout | Backing::Stderr => None,
        // W6-1 — ALWAYS recurse, even with an empty in-VM buffer: an empty `buf` does NOT mean
        // the inner core is clean (a nested `Buffered` inner can hold a mid-write drain).
        Backing::Buffered { inner, buf, .. } => Some((Arc::clone(inner), std::mem::take(buf))),
    })
}
