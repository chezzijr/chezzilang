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
        io => io,
    }
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
    /// hand the RAW bytes to the [`Vm::emit_out_bytes`]/[`Vm::emit_err_bytes`] sink (the parity oracle
    /// — NEVER a raw fd), which is byte-typed end to end, so `write_bytes(b"\xff\xfe")` is byte-exact
    /// on the console exactly as it already was on a file (W6-9); `Buffered` → append to the in-VM
    /// buffer and drain to the inner core once it reaches `cap`. Returns the byte count on success —
    /// no backing can short-write (`write_all` / in-memory / an unbounded queue).
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
                    self.emit_out_bytes(data);
                    None
                }
                Backing::Stderr => {
                    self.emit_err_bytes(data);
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
            self.write_to_core(&inner, &drained).map_err(from_inner)?;
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
                // W6-1 — ALWAYS recurse, even with an empty in-VM buffer: an empty `buf` does NOT mean
                // the inner core is clean. A mid-write drain (`write_to_core`) already `write_all`'d
                // into the inner `BufWriter` WITHOUT flushing it and emptied `buf`, so the old
                // `if buf.is_empty() { None }` short-circuit made `flush`/`close` persist nothing.
                Backing::Buffered { inner, buf, .. } => {
                    Some((Arc::clone(inner), std::mem::take(buf)))
                }
            }
        };
        if let Some((inner, drained)) = drain {
            // Guard the WRITE, not the flush: an empty `write_to_core` on a `Stdout`/`Stderr` inner
            // would hand `emit_out("")` to the parity-oracle sink / stream queue.
            if !drained.is_empty() {
                self.write_to_core(&inner, &drained).map_err(from_inner)?;
            }
            self.flush_core(&inner).map_err(from_inner)?;
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
                let data = if method == "write_bytes"
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
                match self.write_to_core(&core, &data) {
                    Ok(n) => Ok(self.sock_ok(Value::int(n as i64))),
                    Err(WriteErr::Closed) => Ok(self.sock_err("write on a closed writer")),
                    Err(WriteErr::Io(e)) => Ok(self.sock_err(e)),
                }
            }
            "flush" => {
                self.arity_err("flush", args, 0, span)?;
                let core = self.writer_core(h);
                match self.flush_core(&core) {
                    Ok(()) => Ok(self.sock_ok(Value::nil())),
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
                    Ok(()) | Err(WriteErr::Closed) => Ok(self.sock_ok(Value::nil())),
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
            "open" => self.io_open_reader(&args, span),
            "stdout" => Ok(self.io_std_handle(Backing::Stdout)),
            "stderr" => Ok(self.io_std_handle(Backing::Stderr)),
            "buffered" => self.io_buffered(&args, span),
            _ => unreachable!("io_native on '{name}'"),
        }
    }

    /// R2 — `io.create(path)` = truncate + create; `io.append(path)` = append, create-if-absent (never
    /// truncates). Returns `Ok(Writer)` or a clean `Err` (perms, missing dir).
    fn io_open(&mut self, verb: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let path = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(sh)
        {
            s.to_string()
        } else {
            return Err(self.err(format!("io.{verb} expects a path string"), span));
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
                let v = Value::obj(self.heap.alloc(Obj::Writer(core)));
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
            _ => unreachable!("reader_core on non-reader"),
        }
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
    /// No netpoller park — file reads block synchronously (regular files are always epoll-ready).
    ///
    /// ponytail: an inline blocking read can pin an M:N worker on a slow fifo/pipe — the same accepted
    /// ceiling `Writer.write` carries; NOT offloaded to the dirty pool (that path is whole-file-only).
    /// For the intended regular-file use (always ready) the risk is nil; add offload if a fifo use lands.
    pub(super) fn reader_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "read_line" => {
                self.arity_err("read_line", args, 0, span)?;
                let core = self.reader_core(h);
                let outcome = {
                    use std::io::BufRead;
                    // LOCK ORDER: `carry` OUTER, `inner` INNER (see `ReaderCore::carry`).
                    let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                    let mut guard = core.inner.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.as_mut() {
                        None => Err(None), // closed (checked BEFORE the carry is served)
                        Some(br) => {
                            // W7-9: read RAW. `BufRead::read_line(&mut String)` consumes the line off
                            // the reader and then errs with the bytes already dropped — the decode has
                            // to happen where the bytes can still be retained. A pending carry is
                            // re-decoded first (sticky), so a refused line is never skipped.
                            let mut buf: Vec<u8> = std::mem::take(&mut *carry).into();
                            let taken = if buf.is_empty() {
                                br.read_until(b'\n', &mut buf)
                            } else {
                                Ok(buf.len())
                            };
                            match taken {
                                Ok(0) => Ok(None), // EOF
                                Ok(_) => match String::from_utf8(buf) {
                                    Ok(mut line) => {
                                        // Strip the line terminator the same way the module-level
                                        // io.read_line does (native/mod.rs): trailing '\n' then '\r'
                                        // UNCONDITIONALLY — a bare/classic-Mac '\r' must not survive,
                                        // or Reader.read_line drifts from its owning ancestor.
                                        let end = line
                                            .trim_end_matches('\n')
                                            .trim_end_matches('\r')
                                            .len();
                                        line.truncate(end);
                                        Ok(Some(line))
                                    }
                                    Err(e) => {
                                        // Retain the raw line, terminator included: `read_bytes`
                                        // hands it back byte-exactly.
                                        *carry = e.into_bytes().into();
                                        Err(Some("stream did not contain valid UTF-8".to_string()))
                                    }
                                },
                                Err(e) => {
                                    // An IO error mid-line is NOT empty-handed: `read_until`
                                    // documents that everything it read before the error is left in
                                    // `buf`, and those bytes are already off the `BufReader`.
                                    // Dropping them is the same silent loss W7-9 exists to kill, so
                                    // they go in the carry too. (A later read serves them: it is a
                                    // partial line, but the fd errored — there is no more of it.)
                                    *carry = buf.into();
                                    Err(Some(e.to_string()))
                                }
                            }
                        }
                    }
                };
                match outcome {
                    Ok(Some(line)) => {
                        let sv = self.alloc_str(line);
                        Ok(self.alloc_enum("Option", "Some", vec![sv]))
                    }
                    Ok(None) => Ok(self.alloc_enum("Option", "None", vec![])),
                    Err(None) => Err(self.err("read_line on a closed reader".into(), span)),
                    Err(Some(e)) => Err(self.err(
                        format!("{e} — read binary files with Reader.read_bytes"),
                        span,
                    )),
                }
            }
            "read_bytes" => {
                self.arity_err("read_bytes", args, 1, span)?;
                let n = match args.first() {
                    Some(v) if self.is_integral(*v) => self.int_of(*v).max(0) as u64,
                    _ => return Err(self.err("read_bytes expects an int byte count".into(), span)),
                };
                let core = self.reader_core(h);
                let outcome = {
                    use std::io::Read;
                    // LOCK ORDER: `carry` OUTER, `inner` INNER (see `ReaderCore::carry`).
                    let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                    let mut guard = core.inner.lock().unwrap_or_else(|e| e.into_inner());
                    match guard.as_mut() {
                        None => Err(None), // closed (checked BEFORE the carry is served)
                        // W7-9: a pending carry is drained FIRST and the fd is not touched — a
                        // carry-only SHORT read ("at most n" licenses it), byte-identical to
                        // `socket_read_bytes`. Empty-means-EOF survives: we never return empty
                        // while the carry is non-empty.
                        Some(_) if !carry.is_empty() => {
                            let take = (n as usize).min(carry.len());
                            Ok(carry.drain(..take).collect::<Vec<u8>>())
                        }
                        Some(br) => {
                            let mut buf = Vec::new();
                            match br.take(n).read_to_end(&mut buf) {
                                Ok(_) => Ok(buf),
                                Err(e) => Err(Some(e.to_string())),
                            }
                        }
                    }
                };
                match outcome {
                    Ok(buf) => {
                        let bv = Value::obj(self.heap.alloc(Obj::Bytes(buf.into_boxed_slice())));
                        Ok(self.sock_ok(bv))
                    }
                    Err(None) => Ok(self.sock_err("read_bytes on a closed reader")),
                    Err(Some(e)) => Ok(self.sock_err(e)),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.reader_core(h);
                // Take + drop the reader (closing the fd). Idempotent: an already-closed reader is Ok.
                // LOCK ORDER: `carry` OUTER, `inner` INNER — the one ordering `ReaderCore::carry`
                // forbids is inner-then-carry, so close takes the carry first. Closed is closed: the
                // carry is discarded with the fd (W7-9).
                let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                *core.inner.lock().unwrap_or_else(|e| e.into_inner()) = None;
                carry.clear();
                drop(carry);
                Ok(self.sock_ok(Value::nil()))
            }
            _ => Err(self.err(format!("type Reader has no method '{method}'"), span)),
        }
    }

    /// R2b — `io.open(path)` = open a file read-only. Returns `Ok(Reader)` or a clean `Err` (missing
    /// file, perms). The read twin of `io.create`/`io.append`.
    fn io_open_reader(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let path = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(sh)
        {
            s.to_string()
        } else {
            return Err(self.err("io.open expects a path string".into(), span));
        };
        match std::fs::File::open(&path) {
            Ok(f) => {
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
                    return Ok(self.sock_err(format!("{path}: {msg}")));
                }
                let core = Arc::new(ReaderCore {
                    inner: Mutex::new(Some(std::io::BufReader::new(f))),
                    key: core::next_poll_key(),
                    carry: Mutex::new(std::collections::VecDeque::new()),
                });
                let v = Value::obj(self.heap.alloc(Obj::Reader(core)));
                Ok(self.sock_ok(v))
            }
            Err(e) => Ok(self.sock_err(format!("{path}: {e}"))),
        }
    }
}
