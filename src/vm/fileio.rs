// vm::fileio — R2 `Writer` / file-handle methods + openers. `super::*` == the `vm` module.
// Mirrors vm::netio's Socket handle dispatch, but blocking-classified with NO netpoller/park
// (regular files are always epoll-ready, so file writes are synchronous blocking syscalls). The
// stdout/stderr backings route through `Vm::emit_out`/`emit_err` (the `Vm.out` parity oracle), never
// a raw fd.

use super::*;

/// R2 — the outcome of a low-level write to a [`WriterCore`].
enum WriteErr {
    /// The backing was `close()`d (its `Option` is `None`) — a clean use-after-close fault.
    Closed,
    /// A real I/O error (ENOSPC, a broken pipe, …), rendered for the `Result::Err` payload.
    Io(String),
}

impl Vm {
    /// R2 — clone out the shared `Arc<WriterCore>` behind a `Writer` handle (refcount bump), mirroring
    /// [`Vm::socket_core`]. Held only for the calling method, so locking the backing does not borrow
    /// the heap.
    pub(super) fn writer_core(&self, h: GcRef) -> Arc<WriterCore> {
        match self.heap.get(h) {
            Obj::Writer(core) => Arc::clone(core),
            _ => unreachable!("writer_core on non-writer"),
        }
    }

    /// R2 — write `data` to a writer core. `File` → `write_all` on the `BufWriter`; `Stdout`/`Stderr` →
    /// route through the [`Vm::emit_out`]/[`Vm::emit_err`] sink (the parity oracle — NEVER a raw fd),
    /// decoding via `from_utf8_lossy` (the sink is `&str`-typed; the byte-exact common path is
    /// `write(str)`); `Buffered` → append to the in-VM buffer and drain to the inner core once it
    /// reaches `cap`. Returns the byte count on success.
    fn write_to_core(&mut self, core: &WriterCore, data: &[u8]) -> Result<usize, WriteErr> {
        // Take the drain decision under the lock, then route to the inner core with the lock RELEASED
        // (the inner core has its own Mutex; a Stdout inner needs `&mut self`).
        let drain: Option<(Arc<WriterCore>, Vec<u8>)> = {
            let mut guard = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(backing) = guard.as_mut() else {
                return Err(WriteErr::Closed);
            };
            match backing {
                Backing::File(bw) => {
                    use std::io::Write;
                    bw.write_all(data)
                        .map_err(|e| WriteErr::Io(e.to_string()))?;
                    None
                }
                Backing::Stdout => {
                    let s = String::from_utf8_lossy(data).into_owned();
                    self.emit_out(&s);
                    None
                }
                Backing::Stderr => {
                    let s = String::from_utf8_lossy(data).into_owned();
                    self.emit_err(&s);
                    None
                }
                Backing::Buffered { inner, buf, cap } => {
                    buf.extend_from_slice(data);
                    if buf.len() >= *cap {
                        Some((Arc::clone(inner), std::mem::take(buf)))
                    } else {
                        None
                    }
                }
            }
        };
        if let Some((inner, drained)) = drain {
            self.write_to_core(&inner, &drained)?;
        }
        Ok(data.len())
    }

