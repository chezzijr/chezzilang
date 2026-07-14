// vm::netio — split out of vm/mod.rs. `super::*` == the `vm` module.
// Channels, Shared/RwShared/Atomic, sockets/listeners, netpoller parks.

use super::*;

/// B1 — the outcome of decoding one socket chunk (+ the socket's carried tail) as UTF-8.
pub(super) enum Decoded {
    /// A complete, valid `str` (possibly empty — the EOF sentinel).
    Text(String),
    /// A recoverable `Err` message (genuinely-invalid bytes, or a partial codepoint left at EOF).
    Fail(String),
    /// The bytes so far are a strict prefix of ONE codepoint (≤3 bytes, now carried): no complete
    /// codepoint to hand back yet — take more bytes off the fd.
    NeedMore,
}

impl Vm {
    /// B3.1 — clone out the shared `Arc<ChannelCore>` behind a `Channel` handle (refcount bump). The
    /// `Arc` is held only for the duration of the calling method, so locking it does not borrow the
    /// heap, leaving `self` free for the re-entrant value paths (`from_wire`, `invoke_value`).
    pub(super) fn channel_core(&self, h: GcRef) -> Arc<ChannelCore> {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::clone(core),
            _ => unreachable!("channel_core on non-channel"),
        }
    }

    pub(super) fn shared_core(&self, h: GcRef) -> Arc<SharedCore> {
        match self.heap.get(h) {
            Obj::Shared(core) => Arc::clone(core),
            _ => unreachable!("shared_core on non-shared"),
        }
    }

    pub(super) fn rwshared_core(&self, h: GcRef) -> Arc<RwSharedCore> {
        match self.heap.get(h) {
            Obj::RwShared(core) => Arc::clone(core),
            _ => unreachable!("rwshared_core on non-rwshared"),
        }
    }

    pub(super) fn atomic_core(&self, h: GcRef) -> Arc<AtomicCore> {
        match self.heap.get(h) {
            Obj::Atomic(core) => Arc::clone(core),
            _ => unreachable!("atomic_core on non-atomic"),
        }
    }

    /// `Atomic(v)` — pop the init, box its wire form behind a fresh `Arc<AtomicCore>`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        // A non-sendable init (a frame-holding generator) faults gracefully with the `NewAtomic` span.
        let init = self.to_wire_at(init, span)?;
        Ok(Value::Obj(self.heap.alloc(Obj::Atomic(Arc::new(
            AtomicCore {
                v: Mutex::new(init),
            },
        )))))
    }

    /// `timer(ms)` — pop the `ms` int, push a fresh `Channel[bool]` stamped with `now + ms`. Delivery is
    /// handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at the top level
    /// can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the `Instant`/`Duration` math
    /// stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_timer(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let ms = match self.pop() {
            Value::Int(ms) => ms.max(0) as u64,
            other => {
                return Err(self.err(
                    format!("timer(ms) expects int, got {}", self.type_name(other)),
                    span,
                ));
            }
        };
        // Saturate a pathological `ms` to a far-future deadline rather than panic on `Instant` overflow
        // (mirrors the `sleep_ms` offload path).
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(ms))
            .unwrap_or_else(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
            });
        let core = Arc::new(ChannelCore {
            timer: Some(deadline),
            ..Default::default()
        });
        Ok(Value::Obj(self.heap.alloc(Obj::Channel(core))))
    }

    pub(super) fn executor_core(&self, h: GcRef) -> Arc<ExecutorCore> {
        match self.heap.get(h) {
            Obj::Executor(core) => Arc::clone(core),
            _ => unreachable!("executor_core on non-executor"),
        }
    }

    /// D6 — clone out the shared `Arc<SocketCore>`/`Arc<ListenerCore>` behind a handle (refcount bump),
    /// mirroring [`channel_core`](Vm::channel_core). The `Arc` is held only for the calling method, so
    /// locking the fd does not borrow the heap.
    pub(super) fn socket_core(&self, h: GcRef) -> Arc<SocketCore> {
        match self.heap.get(h) {
            Obj::Socket(core) => Arc::clone(core),
            _ => unreachable!("socket_core on non-socket"),
        }
    }

    pub(super) fn listener_core(&self, h: GcRef) -> Arc<ListenerCore> {
        match self.heap.get(h) {
            Obj::Listener(core) => Arc::clone(core),
            _ => unreachable!("listener_core on non-listener"),
        }
    }

    /// D6 — build a `Result::Ok(v)` / `Result::Err(msg)` for a socket op (mirrors `lower_native`'s
    /// `Ok`/`Err` arms — the surface contract is `read/write/accept -> Result`).
    pub(super) fn sock_ok(&mut self, v: Value) -> Value {
        self.alloc_enum("Result", "Ok", vec![v])
    }
    pub(super) fn sock_err(&mut self, msg: impl Into<String>) -> Value {
        let ev = self.alloc_str(msg.into());
        self.alloc_enum("Result", "Err", vec![ev])
    }

    /// B1 — decode ONE `read` chunk off a socket into the `Result[str]` the surface promises. The ONLY
    /// decode point on the socket path (both the fast path and the in-callback demote poller route here
    /// — a second copy is how the lossy decode got duplicated in the first place).
    ///
    /// `String::from_utf8_lossy` used to sit at both sites: it silently turned every non-UTF-8 byte into
    /// U+FFFD (binary payloads corrupted, no error) AND mangled perfectly valid text whose codepoint
    /// straddled the `read(n)` boundary (the ordinary read-in-a-loop idiom). The two cases are distinct
    /// and `Utf8Error` tells them apart:
    /// * `error_len() == None` — TRUNCATED tail: the bytes so far are a valid prefix of a codepoint. Keep
    ///   the (≤3-byte) tail in [`SocketCore::carry`] and prepend it to the next read. If the chunk holds
    ///   no complete codepoint at all, return `None` = "need more bytes" — the caller re-reads (it must
    ///   NOT return `Ok("")`, which every `while chunk != "":` loop reads as EOF).
    /// * `error_len() == Some(_)` — genuinely INVALID bytes: a recoverable `Err` naming the real limit
    ///   (the str seam decodes; binary payloads are read with `read_bytes`), never a silent U+FFFD.
    ///
    /// NON-DESTRUCTIVE either way: the valid text that decoded BEFORE the problem byte is always handed
    /// back, and everything from the problem byte on stays in [`SocketCore::carry`]. So an invalid-utf-8
    /// `Err` is STICKY (the same bytes re-decode and re-err on the next read) rather than eating the
    /// chunk — a recoverable `Err` that silently drops up to `MAX_SOCKET_READ` of already-received
    /// payload would just be a different flavour of the corruption B1 fixes.
    ///
    /// `eof` (the fd returned 0 bytes for a `read(n>0)`) keeps today's `Ok("")` EOF sentinel — unless a
    /// carry is still owed, i.e. the peer closed mid-codepoint: that is a real error, not a silent drop.
    ///
    /// PURE (no `&mut self`, no locking): the caller holds the `carry` guard ACROSS its fd read and
    /// passes `&mut *guard` here, so read-off-the-fd and carry-update are ONE critical section (two
    /// fibers aliasing one socket must decode in wire order — see [`SocketCore::carry`]).
    pub(super) fn decode_carry(carry: &mut Vec<u8>, chunk: &[u8], eof: bool) -> Decoded {
        if eof {
            let owed = carry.len();
            carry.clear();
            return if owed == 0 {
                Decoded::Text(String::new()) // the EOF sentinel, unchanged
            } else {
                Decoded::Fail(format!(
                    "invalid utf-8 at eof: the peer closed mid-codepoint ({owed} trailing byte(s))"
                ))
            };
        }
        // The hot path (no carry, a fully-valid chunk) decodes the fd's buffer BORROWED — one alloc
        // for the resulting `String`, exactly what `from_utf8_lossy(..).into_owned()` cost. Only a
        // pending carry has to splice, and a carry is ≤3 bytes on every non-error path.
        let bytes: std::borrow::Cow<'_, [u8]> = if carry.is_empty() {
            std::borrow::Cow::Borrowed(chunk)
        } else {
            let mut b = std::mem::take(carry);
            b.extend_from_slice(chunk);
            std::borrow::Cow::Owned(b)
        };
        let err = match std::str::from_utf8(&bytes) {
            Ok(s) => return Decoded::Text(s.to_string()),
            Err(e) => e,
        };
        // Whatever decoded BEFORE the problem byte is real text the peer sent: hand it back. Never
        // drop it — a `read` that consumed bytes off the fd and returned neither them nor an error
        // about them is silent data loss (the exact family B1 exists to kill).
        let good = err.valid_up_to();
        let prefix = std::str::from_utf8(&bytes[..good]).expect("valid_up_to prefix is utf-8");
        let prefix = Decoded::Text(prefix.to_string());
        // Everything from the problem byte on stays CARRIED — nothing is consumed-and-dropped.
        //   * truncated tail (`error_len() == None`): ≤3 bytes, the next read prepends them and the
        //     codepoint completes.
        //   * genuinely INVALID bytes (`error_len() == Some(_)`): a str-only seam can never hand them
        //     back, so the carry makes the `Err` STICKY — every later read re-decodes the same bytes
        //     and re-errs identically. A caller that logs the Err and keeps reading (what a `Result`
        //     invites) therefore cannot silently shred the stream; it must `close()`.
        *carry = bytes[good..].to_vec();
        match (err.error_len(), good) {
            // Truncated: nothing complete yet ⇒ the caller must take more bytes off the fd.
            (None, 0) => Decoded::NeedMore,
            (None, _) => prefix,
            // Invalid, but valid text came first: deliver that text now; the bad bytes re-err next read.
            (Some(_), 1..) => prefix,
            (Some(_), _) => Decoded::Fail(
                "invalid utf-8 on the socket: std.net read is str-only — read binary payloads with \
                 Socket.read_bytes. The bytes stay carried (read_bytes hands them back byte-exactly), \
                 so every str read on this socket now returns this error"
                    .to_string(),
            ),
        }
    }

    /// B1 — materialize a [`Decoded`] into the `Result[str]` value (`None` = NeedMore: the caller must
    /// take more bytes off the fd). Split from [`Vm::decode_carry`] so the allocating half runs with no
    /// socket lock held.
    pub(super) fn decoded_value(&mut self, d: Decoded) -> Option<Value> {
        match d {
            Decoded::NeedMore => None,
            Decoded::Text(s) => {
                let sv = self.alloc_str(s);
                Some(self.sock_ok(sv))
            }
            Decoded::Fail(m) => Some(self.sock_err(m)),
        }
    }

    /// D6 — `std.net.connect(addr)` / `listen(addr)`: allocate a non-blocking `Socket`/`Listener`
    /// handle (or a `Result::Err` on a bad address / bind failure). Intercepted in `invoke_native`
    /// because it allocates a heap handle over an `Arc`'d core — a pure off-heap native can't.
    ///
    /// D6b — `connect` is now a **true non-blocking** connect: an in-progress handshake (`EINPROGRESS`)
    /// parks the fiber on the socket's writability rather than pinning a worker for the round trip. The
    /// connecting socket is stashed in `pending_connect`; the netpoller wakes the fiber on writability
    /// and [`Vm::run_one_fiber`] completes it via [`Vm::finish_pending_connect`] (read `SO_ERROR`) and
    /// pushes the resulting `Socket` — the bytecode call site never re-runs. The instant (loopback)
    /// case still returns immediately.
    pub(super) fn net_connect_or_listen(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let addr = match args.first() {
            Some(Value::Obj(h)) => match self.heap.get(*h) {
                Obj::Str(s) => s.to_string(),
                _ => {
                    return Err(self.err(format!("std.net.{name} expects an address string"), span));
                }
            },
            _ => return Err(self.err(format!("std.net.{name} expects an address string"), span)),
        };
        match name {
            "connect" => match crate::native::net::connect_nonblocking(&addr) {
                // Connected synchronously (the common loopback case) — wrap + return at once.
                Ok((stream, false)) => {
                    Ok(self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight()))
                }
                // Handshake in flight: park the fiber on writability under the M:N engine; off it (the
                // cooperative / top-level v1 fallback, where there is no fiber to park), block until the
                // handshake settles. net targets `--parallel`.
                Ok((stream, true)) => {
                    if self.mn.is_some() && self.native_reentry == 0 {
                        self.park_on_connect(stream);
                        Ok(Value::Nil) // parked sentinel; `poll_park` gates the result-push at `do_call`
                    } else if self.mn.is_some() {
                        // `native_reentry > 0` — a `connect` reached inside a native callback (operator
                        // overload, list HOF, `Shared.update`, ...). The caller's loop state lives on the
                        // Rust stack, so the fiber can't park; and blocking here would pin a worker
                        // thread on the handshake. Fail loud, exactly as `read`/`write`/`accept` do.
                        Ok(self.sock_err(
                            "connect would block: std.net sockets require the --parallel engine",
                        ))
                    } else {
                        // Top-level / cooperative: no fiber to park, so block (bounded) until the
                        // handshake settles. net targets `--parallel`; this keeps a top-level
                        // `net.connect` usable as the v1 fallback.
                        Ok(self.block_until_connected(stream))
                    }
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            "listen" => match crate::native::net::listen_nonblocking(&addr) {
                Ok(listener) => {
                    let core = Arc::new(ListenerCore {
                        listener: Mutex::new(Some(listener)),
                        key: core::next_poll_key(),
                        in_flight: core::new_in_flight(),
                    });
                    let v = Value::Obj(self.heap.alloc(Obj::Listener(core)));
                    Ok(self.sock_ok(v))
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            _ => unreachable!("net_connect_or_listen on '{name}'"),
        }
    }

    /// D6b — wrap a connected `TcpStream` in a `Socket` handle and return `Ok(Socket)`. `key`/`in_flight`
    /// become the socket's poll identity for later `read`/`write` parks (a fresh pair for a synchronous
    /// connect; the connect's own pair, reused, for one that parked — its `in_flight` was cleared on
    /// inject).
    pub(super) fn alloc_socket_ok(
        &mut self,
        stream: std::net::TcpStream,
        key: usize,
        in_flight: Arc<AtomicBool>,
    ) -> Value {
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key,
            in_flight,
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::Obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// D6b — finish a connect that parked on writability: `SO_ERROR` clear ⇒ `Ok(Socket)`, else
    /// `Err(msg)`. Reuses the connect's poll key + guard so the resulting socket keeps a stable identity.
    pub(super) fn finish_pending_connect(&mut self, cip: ConnectInProgress) -> Value {
        match crate::native::net::finish_connect(&cip.stream) {
            Ok(()) => self.alloc_socket_ok(cip.stream, cip.key, cip.in_flight),
            Err(e) => self.sock_err(format!("connect failed: {e}")),
        }
    }

    /// D6b — park the current fiber on a connecting socket's writability. Stash the connecting stream
    /// in `pending_connect` (it owns the fd the poller will watch, so it must outlive the park) and set
    /// the `poll_park` sentinel; the worker loop hands both to the netpoller. Unlike a `read`/`write`
    /// park there is NO `ip` rewind — `net.connect`'s call site already popped its args and pushed
    /// nothing (`do_call` saw `paused()`), so on resume [`Vm::run_one_fiber`] finishes the connect and
    /// pushes the `Socket` exactly where the call would have, and execution continues past the call.
    pub(super) fn park_on_connect(&mut self, stream: std::net::TcpStream) {
        let key = core::next_poll_key();
        let in_flight = core::new_in_flight();
        in_flight.store(true, Ordering::Release); // mark parked (matches `park_on_fd`'s swap(true))
        let fd = stream.as_raw_fd();
        self.pending_connect = Some(ConnectInProgress {
            stream,
            key,
            in_flight: Arc::clone(&in_flight),
        });
        // A `connect` never carries a user timeout (the `connect` surface takes only an address); it
        // parks forever (or until `drain_sched` re-injects it on a sibling fault).
        self.poll_park = Some(PollPark {
            key,
            fd,
            interest: poller::Interest::Write,
            in_flight,
            deadline: None,
        });
    }

    /// D6b — the top-level connect fallback (no fiber to park): block until the handshake settles, then
    /// return `Ok(Socket)` / `Err`. Bounded by a wall-clock deadline so a black-hole address (no RST,
    /// no SYN-ACK — `SO_ERROR` never sets, the fd never becomes writable) returns a clean timeout
    /// instead of spinning for the kernel's multi-minute connect timeout. net targets the M:N
    /// `--parallel` engine, so this path exists only to keep a top-level `net.connect` usable.
    pub(super) fn block_until_connected(&mut self, stream: std::net::TcpStream) -> Value {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(CONNECT_BLOCK_TIMEOUT_SECS);
        loop {
            match crate::native::net::finish_connect(&stream) {
                // SO_ERROR clear AND the peer is reachable ⇒ connected.
                Ok(()) if stream.peer_addr().is_ok() => {
                    return self.alloc_socket_ok(
                        stream,
                        core::next_poll_key(),
                        core::new_in_flight(),
                    );
                }
                Err(e) => return self.sock_err(format!("connect failed: {e}")),
                Ok(()) if std::time::Instant::now() >= deadline => {
                    return self.sock_err("connect failed: timed out");
                }
                Ok(()) => std::thread::sleep(std::time::Duration::from_millis(1)), // not settled yet
            }
        }
    }

    /// R1/B1 — `Socket.read_bytes(n) -> Result[bytes]` / `read_bytes(n, timeout_ms)`: the BINARY read.
    /// No decode at all, so no carry/`NeedMore` loop — the contract is the natural byte one: it returns
    /// AT MOST `n` bytes (unlike the str `read`, whose `n` bounds only the NEW fd bytes and which may
    /// hand back up to `n + 3` when it prepends a carried codepoint tail).
    ///
    /// It DRAINS the carry first: bytes a previous str `read` left behind — including the undecodable
    /// bytes its sticky `Err("invalid utf-8 …")` refused to deliver — are handed back here byte-exactly
    /// (that is the escape hatch the str Err points at; ignoring the carry would make a mixed
    /// `read`/`read_bytes` socket silently lossy). Only when the carry is empty does it touch the fd.
    /// `got == 0` on a `read_bytes(n>0)` is EOF → `Ok(b"")`. `read_bytes(0)` is a no-op `Ok(b"")` that
    /// still errs on a CLOSED socket (same shape as `read(0)`). Would-block: park on the netpoller /
    /// demote in-callback / fail loud off the M:N engine, exactly like `read`, with the same LATCHED
    /// `poll_deadline` (a re-park must not re-arm the timeout budget).
    fn socket_read_bytes(
        &mut self,
        h: GcRef,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_range_err("read_bytes", args, 1, 2, span)?;
        if self.poll_timed_out {
            self.poll_timed_out = false;
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        let n = match args.first() {
            Some(Value::Int(n)) => ((*n).max(0) as usize).min(MAX_SOCKET_READ),
            _ => return Err(self.err("read_bytes expects an int byte count".into(), span)),
        };
        let core = self.socket_core(h);
        if n == 0 {
            if core
                .stream
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(self.sock_err("read_bytes on a closed socket"));
            }
            let bv = Value::Obj(self.heap.alloc(Obj::Bytes(Box::default())));
            return Ok(self.sock_ok(bv));
        }
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        let mut buf = vec![0u8; n];
        let attempt = {
            // LOCK ORDER: `carry` OUTER, `stream` INNER (see `SocketCore::carry`).
            let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
            let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
            let Some(stream) = guard.as_mut() else {
                return Ok(self.sock_err("read_bytes on a closed socket"));
            };
            if !carry.is_empty() {
                let take = n.min(carry.len());
                Ok(carry.drain(..take).collect::<Vec<u8>>())
            } else {
                match std::io::Read::read(stream, &mut buf) {
                    Ok(got) => Ok(buf[..got].to_vec()),
                    Err(e) => Err((e, stream.as_raw_fd())),
                }
            }
        };
        // Allocate only AFTER the locks drop.
        match attempt {
            Ok(bytes) => {
                let bv = Value::Obj(self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())));
                Ok(self.sock_ok(bv))
            }
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Read,
                    in_flight: Arc::clone(&core.in_flight),
                    deadline,
                };
                if self.park_on_fd(h, args, target, span)? {
                    return Ok(Value::Nil); // parked (sentinel)
                }
                if self.mn.is_some() && self.native_reentry > 0 {
                    let core = Arc::clone(&core);
                    return self.demote_block_socket(
                        fd,
                        poller::Interest::Read,
                        deadline,
                        span,
                        move |vm| {
                            let mut b = vec![0u8; n];
                            let r = {
                                let mut carry =
                                    core.carry.lock().unwrap_or_else(|e| e.into_inner());
                                let mut guard =
                                    core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                let Some(stream) = guard.as_mut() else {
                                    return SockPoll::Ready(Ok(
                                        vm.sock_err("read_bytes on a closed socket")
                                    ));
                                };
                                if !carry.is_empty() {
                                    let take = n.min(carry.len());
                                    Ok(carry.drain(..take).collect::<Vec<u8>>())
                                } else {
                                    match std::io::Read::read(stream, &mut b) {
                                        Ok(got) => Ok(b[..got].to_vec()),
                                        Err(e) => Err(e),
                                    }
                                }
                            };
                            match r {
                                Ok(bytes) => {
                                    let bv = Value::Obj(
                                        vm.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())),
                                    );
                                    SockPoll::Ready(Ok(vm.sock_ok(bv)))
                                }
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    SockPoll::WouldBlock
                                }
                                Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                            }
                        },
                    );
                }
                Ok(self.sock_err(
                    "read_bytes would block: std.net sockets require the --parallel engine",
                ))
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
        }
    }

    /// D6/B1 — `Socket.read(n) -> Result[str]` / `read(n, timeout_ms)`. On a would-block, under the M:N
    /// engine the fiber PARKS on the netpoller (re-root the receiver, rewind `ip` so the op re-executes
    /// on resume, set the `poll_park` sentinel — mirrors the channel `recv` park, but routed to the
    /// poller). Off the M:N engine (top level / cooperative) there is no fiber to park, so the op fails
    /// loud (a documented v1 fallback — net targets `--parallel`).
    ///
    /// Decodes through [`Vm::decode_carry`] (never `from_utf8_lossy`). Contract: `n` bounds the NEW
    /// bytes taken off the fd; a ≤3-byte incomplete-codepoint tail carried from the previous read is
    /// prepended, so a `read(n)` can return up to `n + 3` bytes and never fewer than the peer sent.
    /// `read(0)` is a no-op `Ok("")` — it never touches the fd and never turns a pending carry into a
    /// false EOF, but it still reports a CLOSED socket (`Ok("")` there would be indistinguishable from
    /// the EOF sentinel).
    ///
    /// A `read` whose bytes end mid-codepoint may BLOCK past its first successful fd read: it needs the
    /// rest of that codepoint, because a str-only seam cannot hand back half a character. (This is the
    /// same contract as Go's `bufio.Reader.ReadRune` and Python's text-mode socket file: a text reader
    /// blocks for a whole rune. The peer OWES those 1–3 bytes; the escapes are `timeout_ms` and the
    /// peer's close, which errors rather than dropping the tail.) `timeout_ms` bounds the WHOLE call on
    /// EVERY path — the netpoller park (the deadline is latched on the fiber, [`Vm::poll_deadline`], so
    /// re-parking to finish a codepoint does NOT restart the budget) and the in-callback demote loop
    /// ([`Vm::demote_block_socket`]) alike. The carry is RETAINED across a timeout `Err` (those bytes
    /// are still owed to the next read), and a poll-once (`timeout_ms == 0`) read that took a partial
    /// codepoint says so — `Err("incomplete utf-8: …")`, not the `Err("timeout")` that means "nothing
    /// arrived".
    fn socket_read(&mut self, h: GcRef, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // The optional trailing int bounds readiness (D6c). On a timeout the netpoller re-injects this
        // fiber with `poll_timed_out` set; the rewound op re-runs and lands HERE — so check it at entry
        // (after the `run_until` loop-top cancel check, which a sibling fault wins) and return Err.
        self.arity_range_err("read", args, 1, 2, span)?;
        if self.poll_timed_out {
            self.poll_timed_out = false;
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        // Cap the per-call buffer: a huge `read(n)` (caller-controlled) must not eagerly allocate
        // gigabytes before a byte arrives (review). The caller already loops for large payloads —
        // `read` returns the actual count.
        let n = match args.first() {
            Some(Value::Int(n)) => ((*n).max(0) as usize).min(MAX_SOCKET_READ),
            _ => return Err(self.err("read expects an int byte count".into(), span)),
        };
        let core = self.socket_core(h);
        // `read(0)` (or a negative / caller-computed-to-zero `n`): nothing to take off the fd. Return
        // the empty string WITHOUT reading — a zero-length `Read::read` returns `Ok(0)` unconditionally,
        // so feeding it to the decode/re-read loop below would spin forever whenever a carry is pending
        // (it can neither make progress nor would-block). It must STILL report a closed socket, though:
        // the stream lock's `None` arm is the only closed-fd detector on this path, and answering `Ok("")`
        // for a closed socket is indistinguishable from the EOF sentinel (review #1).
        if n == 0 {
            if core
                .stream
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(self.sock_err("read on a closed socket"));
            }
            let sv = self.alloc_str(String::new());
            return Ok(self.sock_ok(sv));
        }
        // The per-call deadline, latched on the fiber so it survives a park's ip-rewind re-execution.
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        // B1 — the loop only re-reads when the fd's bytes ended mid-codepoint AND held no complete
        // codepoint at all (`Decoded::NeedMore`): take more bytes for the rest of it. It is bounded —
        // a NeedMore carry is a strict prefix of ONE codepoint (≤3 bytes), so at most 3 data-bearing
        // re-reads can NeedMore before the codepoint completes (or the bytes are invalid ⇒ `Fail`).
        // The ordinary outcome of the re-read is WouldBlock, which falls into the park/demote branch
        // below (every arm of which returns).
        let mut buf = vec![0u8; n];
        // Did THIS call take bytes off the fd that completed no codepoint? Then a would-block is not a
        // "no data arrived" timeout, and saying so would be a lie (review #3/#8).
        let mut took_partial = false;
        loop {
            let attempt = {
                // LOCK ORDER: `carry` OUTER, `stream` INNER — the fd read and the carry update are ONE
                // critical section, or two fibers aliasing this socket decode out of wire order (a
                // valid multibyte stream would then err as "invalid utf-8"). See `SocketCore::carry`.
                let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                let Some(stream) = guard.as_mut() else {
                    return Ok(self.sock_err("read on a closed socket"));
                };
                // A carry that ALREADY decides must not wait on the fd first. After an invalid-utf-8
                // `Err` the offending bytes STAY carried (they are undeliverable through a str seam),
                // so a peer that sends nothing more would otherwise park us forever on bytes we already
                // hold. Re-decoding the carry against an EMPTY chunk settles it: `Fail` (sticky Err) or
                // `NeedMore` (a genuine truncated tail ⇒ go take its remaining bytes).
                let decided = match carry.is_empty() {
                    true => None,
                    false => match Self::decode_carry(&mut carry, &[], false) {
                        Decoded::NeedMore => None,
                        d => Some(d),
                    },
                };
                match decided {
                    Some(d) => Ok(d),
                    None => match std::io::Read::read(stream, &mut buf) {
                        // `got == 0` on a `read(n>0)` is EOF.
                        Ok(got) => Ok(Self::decode_carry(&mut carry, &buf[..got], got == 0)),
                        Err(e) => Err((e, stream.as_raw_fd())),
                    },
                }
            };
            match attempt {
                Ok(d) => match self.decoded_value(d) {
                    Some(v) => return Ok(v),
                    None => {
                        // Incomplete codepoint carried; take the rest of it off the fd.
                        took_partial = true;
                        continue;
                    }
                },
                Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // `timeout_ms == 0` (poll-once): do NOT park — answer immediately. If this poll took
                    // a partial codepoint, say THAT: `Err("timeout")` is documented as "no data within
                    // timeout_ms", and reporting a deadline expiry for a read that removed 1-3 bytes from
                    // the wire is the same lie-about-your-data class B1 exists to kill. The bytes are
                    // retained on the socket, so a retry finishes the codepoint byte-exactly.
                    if timeout.is_some_and(|t| t.poll_once) {
                        if took_partial {
                            let owed = core.carry.lock().unwrap_or_else(|e| e.into_inner()).len();
                            return Ok(self.sock_err(format!(
                                "incomplete utf-8: the poll landed mid-codepoint ({owed} byte(s) \
                                 carried and retained) — read this socket again to finish it"
                            )));
                        }
                        return Ok(self.sock_err("timeout"));
                    }
                    let target = PollPark {
                        key: core.key,
                        fd,
                        interest: poller::Interest::Read,
                        in_flight: Arc::clone(&core.in_flight),
                        deadline,
                    };
                    if self.park_on_fd(h, args, target, span)? {
                        return Ok(Value::Nil); // parked (sentinel; `poll_park` gates the push)
                    }
                    // No netpoller-park: inside a native callback on M:N (`native_reentry > 0`, the
                    // Rust-stack `map`/sort loop can't snapshot-park) → DEMOTE + backoff-poll the
                    // non-blocking read in place (#3 socket half). Off the M:N engine (top-level /
                    // cooperative) there is no fiber to demote → fail loud (a silent hang would also
                    // defeat the cooperative deadlock detector). net targets the `--parallel` engine.
                    if self.mn.is_some() && self.native_reentry > 0 {
                        let core = Arc::clone(&core);
                        return self.demote_block_socket(
                            fd,
                            poller::Interest::Read,
                            deadline,
                            span,
                            move |vm| {
                                let mut b = vec![0u8; n];
                                let r = {
                                    let mut carry =
                                        core.carry.lock().unwrap_or_else(|e| e.into_inner());
                                    let mut guard =
                                        core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                    let Some(stream) = guard.as_mut() else {
                                        return SockPoll::Ready(Ok(
                                            vm.sock_err("read on a closed socket")
                                        ));
                                    };
                                    // Settle a carry that already decides BEFORE touching the fd —
                                    // same guard, same order as the fast path (an invalid carry is
                                    // sticky, so waiting on the fd for it would poll to the deadline
                                    // for nothing).
                                    let decided = match carry.is_empty() {
                                        true => None,
                                        false => match Vm::decode_carry(&mut carry, &[], false) {
                                            Decoded::NeedMore => None,
                                            d => Some(d),
                                        },
                                    };
                                    match decided {
                                        Some(d) => Ok(d),
                                        None => match std::io::Read::read(stream, &mut b) {
                                            Ok(got) => Ok(Vm::decode_carry(
                                                &mut carry,
                                                &b[..got],
                                                got == 0,
                                            )),
                                            Err(e) => Err(e),
                                        },
                                    }
                                };
                                match r {
                                    // Same decode guard as the fast path; a NeedMore (no complete
                                    // codepoint yet) just re-polls, like a would-block.
                                    Ok(d) => match vm.decoded_value(d) {
                                        Some(v) => SockPoll::Ready(Ok(v)),
                                        None => SockPoll::WouldBlock,
                                    },
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        SockPoll::WouldBlock
                                    }
                                    Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                }
                            },
                        );
                    }
                    return Ok(self.sock_err(
                        "read would block: std.net sockets require the --parallel engine",
                    ));
                }
                Err((e, _)) => return Ok(self.sock_err(format!("{e}"))),
            }
        }
    }

    /// D6 — `Socket` methods: `read(n) -> Result[str]` (see [`Vm::socket_read`] — B1: it decodes through
    /// [`Vm::decode_carry`], never `from_utf8_lossy`), `write(s) -> Result[int]`, `close() -> nil`. On a
    /// would-block, under the M:N engine the fiber PARKS on the netpoller (rewind `ip`, set the
    /// `poll_park` sentinel); off it, there is no fiber to park (net targets `--parallel`).
    pub(super) fn socket_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "read" => {
                let r = self.socket_read(h, args, span);
                // B1 — the read's absolute deadline is LATCHED on the fiber (`poll_deadline`) so a park
                // + ip-rewind re-execution keeps the ORIGINAL `timeout_ms` budget instead of restarting
                // it. This logical `read` is over unless it parked (`poll_park` set ⇒ the very same call
                // resumes) — so drop the latch here; the next `read` gets a fresh budget.
                if self.poll_park.is_none() {
                    self.poll_deadline = None;
                }
                r
            }
            "read_bytes" => {
                let r = self.socket_read_bytes(h, args, span);
                // Same latch discipline as `read`: drop the deadline unless the op PARKED (the very
                // same call resumes) — a re-park must not re-arm the timeout budget.
                if self.poll_park.is_none() {
                    self.poll_deadline = None;
                }
                r
            }
            // `write(s[, timeout_ms])` (str) and R1's `write_bytes(b[, timeout_ms])` (raw `bytes`)
            // are the SAME write path — only the byte-extraction differs.
            "write" | "write_bytes" => {
                // The optional trailing int bounds writability.
                self.arity_range_err(method, args, 1, 2, span)?;
                if self.poll_timed_out {
                    self.poll_timed_out = false;
                    return Ok(self.sock_err("timeout"));
                }
                let timeout = self.parse_timeout_ms(args.get(1), span)?;
                let data = match (method, args.first()) {
                    ("write_bytes", Some(v)) => self.collect_bytes_arg("write_bytes", *v, span)?,
                    (_, Some(Value::Obj(sh))) => match self.heap.get(*sh) {
                        Obj::Str(s) => s.as_bytes().to_vec(),
                        _ => return Err(self.err("write expects a str".into(), span)),
                    },
                    _ => return Err(self.err("write expects a str".into(), span)),
                };
                let core = self.socket_core(h);
                let attempt = {
                    let mut guard = core.stream.lock().unwrap();
                    let Some(stream) = guard.as_mut() else {
                        return Ok(self.sock_err("write on a closed socket"));
                    };
                    match std::io::Write::write(stream, &data) {
                        Ok(got) => Ok(got),
                        Err(e) => Err((e, stream.as_raw_fd())),
                    }
                };
                match attempt {
                    Ok(got) => Ok(self.sock_ok(Value::Int(got as i64))),
                    Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if timeout.is_some_and(|t| t.poll_once) {
                            return Ok(self.sock_err("timeout"));
                        }
                        let target = PollPark {
                            key: core.key,
                            fd,
                            interest: poller::Interest::Write,
                            in_flight: Arc::clone(&core.in_flight),
                            deadline: timeout.map(|t| t.deadline),
                        };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking write (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(
                                fd,
                                poller::Interest::Write,
                                timeout.map(|t| t.deadline),
                                span,
                                move |vm| {
                                    let r = {
                                        let mut guard =
                                            core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                        let Some(stream) = guard.as_mut() else {
                                            return SockPoll::Ready(Ok(
                                                vm.sock_err("write on a closed socket")
                                            ));
                                        };
                                        std::io::Write::write(stream, &data)
                                    };
                                    match r {
                                        Ok(got) => {
                                            SockPoll::Ready(Ok(vm.sock_ok(Value::Int(got as i64))))
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            SockPoll::WouldBlock
                                        }
                                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                    }
                                },
                            );
                        }
                        Ok(self.sock_err(
                            "write would block: std.net sockets require the --parallel engine",
                        ))
                    }
                    Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.socket_core(h);
                // Disarm any pending poller registration (a `close` racing a park) before the fd drops;
                // a no-op in the common case (the owning fiber is running, not parked).
                poller::deregister(core.key);
                *core.stream.lock().unwrap() = None;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Socket has no method '{method}'"), span)),
        }
    }

    /// D6 — `Listener` methods: `accept() -> Result[Socket]`, `close() -> nil`. `accept` parks on the
    /// listening fd's readability (a pending connection) under the M:N engine, like `Socket::read`.
    pub(super) fn listener_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "accept" => {
                // `accept()` or `accept(timeout_ms)` — the optional trailing int bounds how long to
                // wait for an inbound connection (D6c). Mirrors `Socket::read`'s timeout handling.
                self.arity_range_err("accept", args, 0, 1, span)?;
                if self.poll_timed_out {
                    self.poll_timed_out = false;
                    return Ok(self.sock_err("timeout"));
                }
                let timeout = self.parse_timeout_ms(args.first(), span)?;
                let core = self.listener_core(h);
                let attempt = {
                    let guard = core.listener.lock().unwrap();
                    let Some(listener) = guard.as_ref() else {
                        return Ok(self.sock_err("accept on a closed listener"));
                    };
                    match listener.accept() {
                        Ok((stream, _peer)) => Ok(stream),
                        Err(e) => Err((e, listener.as_raw_fd())),
                    }
                };
                match attempt {
                    Ok(stream) => Ok(self.accept_socket_value(stream)),
                    Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if timeout.is_some_and(|t| t.poll_once) {
                            return Ok(self.sock_err("timeout"));
                        }
                        let target = PollPark {
                            key: core.key,
                            fd,
                            interest: poller::Interest::Read,
                            in_flight: Arc::clone(&core.in_flight),
                            deadline: timeout.map(|t| t.deadline),
                        };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking accept (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(
                                fd,
                                poller::Interest::Read,
                                timeout.map(|t| t.deadline),
                                span,
                                move |vm| {
                                    let r = {
                                        let guard =
                                            core.listener.lock().unwrap_or_else(|e| e.into_inner());
                                        let Some(listener) = guard.as_ref() else {
                                            return SockPoll::Ready(Ok(
                                                vm.sock_err("accept on a closed listener")
                                            ));
                                        };
                                        listener.accept()
                                    };
                                    match r {
                                        Ok((stream, _peer)) => {
                                            SockPoll::Ready(Ok(vm.accept_socket_value(stream)))
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            SockPoll::WouldBlock
                                        }
                                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                    }
                                },
                            );
                        }
                        Ok(self.sock_err(
                            "accept would block: std.net sockets require the --parallel engine",
                        ))
                    }
                    Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
                }
            }
            "addr" => {
                self.arity_err("addr", args, 0, span)?;
                let core = self.listener_core(h);
                let addr = {
                    let guard = core.listener.lock().unwrap();
                    match guard.as_ref() {
                        Some(l) => l
                            .local_addr()
                            .map(|a| a.to_string())
                            .map_err(|e| e.to_string()),
                        None => Err("addr on a closed listener".to_string()),
                    }
                };
                match addr {
                    Ok(a) => {
                        let v = self.alloc_str(a);
                        Ok(self.sock_ok(v))
                    }
                    Err(e) => Ok(self.sock_err(e)),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.listener_core(h);
                poller::deregister(core.key);
                *core.listener.lock().unwrap() = None;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Listener has no method '{method}'"), span)),
        }
    }

    /// D6 — wrap an accepted `TcpStream` (set non-blocking) into a fresh `Socket` handle, as a
    /// `Result::Ok`.
    pub(super) fn accept_socket_value(&mut self, stream: std::net::TcpStream) -> Value {
        stream.set_nonblocking(true).ok();
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key: core::next_poll_key(),
            in_flight: core::new_in_flight(),
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::Obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// D6 — the M:N park half shared by every would-block socket op. Returns `Ok(true)` if the fiber
    /// was parked on the netpoller; `Ok(false)` off the M:N engine (or inside a native callback, whose
    /// Rust-stack state can't be parked) — the caller then surfaces a `Result::Err` (net requires the
    /// `--parallel` engine; blocking the only thread would wedge the cooperative deadlock detector).
    /// `Err` only for a **concurrent op on a shared socket**: oneshot epoll allows ONE registration per
    /// fd, so a second fiber reaching a would-block op while the first is parked (`in_flight` already
    /// set) faults cleanly rather than corrupting the poller registry (review: Critical). On the park
    /// path it restores the pre-call operand stack (receiver THEN args — the exact layout `CallMethod`
    /// re-pops; unlike a 0-arg `recv` park, `read(n)`/`write(s)` must re-push their args), rewinds `ip`
    /// so the op re-executes on resume, and sets the `poll_park` sentinel for the worker loop.
    ///
    /// D6c — `target.deadline` (the optional `timeout_ms`) is honored ONLY on this snapshot-park path:
    /// the netpoller wakes the fiber on readiness OR at the deadline. The in-callback demote path
    /// (`native_reentry > 0`, where this returns `Ok(false)`) does NOT honor it — a demoted op
    /// backoff-polls in the kernel until readiness regardless of `timeout_ms` (a documented v1 gap;
    /// in-callback socket timeouts are out of scope, matching the in-callback connect-blocks behavior).
    pub(super) fn park_on_fd(
        &mut self,
        h: GcRef,
        args: &[Value],
        target: PollPark,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if self.mn.is_some() && self.native_reentry == 0 {
            // CANCELLATION CHECKPOINT — a socket park is a blocking op, so it is a cancel-delivery
            // point (the single choke point for `accept`/`read`/`write`/`connect`). A cancelled fiber
            // must NOT re-park: `poller::drain_sched` re-injects a poller-parked fiber on cancel and
            // the rewound op re-runs here — without this check it would would-block and re-park
            // forever (the every-instruction check that used to kill it at the dispatch loop top is
            // gone; see `run_until`), wedging the nursery.
            if !self.cancelled
                && self
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                self.cancelled = true;
                return Err(self.err("cancelled".to_string(), span));
            }
            // The `in_flight` guard: at most one op may be parked on a socket at a time. A second
            // concurrent op on a shared socket (`Arc`) faults rather than overwrite the registry entry
            // (which would drop the first fiber + leak `inflight`) or double-`add` the fd (EEXIST panic).
            if target.in_flight.swap(true, Ordering::AcqRel) {
                return Err(self.err(
                    "concurrent operation on a shared socket is not supported".into(),
                    span,
                ));
            }
            self.push(Value::Obj(h)); // receiver (deeper on the stack)
            for &a in args {
                self.push(a); // its args, in order, back on top
            }
            self.frames.last_mut().unwrap().ip -= 1;
            self.poll_park = Some(target);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// `Channel[T]` methods (C2/C4): `send` (move-on-send, deep-copied in), `recv` (FIFO; empty =
    /// deadlock fault under the sequential executor), `len`. Mirrors `interp::eval_channel_method` —
    /// error strings byte-identical (parity-tested).
    pub(super) fn channel_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                self.arity_err("send", args, 1, span)?;
                // B3.1: serialize once into the core (the wire form IS the airlock copy). A
                // non-sendable value (a frame-holding generator) faults gracefully with `send`'s span.
                let w = self.to_wire_at(args[0], span)?;
                // Closed-channel guard: a `send` after `close()` faults (Go-panic analog). A `close`
                // racing in the window between this check and the enqueue is benign — the value is
                // still buffered and drained before the close is observed (drain-before-close), exactly
                // like Go's racy `select`/close. Strict mutual exclusion isn't required.
                if self.channel_core(h).q.lock().unwrap().closed {
                    return Err(self.err("send on a closed channel".to_string(), span));
                }
                self.channel_send_wire(h, w);
                Ok(Value::Nil)
            }
            // `try_send` is the safe partner of `send`: channels are unbounded, so its only failure is
            // a closed channel — returns `false` then (never faults), `true` once the value is queued.
            "try_send" => {
                self.arity_err("try_send", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                if self.channel_core(h).q.lock().unwrap().closed {
                    return Ok(Value::Bool(false));
                }
                self.channel_send_wire(h, w);
                Ok(Value::Bool(true))
            }
            "recv" => {
                self.arity_err("recv", args, 0, span)?;
                // D5 owe #3 (Path C) — a `recv` reached INSIDE a native callback on the M:N engine
                // (`native_reentry > 0`) can't snapshot-park (its host-stack loop frame is not
                // capturable), so it DEMOTES the worker thread: block in place on the channel condvar +
                // spin a replacement, resuming on a sibling `send` (Go's `handoffp`). Handled before
                // `chan_recv_step` (which only covers the snapshot-park / cooperative-park / fault
                // paths). `demote_recv_block` is itself closed-aware (a `close` faults the demoted recv).
                // A `timer(ms)` channel is excluded from demote — it has no sibling sender to block on;
                // `chan_recv_step` synthesises its value (inline-sleep to the deadline) at any reentry.
                if self.mn.is_some()
                    && self.native_reentry > 0
                    && self.channel_core(h).timer.is_none()
                {
                    return match self.demote_recv_block(h, span)? {
                        RecvStep::Got(w) => Ok(self.from_wire(w)),
                        RecvStep::ClosedEmpty => {
                            Err(self.err("receive on a closed channel".to_string(), span))
                        }
                        // demote never parks (it blocks in place); a Parked here is impossible.
                        RecvStep::Parked => unreachable!("demote_recv_block never parks"),
                    };
                }
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => Ok(self.from_wire(w)),
                    // `chan_recv_step` already re-rooted the receiver + set `suspend`; the sentinel is
                    // never observed (`do_method_call` gates the result-push on `suspend`).
                    RecvStep::Parked => Ok(Value::Nil),
                    // Closed-and-drained: a distinct fault (not the deadlock fault) — no producer left.
                    RecvStep::ClosedEmpty => {
                        Err(self.err("receive on a closed channel".to_string(), span))
                    }
                }
            }
            "try_recv" => {
                // A1: non-blocking poll. Unlike `recv` it never touches `scheduler_stack` /
                // `native_reentry` / `suspend` / `ip` — it always returns immediately with an
                // `Option`: `Some(v)` if queued, `None` if empty. Mirrors `interp::eval_channel_method`.
                self.arity_err("try_recv", args, 0, span)?;
                let core = self.channel_core(h);
                let popped = core.q.lock().unwrap().queue.pop_front();
                // A `timer(ms)` channel reports ready (`Some(true)`) once its deadline has passed, even
                // with nothing queued — the level-triggered, non-blocking poll (used by `wait`'s
                // source-order scan and the `else` arm). `--parallel` may also have a real `true`
                // queued by the background send; either way `Some(true)`.
                // A tripped latch (`trip()`) reports ready forever, like a passed timer deadline.
                let popped = popped.or_else(|| {
                    if core.done_latch.load(Ordering::Relaxed) {
                        return Some(WireValue::Bool(true));
                    }
                    core.timer
                        .filter(|d| std::time::Instant::now() >= *d)
                        .map(|_| WireValue::Bool(true))
                });
                Ok(match popped {
                    Some(w) => {
                        let v = self.from_wire(w);
                        self.alloc_enum("Option", "Some", vec![v])
                    }
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            // `close()` marks the channel closed (idempotent) and wakes every parked / demoted
            // receiver so each re-runs and observes the close: a `for v in ch:` ends, a bare `recv`
            // faults. Mirrors `send`'s wake fan-out but delivers no value.
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.channel_core(h);
                core.q.lock().unwrap().closed = true;
                // Same routing as `channel_send_wire`: an inline outermost-`parallel:` builder VM
                // (`self.mn == None`) closing a channel must wake enlisted, parked receivers via the
                // held `mn_enlist_sched`, not just the local condvar. (Cross-nursery flat scheduler #2.)
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    // Wake any demoted OS thread blocked on this core's condvar (in-callback recv).
                    core.cv.notify_all();
                    // Cooperative engine: re-add every sibling fiber parked on this channel's `recv`.
                    self.wake_on_send(h);
                }
                Ok(Value::Nil)
            }
            // `trip()` flips the manual level-trigger latch (the primitive behind `std.cancel`'s
            // `done()`): the channel is then permanently ready (`recv`/`try_recv`/`wait` yield `true`).
            // Idempotent. Reuses `close()`'s exact wake fan-out so a parked `recv`/`wait` re-runs and
            // observes the latch — but does NOT set `closed` (a closed+empty `wait` arm is *skipped*;
            // we need it *ready*).
            "trip" => {
                self.arity_err("trip", args, 0, span)?;
                let core = self.channel_core(h);
                core.done_latch.store(true, Ordering::Relaxed);
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    core.cv.notify_all();
                    self.wake_on_send(h);
                }
                Ok(Value::Nil)
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let n = self.channel_core(h).q.lock().unwrap().queue.len();
                Ok(Value::Int(n as i64))
            }
            _ => Err(self.err(format!("type Channel has no method '{method}'"), span)),
        }
    }

    /// Enqueue an already-wire-serialized message into a channel and wake any receivers — the shared
    /// tail of `send`/`try_send` (after their respective closed-channel guards). On the M:N engine the
    /// enqueue + wake of every fiber parked on this channel is atomic under the sched lock
    /// ([`MnSched::send_wake`]) so a sibling parking concurrently can't be lost. With no scheduler
    /// (cooperative / cross-nursery / top-level) it enqueues + notifies the core condvar (a demoted
    /// in-callback recv) + re-adds any cooperative fiber parked on this channel.
    pub(super) fn channel_send_wire(&mut self, h: GcRef, w: WireValue) {
        let core = self.channel_core(h);
        // Route the enqueue+wake through whatever sched is in scope. A worker shell holds it in
        // `self.mn`; the INLINE outermost-`parallel:` builder VM runs with `self.mn == None` but holds
        // the global sched in `self.mn_enlist_sched` while early-enlisted outer scopes are still pending.
        // An inline-body send must still wake an enlisted, parked receiver (the cross-nursery wake), so
        // fall back to the held sched. The sender never parks, so this does not pull the inline owner
        // onto a worker yield/park path. (Cross-nursery flat scheduler — charges #1/#2.)
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
            let key = self.channel_core_ptr(h);
            sched.send_wake(key, &core, w);
        } else {
            core.q.lock().unwrap().queue.push_back(w);
            core.cv.notify_all();
            self.wake_on_send(h);
        }
    }

    /// One blocking-`recv` step on the snapshot-park / cooperative-park / fault paths (NOT the
    /// in-callback demote path, which `recv` handles directly). Pops a value if one is waiting,
    /// signals `ClosedEmpty` on a closed-and-drained channel, or parks the running fiber (re-rooting
    /// the receiver + rewinding `ip` so the calling op re-runs on resume, setting `suspend`). Shared
    /// by `recv` (`CallMethod`) and the `ChanRecvOrClosed` op (`for v in ch:`).
    pub(super) fn chan_recv_step(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        // CANCELLATION CHECKPOINT (engine-agnostic — the serial drain in `drain_cancelled_children`
        // depends on it, and it replaces the two `mn`-gated checks that used to sit inside the timer
        // and snapshot-park branches). At a `recv` checkpoint CANCEL WINS over a queued value, a
        // tripped done-latch and a fired timer — identically on both engines. `native_reentry == 0`
        // mirrors the park gate: inside a native callback the caller's Rust-stack state cannot be
        // unwound here.
        if self.native_reentry == 0
            && !self.cancelled
            && self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // A tripped latch (`trip()`) delivers `true` immediately and forever, on every engine — a
        // pending queued value (if any) still wins first. Checked before the timer/park logic so a
        // `done().recv()` on a manually-cancelled token never parks.
        {
            let core = self.channel_core(h);
            if core.done_latch.load(Ordering::Relaxed) {
                if let Some(w) = core.q.lock().unwrap().queue.pop_front() {
                    return Ok(RecvStep::Got(w));
                }
                return Ok(RecvStep::Got(WireValue::Bool(true)));
            }
        }
        // A `timer(ms)` channel delivers `true` once its deadline passes. Handled here (uniformly,
        // before the ordinary park logic) so it works regardless of the engine the receiver runs in
        // and where the timer was created. Delivery is scheduled at RECV time, in the recv's own
        // scheduler — not at construction (a timer made at the top level can be recv'd in a child).
        {
            let core = self.channel_core(h);
            if let Some(deadline) = core.timer {
                // A prior park's timer `send` may already have delivered — consume it first.
                if let Some(w) = core.q.lock().unwrap().queue.pop_front() {
                    return Ok(RecvStep::Got(w));
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
                }
                if self.mn.is_some() && self.native_reentry == 0 {
                    // --parallel, top level: schedule a one-shot background `send(true)` at the deadline
                    // (in THIS scheduler) and park. The pending timer is accounted `inflight` so it
                    // vetoes the deadlock predicate while the lone fiber waits; the job un-accounts it.
                    // (Cancel was checked at the top of this fn — the engine-agnostic checkpoint.)
                    let sched = self.mn.clone().unwrap();
                    let key = self.channel_core_ptr(h);
                    let core_job = Arc::clone(&core);
                    let sched_job = Arc::clone(&sched);
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    timer::submit_at(
                        deadline,
                        Box::new(move || {
                            sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                    self.park_recv(h);
                    return Ok(RecvStep::Parked);
                }
                // Cooperative VM / interp / a `--parallel` callback (`native_reentry > 0`): inline-sleep
                // to the deadline (single-thread, or an already-blocking host-stack context), synthesise.
                // Limitation (vs `sleep_ms`, which DEMOTES at `native_reentry > 0`): a `timer.recv()`
                // reached inside a native callback under `--parallel` pins THIS worker for the timeout
                // (no replacement is spun). Sound — siblings on the other N-1 workers still progress —
                // but lower throughput than `sleep_ms`'s demote. Acceptable for v1; demote-reuse is a
                // future improvement. The cooperative/interp inline-sleep blocks siblings the same way
                // their `sleep_ms` already does (single-thread).
                std::thread::sleep(deadline - now);
                return Ok(RecvStep::Got(WireValue::Bool(true)));
            }
        }
        // M:N snapshot-park path (empty-open parks the fiber; the worker loop files it into the wait
        // set). A fiber woken only to be cancelled must not re-park — the top-of-fn checkpoint above
        // already returned in that case (on BOTH engines).
        if self.mn.is_some() && self.native_reentry == 0 {
            let core = self.channel_core(h);
            let mut g = core.q.lock().unwrap();
            if let Some(w) = g.queue.pop_front() {
                return Ok(RecvStep::Got(w));
            }
            if g.closed {
                return Ok(RecvStep::ClosedEmpty);
            }
            drop(g);
            self.park_recv(h);
            return Ok(RecvStep::Parked);
        }
        // Cooperative / no-scheduler path. Pop + closed read are atomic under one lock.
        let core = self.channel_core(h);
        let mut g = core.q.lock().unwrap();
        if let Some(w) = g.queue.pop_front() {
            return Ok(RecvStep::Got(w));
        }
        let closed = g.closed;
        drop(g);
        if closed {
            return Ok(RecvStep::ClosedEmpty);
        }
        // Empty + open: under an active nursery scheduler (and not in a native callback) the fiber
        // suspends; the scheduler resumes it once a sibling `send`s.
        if !self.scheduler_stack.is_empty() && self.native_reentry == 0 {
            self.park_recv(h);
            return Ok(RecvStep::Parked);
        }
        // No scheduler (top level / single fiber) or a native callback on the cooperative engine: no
        // sibling could ever fill the channel — a real deadlock.
        Err(self.err(
            "recv on an empty channel: deadlock — nothing is queued and no task \
             can ever send (a consumer that waits mid-flight on a live producer \
             needs C5)"
                .to_string(),
            span,
        ))
    }

    /// Park the running fiber on an empty `recv`: re-root the receiver on the operand stack, rewind
    /// `ip` so the current op (`CallMethod(recv)` or `ChanRecvOrClosed`) re-executes on resume, and
    /// set the `suspend` sentinel. The scheduler / worker loop files the fiber into the channel's
    /// wait set; a sibling `send`/`close` wakes it.
    pub(super) fn park_recv(&mut self, h: GcRef) {
        self.push(Value::Obj(h));
        self.frames.last_mut().unwrap().ip -= 1;
        self.suspend = Some(h);
    }

    /// `wait:` runtime (§6d) — execute [`Op::WaitPoll`]. The `n` arm channel handles are on the
    /// operand stack (`stack[base..base+n]`, source order). Poll source order: the first channel with
    /// a queued value (or a fired timer) wins → drop the handles, push the value, jump to that arm's
    /// body. A closed+empty arm is skipped. Nothing ready → run `else` (jump), else inline-sleep to
    /// the soonest live timer and take it, else fault (all-closed) or block (cooperative multi-channel
    /// park; the M:N park is a follow-up — a blocking `wait` faults under `--parallel` for now).
    pub(super) fn op_wait_poll(&mut self, meta: &WaitMeta, span: Span) -> Result<(), RuntimeError> {
        // CANCELLATION CHECKPOINT — engine-agnostic, mirroring `chan_recv_step`: cancel wins over a
        // ready arm / a fired timer, and it covers the COOPERATIVE multi-channel park below (which
        // had no check at all, so serial's cancel drain could never unwind a `wait`-parked fiber).
        if self.native_reentry == 0
            && !self.cancelled
            && self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        let n = meta.n;
        let base = self.stack.len() - n;
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        for i in 0..n {
            let Value::Obj(h) = self.stack[base + i] else {
                unreachable!("wait arm operand is not a channel handle");
            };
            let core = self.channel_core(h);
            let (popped, closed) = {
                let mut g = core.q.lock().unwrap();
                (g.queue.pop_front(), g.closed)
            };
            if let Some(w) = popped {
                let v = self.from_wire(w);
                self.take_wait_arm(base, v, meta.arm_targets[i]);
                return Ok(());
            }
            // A tripped latch (`trip()`) is ready like a fired timer — take the arm with `true`.
            if core.done_latch.load(Ordering::Relaxed) {
                self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
                return Ok(());
            }
            if let Some(deadline) = core.timer {
                // A timer channel is never closed and always eventually ready: fired now → take it;
                // otherwise a live waiter whose deadline we may sleep to below.
                if std::time::Instant::now() >= deadline {
                    self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
                    return Ok(());
                }
                all_closed = false;
                if soonest.is_none_or(|(_, d)| deadline < d) {
                    soonest = Some((i, deadline));
                }
            } else if !closed {
                all_closed = false;
            }
        }
        // Nothing ready → the non-blocking `else` fallback.
        if let Some(t) = meta.else_target {
            self.stack.truncate(base);
            self.frames.last_mut().unwrap().ip = t;
            return Ok(());
        }
        // Every arm closed+empty, no `else`, no timer → distinct fault. (A live timer arm set
        // `all_closed = false` above, so this fires only when there is genuinely nothing to wait on.)
        if all_closed {
            return Err(self.err("wait: all channels closed".to_string(), span));
        }
        // Block on all live arms. The N arm handles are on the stack (they root the channels + re-supply
        // the poll on resume). A live timer arm (`soonest`) is just another arm bucket on the M:N paths.
        let keys: Vec<GcRef> = (0..n)
            .map(|i| match self.stack[base + i] {
                Value::Obj(h) => h,
                _ => unreachable!("wait arm operand is not a channel handle"),
            })
            .collect();
        // M:N (`--parallel`) snapshot-park, top level: rewind to re-run `WaitPoll` on wake and set
        // `wait_suspend`; the worker loop captures each arm's (key, core) WHILE the fiber heap is live
        // (`Disp::WaitPark`) and `MnSched::park_wait` files ONE shared token in every arm bucket. A
        // `send`/`close` to any arm claims the fiber once and sweeps the rest (lost-wakeup-safe via the
        // park-gap re-check). Mirrors the single-`recv` `park_recv`/`Disp::Park` path, generalized to N.
        if self.mn.is_some() && self.native_reentry == 0 {
            // WAIT-1 fix — a live timer arm is NOT taken by an inline-sleep (which would pin the worker
            // and strand a sibling `send` that lands mid-window). Instead, for the soonest timer arm
            // submit ONE background `send_wake(true)` at its deadline (in THIS scheduler) and fall
            // through to the snapshot-park, so the timer channel parks as an ordinary arm bucket. On
            // wake the re-poll pops a sibling's value (timer NOT taken) OR finds `now >= deadline` and
            // takes the timer arm. The existing `WaitPark` claimed-CAS sweep guarantees exactly one of
            // {a sibling send/close, the timer's own deadline send} wins (WAIT-2 late-alarm = CAS
            // already claimed = no-op; WAIT-3 same-instant = single claimed CAS = one winner).
            if let Some((i, deadline)) = soonest {
                // A fiber about to be cancelled must not arm a stray timer — the top-of-fn
                // cancellation checkpoint already returned in that case (on BOTH engines).
                let sched = self.mn.clone().unwrap();
                let key = self.channel_core_ptr(keys[i]);
                let core_job = self.channel_core(keys[i]);
                let sched_job = Arc::clone(&sched);
                // Arm ONCE per timer channel: a re-park of this same wait (woken with no consumable
                // value — e.g. a sibling `close` on another arm) re-runs WaitPoll and re-enters this
                // block, but the CAS fails the second time so we do NOT submit a redundant job. The
                // first job survives the re-park (it captures the stable `key`+`core` and wakes
                // whatever token is in this bucket at the deadline). Fresh `timer(ms)` ⇒ fresh core
                // ⇒ `armed=false`, so no reset is needed.
                if core_job
                    .timer_armed
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Account the pending timer `inflight` (gated STRICTLY on `soonest.is_some()`) so it
                    // vetoes the deadlock predicate while a lone fiber waits; the job un-accounts it.
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    timer::submit_at(
                        deadline,
                        Box::new(move || {
                            sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                }
            }
            self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll on resume
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        // M:N inside a native callback (`native_reentry > 0`): a host-stack loop frame sits between the
        // worker loop and here, so we cannot snapshot-park. Demote: block this worker in place, polling
        // all N arm queues in source order on a bounded backoff (mirrors `demote_recv_block`). A live
        // timer arm (`soonest`) is threaded in: after the source-order channel scan fails, the demote
        // loop takes the timer arm once `now >= deadline` (so a real send still beats the timer), and
        // clamps its backoff to the deadline. Lower throughput but sound — the documented v1 limit (§6d).
        if self.mn.is_some() {
            let arms: Vec<(usize, Arc<ChannelCore>)> = keys
                .iter()
                .map(|&h| (self.channel_core_ptr(h), self.channel_core(h)))
                .collect();
            let (arm_index, w) = self.demote_wait_block(arms, soonest, span)?;
            let v = self.from_wire(w);
            self.take_wait_arm(base, v, meta.arm_targets[arm_index]);
            return Ok(());
        }
        // Cooperative VM / interp (single-threaded) — a live timer arm inline-sleeps to the soonest
        // deadline and takes it (the frozen parity oracle; reached only when `mn.is_none()`).
        if let Some((i, deadline)) = soonest {
            let now = std::time::Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
            self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
            return Ok(());
        }
        if !self.scheduler_stack.is_empty() && self.native_reentry == 0 {
            // Cooperative multi-channel park: keep the N handles on the stack (they root the channels
            // + re-supply the poll), rewind to re-run `WaitPoll` on wake, and register the fiber on
            // every live arm channel via `wait_suspend` (consumed by `run_child`).
            self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll on resume
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        // No scheduler (top level / single fiber) or inside a native callback: no sibling could ever
        // fill the channels — a real deadlock (mirrors `chan_recv_step`'s sequential `recv` fault).
        Err(self.err(
            "wait on channels that are all empty: deadlock — nothing is queued and no task \
             can ever send"
                .to_string(),
            span,
        ))
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push the received
    /// value, and jump to the arm body's target ip (the bind/assign/discard prologue).
    pub(super) fn take_wait_arm(&mut self, base: usize, value: Value, target: usize) {
        self.stack.truncate(base);
        self.push(value);
        self.frames.last_mut().unwrap().ip = target;
    }

    /// `Shared[T]` methods (C3/C4): `get` (copies out), `set` (copies in), `update` (read-modify-write
    /// via the re-entrant call path). Mirrors `interp::eval_shared_method`. The box is re-rooted on
    /// the operand stack across `update`'s nested call (the receiver was popped in `do_method_call`).
    pub(super) fn shared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the lock, then reconstruct into this heap (one
                // round-trip == the old deep_clone-out).
                let w = self.shared_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                *self.shared_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "update" => {
                self.arity_err("update", args, 1, span)?;
                let f = args[0];
                let core = self.shared_core(h);
                // B3.3-threads: serialise the whole read-modify-write so concurrent OS-thread updates
                // can't lose each other (Shared[T]'s core contract). Held only under `--parallel`
                // (the cooperative engine is single-thread, so it keeps its current behavior and
                // never risks deadlocking a same-box nested update). The value lock `v` is still held
                // only briefly — read here, write at the end — so the closure may freely re-enter
                // `get`/`set` (or `update` on a *different* box). A `--parallel` closure that re-enters
                // `update` on the SAME box deadlocks: a documented edge (it could only lose-update
                // before). The handle is re-rooted on the operand stack so the nested call's GC keeps
                // the core's contents traced (the receiver was popped off the stack in `do_method_call`).
                let _serialise = if self.parallel {
                    Some(core.update_lock.lock().unwrap())
                } else {
                    None
                };
                let w = core.v.lock().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_at(next, span)?;
                *core.v.lock().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Shared has no method '{method}'"), span)),
        }
    }

    /// `RwShared[T]` methods: `get`/`set` (read/write-guarded copy out/in), `read(f)` (SHARED read
    /// guard: clone out, drop guard, run `f`, return its result — NO write-back), `write(f)`
    /// (EXCLUSIVE write guard: a write-locked read-modify-write, the `Shared.update` shape under a
    /// `RwLock`). Mirrors `interp::eval_rwshared_method`. As with `Shared.update`, the lock guard is
    /// dropped across the user closure (a `RwLock` guard is not reentrant) and the receiver is
    /// re-rooted on the operand stack so the nested call's GC keeps the core's contents traced (the
    /// receiver was popped off the stack in `do_method_call`). `write`'s whole RMW is serialised
    /// across threads by a separate `update_lock` (held only under `--parallel`) — the `RwLock` write
    /// guard alone is NOT enough because it is dropped across the closure, so two writers could clone
    /// the same base and lose an update (same discipline as `Shared.update`). A closure that
    /// re-acquires the SAME box's write lock (or a write inside a read) deadlocks — a documented edge,
    /// mirroring `Shared.update`'s same-box re-entry limit.
    pub(super) fn rwshared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the SHARED read guard, reconstruct into this heap.
                let w = self.rwshared_core(h).v.read().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                *self.rwshared_core(h).v.write().unwrap() = w;
                Ok(Value::Nil)
            }
            "read" => {
                self.arity_err("read", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // SHARED read guard: clone the value out, then DROP the guard before invoking `f`
                // (the guard is not reentrant; dropping it also lets other readers/a writer proceed).
                // No write-back — `read` returns `f`'s result.
                let w = core.v.read().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let result = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                result
            }
            "write" => {
                self.arity_err("write", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // Serialise the whole read-modify-write so concurrent OS-thread writes can't lose each
                // other (the box's contract, exactly like `Shared.update`). The `RwLock` write guard
                // alone is NOT enough: it must be DROPPED across the user closure (not reentrant), so
                // two `write`s could clone the same base and lose an update — hence a separate
                // `update_lock` held for the entire RMW *only under `--parallel`*. The cooperative
                // engine is single-thread, so it never takes `update_lock` (taking it would needlessly
                // deadlock a same-box nested write). The value lock `v` is taken only briefly (read
                // here, write at the end), so the closure may freely re-enter `get`/`set`/`read` (or
                // `write` on a *different* box). The handle is re-rooted on the operand stack so the
                // nested call's GC keeps the core's contents traced.
                let _serialise = if self.parallel {
                    Some(core.update_lock.lock().unwrap())
                } else {
                    None
                };
                let w = core.v.write().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_at(next, span)?;
                *core.v.write().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type RwShared has no method '{method}'"), span)),
        }
    }

    /// `Atomic[T]` methods: `load` (copy out), `store` (copy in), `exchange` (swap, returns old),
    /// `cas(expected, new) -> bool` (swap iff the box equals `expected`), `add`/`sub` (numeric RMW,
    /// returns the new value). Each is a single lock-op-unlock, so the RMW is atomic across threads —
    /// no user closure runs under the lock (unlike `Shared.update`), so no `update_lock` is needed.
    /// Mirrors `interp::eval_atomic_method`. `add`/`sub` use the language's `checked_add`/`checked_sub`
    /// (int overflow faults, like the `+`/`-` operators) and plain float arithmetic.
    pub(super) fn atomic_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let w = self.atomic_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                *self.atomic_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let new_w = self.to_wire_at(args[0], span)?;
                let core = self.atomic_core(h);
                let old = {
                    let mut g = core.v.lock().unwrap();
                    std::mem::replace(&mut *g, new_w)
                };
                Ok(self.from_wire(old))
            }
            "cas" => {
                self.arity_err("cas", args, 2, span)?;
                let core = self.atomic_core(h);
                // Hold the value lock across compare+swap so the CAS is atomic. `from_wire`/`to_wire`/
                // `values_equal` borrow `self`, not the guard (which borrows the cloned `Arc`), so the
                // lock can stay held while they run.
                let mut g = core.v.lock().unwrap();
                let cur = self.from_wire(g.clone());
                let swapped = self.values_equal(cur, args[0]);
                if swapped {
                    *g = self.to_wire_at(args[1], span)?;
                }
                Ok(Value::Bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.to_wire_at(args[0], span)?;
                let core = self.atomic_core(h);
                let mut g = core.v.lock().unwrap();
                let new = match (&*g, &delta) {
                    (WireValue::Int(a), WireValue::Int(b)) => {
                        let (r, label) = if method == "add" {
                            (a.checked_add(*b), "Add")
                        } else {
                            (a.checked_sub(*b), "Sub")
                        };
                        WireValue::Int(r.ok_or_else(|| {
                            self.err(format!("integer overflow in {label}"), span)
                        })?)
                    }
                    (WireValue::Float(a), WireValue::Float(b)) => {
                        WireValue::Float(if method == "add" { a + b } else { a - b })
                    }
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    _ => {
                        return Err(self.err(format!("type Atomic has no method '{method}'"), span));
                    }
                };
                *g = new.clone();
                drop(g);
                Ok(self.from_wire(new))
            }
            _ => Err(self.err(format!("type Atomic has no method '{method}'"), span)),
        }
    }

    /// `Executor` methods (C5/escape hatch): `submit` (enqueue a detached task closure, rejected once
    /// shut), `shutdown` (graceful — drain FIFO via the re-entrant call path), `shutdown_now` (discard
    /// pending). Mirrors `interp::eval_executor_method` — error strings byte-identical (parity-tested).
    /// The executor handle is re-rooted on the operand stack across the drain, and each popped task is
    /// rooted across its nested call (the receiver was popped in `do_method_call`).
    pub(super) fn executor_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                self.arity_err("submit", args, 1, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)"
                                .to_string(),
                            span,
                        ));
                    }
                    // Option B — gate the submitted job for generator-global reachability, exactly like a
                    // nursery spawn's spawn-site gate (`register_task` → `check_task_generator_reach`). An
                    // `Executor` job is a zero-arg closure invoked with `vec![]`, so wrap the live callee in a
                    // no-arg `PendingCall::Call` and run the SAME conservative reach scan. This reads
                    // submit-time globals; the drain (`shutdown`) re-gates against the globals as they then
                    // stand, closing the submit→shutdown TOCTOU. Runs on the host VM during body execution on
                    // BOTH engines (serial and M:N), so their verdicts agree by construction. A generator-free
                    // program short-circuits at zero cost inside `check_task_generator_reach`.
                    self.check_task_generator_reach(
                        &PendingCall::Call {
                            callee: args[0],
                            args: Vec::new(),
                            span,
                        },
                        span,
                    )?;
                    // The task closure crosses the airlock **by value** on BOTH engines
                    // (`wire_callable` → `to_wire`: proto + deep-copied captures + home index), exactly
                    // like plain `spawn` (`cross_spawn_callee`). This is the sole serial==M:N invariant:
                    // routing coop through the SAME wire path runs the ref/Ref + generator airlock
                    // enforcement on the cooperative engine too, and isolates captures at submit time, so
                    // serial and M:N behave identically for every submitted closure. (Earlier the coop
                    // branch queued the callable's own `Handle` — captures shared by reference, bypassing
                    // `to_wire` — to mirror the tree-walk `interp` oracle; that oracle has been removed, so
                    // the by-handle preservation was pure serial-vs-M:N divergence and is retired.)
                    // Under `--parallel` a pool-thread drain rebuilds the closure from the wire value;
                    // under the cooperative engine the inline drain (`from_wire` at `shutdown`) rebuilds an
                    // isolated closure over this same heap home. Queued captures stay rooted via the
                    // executor handle's `children()` (the `Closure` arm of `collect_core_gcrefs`).
                    let w = self.wire_callable(args[0], span)?;
                    g.queue.push_back(w);
                }
                Ok(Value::Nil)
            }
            "shutdown" => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                // Option B (drain-time re-gate) — mirror the LAZY-nursery join re-gate: before draining,
                // conservatively fault if ANY queued job could reach a live generator module-global AS THE
                // GLOBALS STAND NOW. `submit` gates against submit-time globals, but a job only RUNS — and the
                // M:N drain snapshot (`drain_executor_on_pool` → `ensure_snapshot`, which freezes a generator
                // global to a `Poison`→`Nil`) is only taken — at shutdown, so a global reassigned to a
                // generator between `submit` and `shutdown` would slip past the submit gate (serial then runs
                // the real generator while an M:N worker replays Nil, diverging). This runs on the host VM on
                // BOTH engines against the same live globals, BEFORE the M:N snapshot freezes them, so serial
                // == M:N by construction.
                self.gate_executor_queue(&core, span)?;
                if self.parallel {
                    // B3.6: drain the whole queue under the lock (drop the guard before running any
                    // task — never hold the core lock across an invoke), then run the tasks on the
                    // bounded pool. Output flushes in submission order; the first fault propagates.
                    let tasks: Vec<WireValue> =
                        core.inner.lock().unwrap().queue.drain(..).collect();
                    self.drain_executor_on_pool(tasks, span)?;
                } else {
                    // Cooperative engine: inline FIFO drain. Root the executor handle across the drain
                    // (its remaining queue is traced via it); each popped task is rooted on the stack
                    // across its re-entrant call. A submitted task runs INLINE on the entry `Vm`, so
                    // it reads the one shared `host.stdin` — which is exactly the contract (the M:N
                    // drain's workers get the same source via `spawn_worker`).
                    self.push(Value::Obj(h));
                    loop {
                        // Pop under the lock, then DROP the guard before the re-entrant call.
                        let task = core.inner.lock().unwrap().queue.pop_front();
                        let Some(task) = task else { break };
                        let task = self.from_wire(task);
                        self.push(task);
                        let r = self.guarded(|vm| vm.invoke_value(task, vec![], span));
                        self.pop();
                        r?;
                    }
                    self.pop(); // the executor root
                }
                Ok(Value::Nil)
            }
            "shutdown_now" => {
                self.arity_err("shutdown_now", args, 0, span)?;
                let core = self.executor_core(h);
                let mut g = core.inner.lock().unwrap();
                g.shut = true;
                g.queue.clear();
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Executor has no method '{method}'"), span)),
        }
    }

    /// Option B (drain-time re-gate) — before a shut `Executor` drains its queue, conservatively fault
    /// if ANY pending job could reach a live generator held in a module global AS THE GLOBALS STAND NOW.
    /// This is the `Executor` analogue of the lazy-nursery JOIN-time re-gate ([`Vm::join_nursery`] →
    /// [`Vm::check_task_generator_reach`]). Each queued job is a zero-arg closure, so we reconstruct its
    /// live callee (`from_wire` — a `Handle` on the coop engine resolves to the same closure, a by-value
    /// `Closure` on M:N rebuilds it over the host home module) and run the SAME reach scan the nursery
    /// path uses. The queue is peeked (cloned), never consumed — the real drain still runs every job.
    /// Short-circuits at zero cost when no generator body exists, or no module global embeds a live
    /// generator, so no per-job `from_wire` happens for the common case.
    pub(super) fn gate_executor_queue(
        &mut self,
        core: &Arc<ExecutorCore>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // Same two short-circuits as `check_task_generator_reach`, hoisted so a generator-free (or
        // no-live-global-generator) program never pays a per-job `from_wire`.
        if !self.has_generators || !self.any_module_global_embeds_generator() {
            return Ok(());
        }
        // Peek (clone) the queued wire values — gating must not consume the queue the drain will run.
        let queued: Vec<WireValue> = core.inner.lock().unwrap().queue.iter().cloned().collect();
        for w in queued {
            let callee = self.from_wire(w);
            self.check_task_generator_reach(
                &PendingCall::Call {
                    callee,
                    args: Vec::new(),
                    span,
                },
                span,
            )?;
        }
        Ok(())
    }

    /// Mirrors `interp::Interp::drain_live_executors` (C5 / A2): at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`/`shutdown_now`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, first-fault-aborts-siblings). A hard
    /// `std.os.exit` is not drained (the caller gates on `pending_exit`); a task that calls
    /// `os.exit` mid-drain stops the remaining drain.
    pub(super) fn drain_live_executors(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        // Snapshot the handles: a drained task may create new executors; reap only those alive at
        // exit (parity with the interpreter's `Vec<Rc>` snapshot).
        let execs = self.executors.clone();
        for h in execs {
            let shut = self.executor_core(h).inner.lock().unwrap().shut;
            if shut {
                continue;
            }
            self.executor_method(h, "shutdown", &[], span)?;
            if self.pending_exit.is_some() {
                break; // a drained task called os.exit — hard halt, stop draining
            }
        }
        Ok(())
    }
}
