//! VM: sockets and listeners: connect, read, write, accept, poll timeouts, parking on an fd.
//! Split out of `src/vm/netio.rs`. `super::*` is the parent module.

use super::netio::Decoded;
use super::netio::sock_would_block_msg;
use super::netio::write_from;
use super::*;
use crate::base::builtin_method::{ListenerMethod, SocketMethod};

impl Vm {
    /// D6 — clone out the shared `Arc<SocketCore>`/`Arc<ListenerCore>` behind a handle (refcount bump),
    /// mirroring [`channel_core`](Vm::channel_core). The `Arc` is held only for the calling method, so
    /// locking the fd does not borrow the heap.
    pub(super) fn socket_core(&self, h: GcRef) -> Arc<SocketCore> {
        match self.heap.get(h) {
            Obj::Socket(core) => Arc::clone(core),
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
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => unreachable!("socket_core on non-socket"),
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

    /// N3(a) — the ONE builder for the "read took a partial codepoint then could not finish it" error,
    /// shared by every path that can hit it: the poll-once return, the netpoller-park timeout re-entry,
    /// and the in-callback demote timeout. `owed` is the carried (retained) byte count. Distinct from
    /// `Err("timeout")` — which is documented as "nothing arrived" — because 1-3 bytes ARE off the wire
    /// (kept on the socket), so a retry finishes the codepoint byte-exactly.
    pub(super) fn sock_incomplete_err(&mut self, owed: usize) -> Value {
        self.sock_err(format!(
            "incomplete utf-8: the read landed mid-codepoint ({owed} byte(s) carried and retained) — \
             read this socket again to finish it"
        ))
    }

    /// N2/N3(a) — drop the per-op fiber latches (`poll_deadline` timeout budget + `poll_partial`
    /// taken-partial flag) once a socket op is over, UNLESS it parked (`poll_park` set ⇒ the very same
    /// call resumes and still owns them). Called from every socket/listener op arm so the NEXT op on
    /// this fiber starts with a fresh budget and no stale partial flag. Symmetric-clear is load-bearing:
    /// a leaked `poll_deadline` would corrupt the next read's timeout, a leaked `poll_partial` would
    /// make it lie "incomplete" when nothing arrived.
    pub(super) fn drop_poll_latch(&mut self) {
        if self.poll_park.is_none() {
            self.poll_deadline = None;
            self.poll_partial = None;
            self.poll_written = None;
        }
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
        let addr = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(sh)
        {
            s.to_string()
        } else {
            return Err(self.err(format!("std.net.{name} expects an address string"), span));
        };
        match name {
            "connect" => match crate::native::net::connect_nonblocking(&addr) {
                // Connected synchronously — wrap + return at once. RARE, and this comment used to
                // claim it was "the common loopback case", which is measured false on Linux: a
                // non-blocking `connect` reports `EINPROGRESS` even to a LIVE loopback listener
                // (and to a closed loopback port — `connect_ex(('127.0.0.1', <closed>))` → `115`).
                // The arm below is the normal path, not the fallback, which is why W7-59's gate
                // choice there decides `net.connect`'s whole behaviour rather than a corner of it.
                Ok((stream, false)) => {
                    Ok(self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight()))
                }
                // Handshake in flight: park the fiber on writability under the M:N engine; off it,
                // block until the handshake settles — everywhere except an eager `Executor` job,
                // whose thread is shared (W7-59).
                Ok((stream, true)) => {
                    let connect_mode = self.block_mode(WaitSpec::Connect);
                    if connect_mode == BlockMode::Park {
                        self.park_on_connect(stream, span);
                        Ok(Value::nil()) // parked sentinel; `poll_park` gates the result-push at `do_call`
                    } else if connect_mode == BlockMode::Refuse {
                        // W7-59 — an eager `Executor` job. A job does not own its thread: it runs on the
                        // bounded, process-wide `vm::pool` (`worker_count()`, never grown on demand) with
                        // no `MnSched` under it to spin a replacement, so blocking here steals width from
                        // every other job and every `parallel:` nursery sharing that pool — measured at
                        // `CHEZZI_THREADS=1` as a 10 s pin on a black-hole address. Same family as
                        // `W7-40`'s R2, and the same message the four sibling ops give this context.
                        //
                        // This `Connect` row is deliberately NARROWER than the siblings' `Socket`
                        // row of `block::mode`, and the difference is not an oversight.
                        // `accept`/`read` wait on a CHEZZI peer — a fiber that can only run on the very
                        // thread they would block — so blocking the one thread that owns it is
                        // self-starvation (`W7-40` R1). A `connect` handshake is completed by the
                        // KERNEL, so no chezzi party is starved by waiting for it, and both ancestors
                        // block: CPython `socket.connect` from the main thread returns in 0.1 ms, Go
                        // `net.Dial` from the main goroutine in 314 µs — refusing it here would be a
                        // divergence with nothing behind it.
                        Ok(self.sock_err(sock_would_block_msg("connect")))
                    } else {
                        // Everywhere the thread is the program's own — top-level `main` on the
                        // default engine, a `connect` inside a native callback on M:N: block, but
                        // through the SHARED demote loop rather than a private sleep-spin, so the wait
                        // gets that loop's escapes (`--timeout`, `cancel`, a run-wide `os.exit` — W7-47 —
                        // and a torn-down nursery) and, on a worker shell, `block_enter`'s
                        // replacement worker.
                        //
                        // The 10 s connect cap is deliberately NOT clamped by `self.deadline`, unlike the
                        // spin this replaces. `demote_block_socket` re-reads the run deadline at the top
                        // of every iteration and caps its kernel wait at `DEMOTE_POLL_BACKOFF`, so a
                        // `--timeout` is observed within 5 ms and raised as a HARD `Err`. Clamping would
                        // make the op's OWN deadline expire in the same instant, and the op's expiry is
                        // the CATCHABLE `Err("timeout")` — i.e. the clamp would turn W7-18's swallow into
                        // a race instead of preventing it.
                        let dl = std::time::Instant::now()
                            + std::time::Duration::from_secs(CONNECT_BLOCK_TIMEOUT_SECS);
                        let fd = stream.as_raw_fd();
                        // The closure is `FnMut`, so it cannot move `stream` out on the ready edge —
                        // hold it in an `Option` and `take()` it there. It must outlive the wait either
                        // way: it owns the fd the poller watches.
                        let mut pending = Some(stream);
                        let v = self.demote_block_socket(
                            fd,
                            poller::Interest::Write,
                            Some(dl),
                            span,
                            move |vm| {
                                let Some(s) = pending.as_ref() else {
                                    // Unreachable: the ready edge below is the only `take`, and it also
                                    // ends the loop.
                                    return SockPoll::Ready(Ok(
                                        vm.sock_err("connect failed: already completed")
                                    ));
                                };
                                match crate::native::net::finish_connect(s) {
                                    // SO_ERROR clear AND the peer is reachable ⇒ connected.
                                    Ok(()) if s.peer_addr().is_ok() => {
                                        let s = pending.take().expect("checked above");
                                        SockPoll::Ready(Ok(vm.alloc_socket_ok(
                                            s,
                                            core::next_poll_key(),
                                            core::new_in_flight(),
                                        )))
                                    }
                                    Err(e) => SockPoll::Ready(Ok(
                                        vm.sock_err(format!("connect failed: {e}"))
                                    )),
                                    Ok(()) => SockPoll::WouldBlock, // not settled yet
                                }
                            },
                        )?;
                        // W7-18 — kept as a fence, no longer the mechanism. `demote_block_socket`'s own
                        // rung raises the run deadline as a HARD `Err` within 5 ms, so this fires only
                        // if a future change re-clamps the op deadline (see above) or reorders that
                        // loop's two deadline checks — either of which would let a `--timeout` come
                        // back as the CATCHABLE `Err("timeout")` and be swallowed by a `recover:`,
                        // exactly what the hard-abort contract forbids. It costs one `Instant::now()`,
                        // and only when a `--timeout` is armed at all.
                        self.deadline_halt(span)?;
                        Ok(v)
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
                        closed: core::new_closed(),
                    });
                    let v = Value::obj(self.heap.alloc(Obj::Listener(core)));
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
            closed: core::new_closed(),
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::obj(self.heap.alloc(Obj::Socket(core)));
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
    pub(super) fn park_on_connect(&mut self, stream: std::net::TcpStream, span: Span) {
        let key = core::next_poll_key();
        let in_flight = core::new_in_flight();
        in_flight.store(true, Ordering::Release); // mark parked (matches `park_on_fd`'s swap(true))
        let fd = stream.as_raw_fd();
        self.pending_connect = Some(ConnectInProgress {
            stream,
            key,
            in_flight: Arc::clone(&in_flight),
            span,
        });
        // A `connect` never carries a user timeout (the `connect` surface takes only an address), so it
        // parks until readiness, a `drain_family` re-inject on a sibling fault — or, W7-18, the RUN's
        // `--timeout` deadline, which is the only thing that can set `deadline` here. That makes
        // `poll_timed_out` on a connect resume unambiguous: it is always the hard halt, never an op
        // timeout, and the `pending_connect` arm in `run_one_fiber` raises it as one.
        self.poll_park = Some(PollPark {
            key,
            fd,
            interest: poller::Interest::Write,
            in_flight,
            closed: core::new_closed(),
            deadline: self.deadline,
        });
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
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        let n = match args.first() {
            Some(v) if self.is_integral(*v) => {
                (self.int_of(*v).max(0) as usize).min(MAX_SOCKET_READ)
            }
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
            let bv = Value::obj(self.heap.alloc(Obj::Bytes(Box::default())));
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
                let bv = Value::obj(self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())));
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
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "read_bytes", span)? {
                    return Ok(v); // parked (sentinel), or refused (`would block`)
                }
                // No fiber to park: block the thread in place, where that starves nobody
                // (the `Socket` row of `block::mode`; `park_on_fd` returned the refusal elsewhere).
                self.demote_block_socket(fd, poller::Interest::Read, deadline, span, move |vm| {
                    let mut b = vec![0u8; n];
                    let r = {
                        let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                        let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
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
                            let bv =
                                Value::obj(vm.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())));
                            SockPoll::Ready(Ok(vm.sock_ok(bv)))
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
        }
    }

    /// D6/B1 — `Socket.read(n) -> Result[str]` / `read(n, timeout_ms)`. On a would-block, on an M:N
    /// worker shell the fiber PARKS on the netpoller (re-root the receiver, rewind `ip` so the op
    /// re-executes on resume, set the `poll_park` sentinel — mirrors the channel `recv` park, but
    /// routed to the poller). Off a worker shell (top level) there is no fiber to park, so the op
    /// fails loud (a documented v1 fallback — net targets `--parallel`).
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
        if self.poll_timeout_check(span)? {
            // N3(a) — if this read took a partial codepoint off the wire before the deadline fired
            // (`poll_partial` was latched at the NeedMore point and survived the park), classify it as
            // `incomplete utf-8` (those bytes are carried/retained), NOT `timeout` (which means nothing
            // arrived). Same rule as the poll-once path below.
            return Ok(match self.poll_partial {
                Some(owed) => self.sock_incomplete_err(owed),
                None => self.sock_err("timeout"),
            });
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        // Cap the per-call buffer: a huge `read(n)` (caller-controlled) must not eagerly allocate
        // gigabytes before a byte arrives (review). The caller already loops for large payloads —
        // `read` returns the actual count.
        let n = match args.first() {
            Some(v) if self.is_integral(*v) => {
                (self.int_of(*v).max(0) as usize).min(MAX_SOCKET_READ)
            }
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
                        d @ Decoded::Text(_) | d @ Decoded::Fail(_) => Some(d),
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
                        // N3(a) — latch the taken-partial state on the fiber so a later timeout (the
                        // netpoller-park re-entry, which re-executes this op after `took_partial` is
                        // lost) reports `incomplete utf-8` instead of `timeout`. `owed` = the carried
                        // (retained) bytes; a fresh short lock, like the poll-once path.
                        self.poll_partial =
                            Some(core.carry.lock().unwrap_or_else(|e| e.into_inner()).len());
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
                            return Ok(self.sock_incomplete_err(owed));
                        }
                        return Ok(self.sock_err("timeout"));
                    }
                    let target = PollPark {
                        key: core.key,
                        fd,
                        interest: poller::Interest::Read,
                        in_flight: Arc::clone(&core.in_flight),
                        closed: Arc::clone(&core.closed),
                        deadline,
                    };
                    if let Some(v) = self.park_on_fd(h, args, target, "read", span)? {
                        return Ok(v); // parked (sentinel; `poll_park` gates the push), or refused
                    }
                    // No netpoller-park: inside a native callback on M:N (`native_reentry > 0`, the
                    // Rust-stack `map`/sort loop can't snapshot-park) → DEMOTE + backoff-poll the
                    // non-blocking read in place (#3 socket half); top-level `main` on the default
                    // engine blocks in place too (Go-identical). Anywhere else the calling thread is
                    // shared, so blocking it starves the peer that would make the fd ready, and
                    // `park_on_fd` already returned the loud refusal (a `Refuse` cell of the `Socket`
                    // row of `block::mode`).
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
                                        d @ Decoded::Text(_) | d @ Decoded::Fail(_) => Some(d),
                                    },
                                };
                                match decided {
                                    Some(d) => Ok(d),
                                    None => match std::io::Read::read(stream, &mut b) {
                                        Ok(got) => {
                                            Ok(Vm::decode_carry(&mut carry, &b[..got], got == 0))
                                        }
                                        Err(e) => Err(e),
                                    },
                                }
                            };
                            match r {
                                // Same decode guard as the fast path; a NeedMore (no complete
                                // codepoint yet) just re-polls, like a would-block.
                                Ok(d) => match vm.decoded_value(d) {
                                    Some(v) => SockPoll::Ready(Ok(v)),
                                    None => {
                                        // N3(a) — took a partial off the fd: latch it so the demote
                                        // loop's timeout branch (sched.rs) reports `incomplete
                                        // utf-8` rather than `timeout`.
                                        vm.poll_partial = Some(
                                            core.carry
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .len(),
                                        );
                                        SockPoll::WouldBlock
                                    }
                                },
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    SockPoll::WouldBlock
                                }
                                Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                            }
                        },
                    );
                }
                Err((e, _)) => return Ok(self.sock_err(format!("{e}"))),
            }
        }
    }

    /// D6/R1 — `Socket.write(s) -> Result[int]` / `write_bytes(b) -> Result[int]` (+ optional
    /// `timeout_ms`). One write path — only the byte extraction differs. On a would-block the fiber
    /// PARKS on writability (M:N) or DEMOTE-polls it in-callback; off the M:N engine it fails loud.
    ///
    /// N2 — `timeout_ms` is LATCHED on the fiber ([`Vm::poll_deadline`]) exactly like `read`: a park
    /// rewinds `ip` and re-executes the op, so an un-latched `now + timeout_ms` would re-arm on every
    /// re-park and never expire. Extracted from `socket_method` so the `drop_poll_latch` clear on
    /// completion has ONE seam catching every early return (closed socket, poll-once, would-block).
    ///
    /// W15-3 — WRITE-ALL, like Go's `Conn.Write`: `Ok(len)` only once every byte is sent. A full
    /// buffer parks (or demote-polls) and resumes at the offset latched in [`Vm::poll_written`];
    /// any early stop — closed socket, deadline, poll-once `write(s, 0)`, OS error — is `Err`, and
    /// the bytes already sent are not reported.
    fn socket_write(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // The optional trailing int bounds writability.
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
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
        // N2 — the per-call deadline, latched on the fiber so it survives a park's ip-rewind re-run
        // (identical discipline to `socket_read`).
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        // W15-3 — the bytes already sent, latched on the fiber so a park's ip-rewind re-run resumes
        // at that offset instead of resending from byte 0.
        let mut sent = self.poll_written.unwrap_or(0).min(data.len());
        let core = self.socket_core(h);
        let attempt = {
            let mut guard = core.stream.lock().unwrap();
            let Some(stream) = guard.as_mut() else {
                return Ok(self.sock_err("write on a closed socket"));
            };
            match write_from(stream, &data, &mut sent) {
                Ok(()) => Ok(()),
                Err(e) => Err((e, stream.as_raw_fd())),
            }
        };
        match attempt {
            Ok(()) => Ok(self.sock_ok(Value::int(data.len() as i64))),
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                self.poll_written = Some(sent);
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Write,
                    in_flight: Arc::clone(&core.in_flight),
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "write", span)? {
                    return Ok(v);
                }
                // In-callback on M:N (or top-level `main` on the default engine) → demote +
                // backoff-poll the non-blocking write in place (#3 socket half).
                let mut sent = sent;
                self.demote_block_socket(fd, poller::Interest::Write, deadline, span, move |vm| {
                    let r = {
                        let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(stream) = guard.as_mut() else {
                            return SockPoll::Ready(Ok(vm.sock_err("write on a closed socket")));
                        };
                        write_from(stream, &data, &mut sent)
                    };
                    match r {
                        Ok(()) => SockPoll::Ready(Ok(vm.sock_ok(Value::int(data.len() as i64)))),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
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
        let Some(which) = SocketMethod::from_name(method) else {
            return Err(self.err(format!("type Socket has no method '{method}'"), span));
        };
        match which {
            SocketMethod::Read => {
                let r = self.socket_read(h, args, span);
                // B1 — the read's absolute deadline is LATCHED on the fiber (`poll_deadline`) so a park
                // + ip-rewind re-execution keeps the ORIGINAL `timeout_ms` budget instead of restarting
                // it. This logical `read` is over unless it parked (`poll_park` set ⇒ the very same call
                // resumes) — so drop the latch here; the next `read` gets a fresh budget.
                self.drop_poll_latch();
                r
            }
            SocketMethod::ReadBytes => {
                let r = self.socket_read_bytes(h, args, span);
                // Same latch discipline as `read`: drop the deadline unless the op PARKED (the very
                // same call resumes) — a re-park must not re-arm the timeout budget.
                self.drop_poll_latch();
                r
            }
            // `write(s[, timeout_ms])` (str) and R1's `write_bytes(b[, timeout_ms])` (raw `bytes`)
            // are the SAME write path — only the byte-extraction differs.
            SocketMethod::Write | SocketMethod::WriteBytes => {
                let r = self.socket_write(h, method, args, span);
                // N2 — `write` now latches its deadline on the fiber like `read` (a re-park must not
                // re-arm the budget), so it MUST drop the latch on completion too.
                self.drop_poll_latch();
                r
            }
            SocketMethod::Close => {
                self.arity_err("close", args, 0, span)?;
                let core = self.socket_core(h);
                // W15-1 — take the stream out first so a fiber `register`ing concurrently (already past
                // its `WouldBlock` but not yet under the registry lock) sees `closed` before it can arm
                // the fd; only then deregister a pending park, and drop the stream (closing the fd) LAST
                // so `deregister`'s epoll `delete` never races the close. Must not hold `stream`'s guard
                // across `deregister`: it takes the sched lock via `complete_offload`.
                let stream = core.stream.lock().unwrap().take();
                core.closed.store(true, Ordering::Release);
                poller::deregister(core.key);
                drop(stream);
                Ok(Value::nil())
            }
        }
    }

    /// D6/D6c — `Listener.accept() -> Result[Socket]` (+ optional `timeout_ms`). Parks on the listen
    /// fd's readability (a pending connection) under the M:N engine, like `Socket::read`; demotes
    /// in-callback; fails loud off the M:N engine.
    ///
    /// N2 — `timeout_ms` is LATCHED on the fiber ([`Vm::poll_deadline`]) like `read`/`write`: a park
    /// re-executes the op, so an un-latched deadline would re-arm on every re-park. Extracted from
    /// `listener_method` so the `drop_poll_latch` clear has ONE seam over every early return.
    fn listener_accept(
        &mut self,
        h: GcRef,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // `accept()` or `accept(timeout_ms)` — the optional trailing int bounds how long to wait for
        // an inbound connection (D6c). Mirrors `Socket::read`'s timeout handling.
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.first(), span)?;
        // N2 — the per-call deadline, latched on the fiber so it survives a park's ip-rewind re-run.
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
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
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "accept", span)? {
                    return Ok(v);
                }
                // In-callback on M:N (or top-level `main` on the default engine) → demote +
                // backoff-poll the non-blocking accept in place (#3 socket half).
                self.demote_block_socket(fd, poller::Interest::Read, deadline, span, move |vm| {
                    let r = {
                        let guard = core.listener.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(listener) = guard.as_ref() else {
                            return SockPoll::Ready(Ok(vm.sock_err("accept on a closed listener")));
                        };
                        listener.accept()
                    };
                    match r {
                        Ok((stream, _peer)) => SockPoll::Ready(Ok(vm.accept_socket_value(stream))),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
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
        let Some(which) = ListenerMethod::from_name(method) else {
            return Err(self.err(format!("type Listener has no method '{method}'"), span));
        };
        match which {
            ListenerMethod::Accept => {
                let r = self.listener_accept(h, args, span);
                // N2 — `accept` latches its deadline on the fiber like `read`, so it MUST drop the
                // latch on completion too (a re-park must not re-arm the budget).
                self.drop_poll_latch();
                r
            }
            ListenerMethod::Addr => {
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
            ListenerMethod::Close => {
                self.arity_err("close", args, 0, span)?;
                let core = self.listener_core(h);
                // W15-1 — same order as `Socket.close`: see its comment.
                let listener = core.listener.lock().unwrap().take();
                core.closed.store(true, Ordering::Release);
                poller::deregister(core.key);
                drop(listener);
                Ok(Value::nil())
            }
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
            closed: core::new_closed(),
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// W7-18 — the resume half of the socket-park deadline story, shared by every op that can be
    /// re-injected by the netpoller (`read`, `read_bytes`, `write`, `accept`). Returns `true` when THIS
    /// op's own D6c `timeout_ms` is what fired, i.e. when the caller should surface its catchable
    /// `Err("timeout")` exactly as before.
    ///
    /// The two wake causes are told apart by the CLOCK, not by a second marker: `park_on_fd` clamped
    /// the park to `min(op deadline, run deadline)`, both are absolute `Instant`s, and `self.deadline`
    /// is `Some` only under `chezzi test --timeout` — so `now >= self.deadline` at resume is true iff
    /// the RUN deadline is what expired. A run-deadline expiry must be a hard, `recover:`-proof halt;
    /// an op-timeout expiry must stay an ordinary catchable value.
    ///
    /// Two orderings here are load-bearing and both were got wrong in the obvious version:
    ///
    /// 1. **Consume `poll_timed_out` FIRST, unconditionally.** Halting with `?` while the flag is still
    ///    set leaves it for the unwind: the first socket op inside any `defer` then consumes it and
    ///    reports a fabricated `Err("timeout")` for an op that was never given a `timeout_ms`, so the
    ///    cleanup silently does nothing. That is the "stopped promptly ≠ cleaned up" failure W7-16 was
    ///    caught on, and no existing fence sees it (their `defer`s only `print`).
    /// 2. **Inside a `defer`, a run-deadline wake is NOT this op's timeout** — hence `Ok(false)`, which
    ///    retries the syscall so a cleanup write that can complete immediately still does (W7-17's
    ///    `deferring > 0` suppression, same term `halt_requested` uses). If the retry would block,
    ///    `park_on_fd`'s ungated check halts it there — which is what makes that check load-bearing.
    ///
    /// Accepted degeneracy: with an op deadline SHORTER than the run deadline by less than the
    /// poller-inject-to-schedule latency, the op's honest timeout fires but the fiber is not scheduled
    /// until the run deadline has also passed, converting a catchable `Err("timeout")` into a hard
    /// halt. That window is scheduling latency — normally sub-millisecond, but it widens with worker
    /// contention (`--threads=1` plus a CPU-bound sibling makes it reachable), so it is bounded by
    /// load rather than by a constant. Accepted, not fenced, because it is correct by consequence: a
    /// fiber that resumes past the run deadline hard-halts at its very next checkpoint regardless, so
    /// the only thing lost is which of two aborts is reported, and a test that pinned it would pass
    /// either way.
    fn poll_timeout_check(&mut self, span: Span) -> Result<bool, RuntimeError> {
        let fired = std::mem::take(&mut self.poll_timed_out);
        match self.deadline_halt(span) {
            Ok(()) => Ok(fired),
            Err(e) if self.deferring == 0 => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// D6 — the would-wait decision shared by every would-block socket op (`op` names it). Returns
    /// `Ok(Some(v))` when the op settles here: `v` is the parked sentinel (the fiber was parked on
    /// the netpoller) or, in a `Refuse` cell of the `Socket` row of `block::mode`, the op's
    /// `would block` `Err` value. Returns `Ok(None)` when the caller must block in place: on the two
    /// contexts that own their whole thread (an M:N in-callback demote, and top-level `main` on the
    /// default engine — Go-identical, and what makes the hello-world TCP server writable) it falls
    /// through to [`Vm::demote_block_socket`] and BLOCKS there, bounded only by the op's `timeout_ms`,
    /// the run's `--timeout`, or a cancel — and on top-level `main` under `chezzi run` **only the first
    /// of those three exists**: `--timeout` is a `chezzi test` flag (`chezzi run` rejects it as an
    /// unknown flag) and `main` has no scope cancel to trip, so an untimed op there blocks until SIGINT
    /// (see [`Vm::demote_block_socket`]'s doc); everywhere else — an eager `Executor` job, a
    /// callback on a non-M:N thread — it keeps the loud `Err("<op> would block: an Executor job
    /// doesn't own its thread …")`, because blocking a SHARED thread starves the very peer that
    /// would make the fd ready (both shapes measured as hangs; see that helper's doc).
    /// `Err` only for a **concurrent op on a shared socket**: oneshot epoll allows ONE registration per
    /// fd, so a second fiber reaching a would-block op while the first is parked (`in_flight` already
    /// set) faults cleanly rather than corrupting the poller registry (review: Critical). On the park
    /// path it restores the pre-call operand stack (receiver THEN args — the exact layout `CallMethod`
    /// re-pops; unlike a 0-arg `recv` park, `read(n)`/`write(s)` must re-push their args), rewinds `ip`
    /// so the op re-executes on resume, and sets the `poll_park` sentinel for the worker loop.
    ///
    /// D6c — `target.deadline` (the optional `timeout_ms`) is honored on this snapshot-park path (the
    /// netpoller wakes the fiber on readiness OR at the deadline) AND, since the deadline was threaded
    /// into [`Vm::demote_block_socket`], on the in-callback demote path too (`native_reentry > 0`, where
    /// this returns `Ok(false)` — the demote loop caps its kernel wait by the remaining budget and
    /// expires with the same timeout `Err`). Every socket op latches its deadline on the fiber the same
    /// way (`Vm::poll_deadline`, N2), so a re-park does not re-arm the budget.
    ///
    /// W7-18 — the park also observes the RUN's `--timeout` deadline, in two halves that are one fix:
    /// the halt below (a fiber must not park PAST a deadline that has already passed) and the clamp
    /// further down (a park already under way wakes at the sooner of the two deadlines). There is
    /// deliberately no [`deadline_gap_wake`] analogue here, and that is worth stating because W7-17
    /// needed one three functions away: there `timer::submit_at` armed a *job* before `MnSched::park`
    /// had filled the fiber's bucket, so an early fire found an empty bucket and was LOST. Here the
    /// deadline is not a job but a FIELD IN THE REGISTRY ROW, and [`poller::register`] inserts the row
    /// and the fiber together under the registry lock — the wake is re-derived by re-reading the
    /// registry (`next_timeout` / `fire_due_socket_timeouts`), so no fire can precede the park. An
    /// already-expired row just makes `next_timeout` return `ZERO`, and `register`'s `notify()` covers
    /// the insert-after-`next_timeout`-was-read window.
    pub(super) fn park_on_fd(
        &mut self,
        h: GcRef,
        args: &[Value],
        target: PollPark,
        op: &str,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        // W7-18 — `--timeout` ABOVE the cancellation checkpoint, mirroring W7-17's ordering in
        // `chan_recv_step`: the deadline outranks a cancel, so a fiber reaching here after the run
        // deadline reports the honest hard halt rather than `cancelled`. UNGATED by `deferring`
        // (unlike `poll_timeout_check`'s entry check): everything above a park settles without
        // blocking, and a `defer` that would PARK past the deadline is a hang, not cleanup.
        self.deadline_halt(span)?;
        // TICKET-194 — a refused op never waits, so it is not a cancellation point (owner decision
        // 1): `Refuse` is decided BEFORE `wait_halt`, or a pending cancel cuts an op that returns its
        // `would block` `Err` at once.
        let mode = self.block_mode(WaitSpec::Socket);
        if mode == BlockMode::Refuse {
            return Ok(Some(self.sock_err(sock_would_block_msg(op))));
        }
        // CANCELLATION CHECKPOINT — a socket op that would wait is a cancel-delivery point
        // (the single choke point for `accept`/`read`/`write`): the check sits OUTSIDE the
        // `mn.is_some()` gate, because top-level `main` (and any other non-worker-shell context) runs
        // the op as a BLOCKING syscall below and would otherwise have no cancel-delivery point at a
        // socket at all. On M:N a cancelled fiber must also not RE-park: `poller::drain_family`
        // re-injects a poller-parked fiber on cancel and the rewound op re-runs here — without this
        // check it would would-block and re-park forever (the every-instruction check that used to
        // kill it at the dispatch loop top is gone; see `run_until`), wedging the nursery.
        self.wait_halt(span)?;
        if mode == BlockMode::Park {
            // The `in_flight` guard: at most one op may be parked on a socket at a time. A second
            // concurrent op on a shared socket (`Arc`) faults rather than overwrite the registry entry
            // (which would drop the first fiber + leak `inflight`) or double-`add` the fd (EEXIST panic).
            if target.in_flight.swap(true, Ordering::AcqRel) {
                return Err(self.err(
                    "concurrent operation on a shared socket is not supported".into(),
                    span,
                ));
            }
            self.push(Value::obj(h)); // receiver (deeper on the stack)
            for &a in args {
                self.push(a); // its args, in order, back on top
            }
            self.rewind_op();
            // W7-18 — wake at the SOONER of the op's own D6c budget and the run's `--timeout`
            // deadline, so a park with no `timeout_ms` at all (`deadline: None` — the shape that
            // HUNG: a nursery `l.accept()` nobody connects to) is still reached by the wall clock.
            // `poll_timeout_check` then tells the two causes apart at resume by re-reading the clock,
            // which is why no second marker beside `poll_timed_out` is needed.
            //
            // Clamp `target.deadline` ONLY — never `self.poll_deadline` (the per-op budget latch, N2,
            // which survives an ip-rewind re-park): `demote_block_socket` reads that latch as the op's
            // own budget and expiring it yields a CATCHABLE `Err("timeout")`, so folding the run
            // deadline into it would report a hard `--timeout` abort as an ordinary socket timeout.
            let target = PollPark {
                deadline: match (target.deadline, self.deadline) {
                    (Some(op), Some(run)) => Some(op.min(run)),
                    (op, run) => op.or(run),
                },
                ..target
            };
            self.poll_park = Some(target);
            Ok(Some(Value::nil())) // parked sentinel; `poll_park` gates the result-push at `do_call`
        } else {
            Ok(None)
        }
    }
}