    /// R2 — flush a writer core. `File` → flush the `BufWriter` to the fd. `Stdout`/`Stderr` → honest
    /// no-op (the sink is unbuffered — `io.flush()` stays inert there). `Buffered` → drain the in-VM
    /// buffer to the inner core, THEN flush the inner (so `buffered(create(f))` is durable on disk).
    fn flush_core(&mut self, core: &WriterCore) -> Result<(), WriteErr> {
        let drain: Option<(Arc<WriterCore>, Vec<u8>)> = {
            let mut guard = core.inner.lock().unwrap_or_else(|e| e.into_inner());
            let Some(backing) = guard.as_mut() else {
                return Err(WriteErr::Closed);
            };
            match backing {
                Backing::File(bw) => {
                    use std::io::Write;
                    bw.flush().map_err(|e| WriteErr::Io(e.to_string()))?;
                    None
                }
                Backing::Stdout | Backing::Stderr => None,
                Backing::Buffered { inner, buf, .. } => {
                    if buf.is_empty() {
                        None
                    } else {
                        Some((Arc::clone(inner), std::mem::take(buf)))
                    }
                }
            }
        };
        if let Some((inner, drained)) = drain {
            self.write_to_core(&inner, &drained)?;
            self.flush_core(&inner)?;
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
        match method {
            "write" | "write_bytes" => {
                self.arity_err(method, args, 1, span)?;
                let data = match (method, args.first()) {
                    ("write_bytes", Some(v)) => self.collect_bytes_arg("write_bytes", *v, span)?,
                    (_, Some(Value::Obj(sh))) => match self.heap.get(*sh) {
                        Obj::Str(s) => s.as_bytes().to_vec(),
                        _ => return Err(self.err("write expects a str".into(), span)),
                    },
                    _ => return Err(self.err("write expects a str".into(), span)),
                };
                let core = self.writer_core(h);
                match self.write_to_core(&core, &data) {
                    Ok(n) => Ok(self.sock_ok(Value::Int(n as i64))),
                    Err(WriteErr::Closed) => Ok(self.sock_err("write on a closed writer")),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                }
            }
            "flush" => {
                self.arity_err("flush", args, 0, span)?;
                let core = self.writer_core(h);
                match self.flush_core(&core) {
                    Ok(()) => Ok(self.sock_ok(Value::Nil)),
                    Err(WriteErr::Closed) => Ok(self.sock_err("flush on a closed writer")),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.writer_core(h);
                // Flush the tail first, THEN take + drop the backing (closing the fd). A flush error on
                // close still surfaces, but the backing is dropped regardless (no leaked fd).
                let flushed = self.flush_core(&core);
                *core.inner.lock().unwrap_or_else(|e| e.into_inner()) = None;
                match flushed {
                    Ok(()) | Err(WriteErr::Closed) => Ok(self.sock_ok(Value::Nil)),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                }
            }
            _ => Err(self.err(format!("type Writer has no method '{method}'"), span)),
        }
    }

    /// R2 — dispatch the `std.io` openers/handles intercepted by func-pointer in `invoke_native`. Each
    /// allocates a heap `Writer` handle over an `Arc`'d core (a pure off-heap native cannot), mirroring
    /// `std.net`'s `connect`/`listen`.
    pub(super) fn io_native(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match name {
            "create" | "append" => self.io_open(name, &args, span),
            "stdout" => Ok(self.io_std_handle(Backing::Stdout)),
            "stderr" => Ok(self.io_std_handle(Backing::Stderr)),
            "buffered" => self.io_buffered(&args, span),
            _ => unreachable!("io_native on '{name}'"),
        }
    }

    /// R2 — `io.create(path)` = truncate + create; `io.append(path)` = append, create-if-absent (never
    /// truncates). Returns `Ok(Writer)` or a clean `Err` (perms, missing dir).
    fn io_open(&mut self, verb: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let path = match args.first() {
            Some(Value::Obj(h)) => match self.heap.get(*h) {
                Obj::Str(s) => s.to_string(),
                _ => return Err(self.err(format!("io.{verb} expects a path string"), span)),
            },
            _ => return Err(self.err(format!("io.{verb} expects a path string"), span)),
        };
        let opened = match verb {
            "create" => std::fs::File::create(&path),
            "append" => std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path),
            _ => unreachable!("io_open on '{verb}'"),
        };
        match opened {
            Ok(f) => {
                let core = Arc::new(WriterCore {
                    inner: Mutex::new(Some(Backing::File(std::io::BufWriter::new(f)))),
                    key: core::next_poll_key(),
                });
                let v = Value::Obj(self.heap.alloc(Obj::Writer(core)));
                Ok(self.sock_ok(v))
            }
            Err(e) => Ok(self.sock_err(format!("{path}: {e}"))),
        }
    }

    /// R2 — `io.stdout()` / `io.stderr()`: a FRESH stream `Writer` per call, routing through the
    /// existing `Vm::emit_out`/`emit_err` sink. NOT a `Result` (opening a stream can't fail).
    fn io_std_handle(&mut self, backing: Backing) -> Value {
        let core = Arc::new(WriterCore {
            inner: Mutex::new(Some(backing)),
            key: core::next_poll_key(),
        });
        Value::Obj(self.heap.alloc(Obj::Writer(core)))
    }

    /// R2 — `io.buffered(w, size = 8192)`: wrap a `Writer` in a `Backing::Buffered` that accumulates in
    /// the VM and drains to `w` on flush / buffer-full / close (the Go `bufio.NewWriter` escape hatch).
    fn io_buffered(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let inner = match args.first() {
            Some(Value::Obj(h)) if matches!(self.heap.get(*h), Obj::Writer(_)) => {
                self.writer_core(*h)
            }
            _ => return Err(self.err("buffered expects a Writer".into(), span)),
        };
        let cap = match args.get(1) {
            None => 8192,
            Some(Value::Int(n)) => (*n).max(1) as usize,
            Some(_) => return Err(self.err("buffered size expects an int".into(), span)),
        };
        let core = Arc::new(WriterCore {
            inner: Mutex::new(Some(Backing::Buffered {
                inner,
                buf: Vec::new(),
                cap,
            })),
            key: core::next_poll_key(),
        });
        Ok(Value::Obj(self.heap.alloc(Obj::Writer(core))))
    }
}
