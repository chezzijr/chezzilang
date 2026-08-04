// vm::netio — split out of vm/mod.rs. `super::*` == the `vm` module.
// Channels, Shared/RwShared/Atomic, sockets/listeners, netpoller parks.

use super::*;

/// Generate a `pub(super) fn <name>(&self, GcRef) -> Arc<CoreType>` that clones out the shared
/// `Arc` behind a handle of the given `Obj` variant (refcount bump). See [`Vm::channel_core`] for
/// the rationale (the `Arc` is held only for the calling method, so it does not borrow the heap);
/// `channel_core`/`socket_core` stay hand-written to carry that doc.
macro_rules! core_accessor {
    ($name:ident, $variant:ident, $core:ty) => {
        pub(super) fn $name(&self, h: GcRef) -> Arc<$core> {
            match self.heap.get(h) {
                Obj::$variant(core) => Arc::clone(core),
                _ => unreachable!(concat!(stringify!($name), " on non-", stringify!($variant))),
            }
        }
    };
}

/// The shared fault for a `send` on a FULL bounded channel that cannot park (top level with no
/// nursery, or inside a native callback). ONE const so every non-parkable full-send path — on BOTH
/// engines — emits byte-identical text (parity). Mirrors `chan_recv_step`'s empty-recv deadlock note.
const FULL_SEND_DEADLOCK: &str = "send on a full channel: deadlock — the bounded channel is at \
    capacity and no runnable task can receive to free a slot. If a consumer is spawned in the same \
    `parallel:` nursery, note the nursery body runs before its spawned tasks start, so a blocking \
    send in the body can't reach it — put the producer in a `spawn:` too.";

/// The shared fault for a `send` to a CLOSED channel. ONE const for the same reason
/// [`FULL_SEND_DEADLOCK`] is one: the top-of-`send` guard, the `wait:` send arm and the eager
/// blocked-sender loop must all emit byte-identical text. Go panics `send on closed channel` here.
const CLOSED_SEND: &str = "send on a closed channel";

/// The shared fault for a `recv` on a CLOSED-and-drained channel — the twin of [`CLOSED_SEND`], and
/// one const for the same byte-identical-text reason (it is raised from both the demote arm and the
/// ordinary `chan_recv_step` arm of the same `match`).
const CLOSED_RECV: &str = "receive on a closed channel";

/// The shared fault for a `recv` on an EMPTY channel that cannot park. Raised both by the plain
/// no-scheduler arm of [`Vm::chan_recv_step`] and — since W7-12 — by an eager `Executor` job that
/// blocked and then found its own `shutdown()` waiting on it: ONE const so the two spellings of the
/// same verdict stay byte-identical (the M:N text must match `--serial`'s inline drain).
const EMPTY_RECV_DEADLOCK: &str = "recv on an empty channel: deadlock — no runnable task can send. \
    If a producer is spawned in the same `parallel:` nursery, note the nursery body runs before its \
    spawned tasks start, so a blocking recv / `for v in ch:` in the body can't reach it — put the \
    consumer in a `spawn:` too.";

/// The `wait:` sibling of [`EMPTY_RECV_DEADLOCK`] — every arm empty and nobody left to send.
const EMPTY_WAIT_DEADLOCK: &str = "wait on channels that are all empty: deadlock — no runnable task \
    can send. If a producer is spawned in the same `parallel:` nursery, note the nursery body runs \
    before its spawned tasks start, so a blocking `wait:` in the body can't reach it — put the \
    `wait:` in a `spawn:` too.";

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

    core_accessor!(shared_core, Shared, SharedCore);
    core_accessor!(rwshared_core, RwShared, RwSharedCore);
    core_accessor!(atomic_core, Atomic, AtomicCore);
    core_accessor!(atomic_int_core, AtomicInt, AtomicIntCore);

    /// `AtomicInt(v)` — pop the int init, wrap it in a fresh lock-free `Arc<AtomicIntCore>`. The checker
    /// guarantees the single arg is an int; a boxed BigInt is narrowed via `int_of`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic_int(&mut self, _span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        let n = self.int_of(init);
        Ok(Value::obj(self.heap.alloc(Obj::AtomicInt(Arc::new(
            AtomicIntCore {
                v: std::sync::atomic::AtomicI64::new(n),
            },
        )))))
    }

    /// `Atomic(v)` — pop the init, box its wire form behind a fresh `Arc<AtomicCore>`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        // A non-sendable init (a frame-holding generator / module/native/FFI handle) faults gracefully
        // with the `NewAtomic` span — the box is a shared cross-thread cell.
        let init = self.to_wire_crossable(init, span)?;
        Ok(Value::obj(self.heap.alloc(Obj::Atomic(Arc::new(
            // The summary starts `WS_UNKNOWN` (like every other core constructor): the first GC
            // pass walks the initial payload once and memoizes it.
            AtomicCore {
                v: Mutex::new(init),
                ..Default::default()
            },
        )))))
    }

    /// `timer(ms)` — pop the `ms` int, push a fresh `Channel[bool]` stamped with `now + ms`. Delivery is
    /// handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at the top level
    /// can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the `Instant`/`Duration` math
    /// stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_timer(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let v = self.pop();
        let ms = if let Some(ms) = self.int_val(v) {
            ms.max(0) as u64
        } else {
            return Err(self.err(
                format!("timer(ms) expects int, got {}", self.type_name(v)),
                span,
            ));
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
        Ok(Value::obj(self.heap.alloc(Obj::Channel(core))))
    }

    core_accessor!(executor_core, Executor, ExecutorCore);

    /// D6 — clone out the shared `Arc<SocketCore>`/`Arc<ListenerCore>` behind a handle (refcount bump),
    /// mirroring [`channel_core`](Vm::channel_core). The `Arc` is held only for the calling method, so
    /// locking the fd does not borrow the heap.
    pub(super) fn socket_core(&self, h: GcRef) -> Arc<SocketCore> {
        match self.heap.get(h) {
            Obj::Socket(core) => Arc::clone(core),
            _ => unreachable!("socket_core on non-socket"),
        }
    }

    core_accessor!(listener_core, Listener, ListenerCore);

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
                        Ok(Value::nil()) // parked sentinel; `poll_park` gates the result-push at `do_call`
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
                    deadline,
                };
                if self.park_on_fd(h, args, target, span)? {
                    return Ok(Value::nil()); // parked (sentinel)
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
                                    let bv = Value::obj(
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
                        deadline,
                    };
                    if self.park_on_fd(h, args, target, span)? {
                        return Ok(Value::nil()); // parked (sentinel; `poll_park` gates the push)
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
                    return Ok(self.sock_err(
                        "read would block: std.net sockets require the --parallel engine",
                    ));
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
    fn socket_write(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // The optional trailing int bounds writability.
        self.arity_range_err(method, args, 1, 2, span)?;
        if self.poll_timed_out {
            self.poll_timed_out = false;
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
            Ok(got) => Ok(self.sock_ok(Value::int(got as i64))),
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Write,
                    in_flight: Arc::clone(&core.in_flight),
                    deadline,
                };
                if self.park_on_fd(h, args, target, span)? {
                    return Ok(Value::nil());
                }
                // In-callback on M:N → demote + backoff-poll the non-blocking write (#3 socket half).
                if self.mn.is_some() && self.native_reentry > 0 {
                    let core = Arc::clone(&core);
                    return self.demote_block_socket(
                        fd,
                        poller::Interest::Write,
                        deadline,
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
                                Ok(got) => SockPoll::Ready(Ok(vm.sock_ok(Value::int(got as i64)))),
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    SockPoll::WouldBlock
                                }
                                Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                            }
                        },
                    );
                }
                Ok(self
                    .sock_err("write would block: std.net sockets require the --parallel engine"))
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
        match method {
            "read" => {
                let r = self.socket_read(h, args, span);
                // B1 — the read's absolute deadline is LATCHED on the fiber (`poll_deadline`) so a park
                // + ip-rewind re-execution keeps the ORIGINAL `timeout_ms` budget instead of restarting
                // it. This logical `read` is over unless it parked (`poll_park` set ⇒ the very same call
                // resumes) — so drop the latch here; the next `read` gets a fresh budget.
                self.drop_poll_latch();
                r
            }
            "read_bytes" => {
                let r = self.socket_read_bytes(h, args, span);
                // Same latch discipline as `read`: drop the deadline unless the op PARKED (the very
                // same call resumes) — a re-park must not re-arm the timeout budget.
                self.drop_poll_latch();
                r
            }
            // `write(s[, timeout_ms])` (str) and R1's `write_bytes(b[, timeout_ms])` (raw `bytes`)
            // are the SAME write path — only the byte-extraction differs.
            "write" | "write_bytes" => {
                let r = self.socket_write(h, method, args, span);
                // N2 — `write` now latches its deadline on the fiber like `read` (a re-park must not
                // re-arm the budget), so it MUST drop the latch on completion too.
                self.drop_poll_latch();
                r
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.socket_core(h);
                // Disarm any pending poller registration (a `close` racing a park) before the fd drops;
                // a no-op in the common case (the owning fiber is running, not parked).
                poller::deregister(core.key);
                *core.stream.lock().unwrap() = None;
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Socket has no method '{method}'"), span)),
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
        self.arity_range_err("accept", args, 0, 1, span)?;
        if self.poll_timed_out {
            self.poll_timed_out = false;
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
                    deadline,
                };
                if self.park_on_fd(h, args, target, span)? {
                    return Ok(Value::nil());
                }
                // In-callback on M:N → demote + backoff-poll the non-blocking accept (#3 socket half).
                if self.mn.is_some() && self.native_reentry > 0 {
                    let core = Arc::clone(&core);
                    return self.demote_block_socket(
                        fd,
                        poller::Interest::Read,
                        deadline,
                        span,
                        move |vm| {
                            let r = {
                                let guard = core.listener.lock().unwrap_or_else(|e| e.into_inner());
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
                Ok(self
                    .sock_err("accept would block: std.net sockets require the --parallel engine"))
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
        match method {
            "accept" => {
                let r = self.listener_accept(h, args, span);
                // N2 — `accept` latches its deadline on the fiber like `read`, so it MUST drop the
                // latch on completion too (a re-park must not re-arm the budget).
                self.drop_poll_latch();
                r
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
                Ok(Value::nil())
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
        let v = Value::obj(self.heap.alloc(Obj::Socket(core)));
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
    /// D6c — `target.deadline` (the optional `timeout_ms`) is honored on this snapshot-park path (the
    /// netpoller wakes the fiber on readiness OR at the deadline) AND, since the deadline was threaded
    /// into [`Vm::demote_block_socket`], on the in-callback demote path too (`native_reentry > 0`, where
    /// this returns `Ok(false)` — the demote loop caps its kernel wait by the remaining budget and
    /// expires with the same timeout `Err`). Every socket op latches its deadline on the fiber the same
    /// way (`Vm::poll_deadline`, N2), so a re-park does not re-arm the budget.
    pub(super) fn park_on_fd(
        &mut self,
        h: GcRef,
        args: &[Value],
        target: PollPark,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        // CANCELLATION CHECKPOINT — a socket op is a blocking op, so it is a cancel-delivery point
        // (the single choke point for `accept`/`read`/`write`/`connect`), on BOTH engines: the check
        // sits OUTSIDE the `mn.is_some()` gate, because serial runs the op as a BLOCKING syscall below
        // and would otherwise have no cancel-delivery point at a socket at all. On M:N a cancelled
        // fiber must also not RE-park: `poller::drain_sched` re-injects a poller-parked fiber on
        // cancel and the rewound op re-runs here — without this check it would would-block and re-park
        // forever (the every-instruction check that used to kill it at the dispatch loop top is gone;
        // see `run_until`), wedging the nursery.
        if self.native_reentry == 0 && self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        if self.mn.is_some() && self.native_reentry == 0 {
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
                // non-sendable value (a frame-holding generator, or a module/native/FFI handle) faults
                // gracefully with `send`'s span — the value crosses a heap boundary into the receiver.
                let w = self.to_wire_crossable(args[0], span)?;
                // Closed-channel guard: a `send` after `close()` faults (Go-panic analog). A `close`
                // racing in the window between this check and the enqueue is benign — the value is
                // still buffered and drained before the close is observed (drain-before-close), exactly
                // like Go's racy `select`/close. Strict mutual exclusion isn't required.
                if self.channel_core(h).q.lock().unwrap().closed {
                    return Err(self.err(CLOSED_SEND.to_string(), span));
                }
                // Unbounded: enqueue immediately (byte-identical to the pre-bounded path). Bounded +
                // full: park the fiber (`SendStep::Parked` — the receiver+value were re-rooted) or, in
                // a non-parkable context, fault. On park, `do_method_call` skips the result push.
                match self.chan_send_step(h, w, args[0], span)? {
                    SendStep::Sent | SendStep::Parked => Ok(Value::nil()),
                }
            }
            // `try_send` is the non-blocking partner of `send`: it never parks. It returns `false` when
            // the send can't proceed — the channel is CLOSED, or a BOUNDED channel is FULL (queue at
            // capacity) — and `true` once the value is queued. (An unbounded channel is never full.)
            // NOTE: the full-vs-not decision on a bounded channel under multi-sender contention is the
            // SAME nondeterminism class as `try_recv` returning `None`-vs-`Some` under contention.
            "try_send" => {
                self.arity_err("try_send", args, 1, span)?;
                let w = self.to_wire_crossable(args[0], span)?;
                let core = self.channel_core(h);
                let closed = core.q.lock().unwrap().closed;
                if closed {
                    return Ok(Value::bool(false));
                }
                match core.cap {
                    // Unbounded: never full — enqueue immediately (byte-identical to `send`).
                    None => {
                        self.channel_send_wire(h, w);
                        Ok(Value::bool(true))
                    }
                    // Bounded: the space-check + enqueue MUST be atomic (same path as blocking `send`),
                    // or two concurrent `try_send`s both see space and over-fill past `cap`. Returns
                    // `false` when full — non-blocking, so decline (never parks).
                    Some(cap) => Ok(Value::bool(self.enqueue_bounded(h, &core, cap, w))),
                }
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
                        RecvStep::Got(w) => {
                            self.wake_senders(h); // freed a slot — wake a parked bounded sender
                            Ok(self.from_wire(w))
                        }
                        RecvStep::ClosedEmpty => Err(self.err(CLOSED_RECV.to_string(), span)),
                        // demote never parks (it blocks in place); a Parked here is impossible.
                        RecvStep::Parked => unreachable!("demote_recv_block never parks"),
                    };
                }
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => {
                        self.wake_senders(h); // freed a slot — wake a parked bounded sender
                        Ok(self.from_wire(w))
                    }
                    // `chan_recv_step` already re-rooted the receiver + set `suspend`; the sentinel is
                    // never observed (`do_method_call` gates the result-push on `suspend`).
                    RecvStep::Parked => Ok(Value::nil()),
                    // Closed-and-drained: a distinct fault (not the deadlock fault) — no producer left.
                    RecvStep::ClosedEmpty => Err(self.err(CLOSED_RECV.to_string(), span)),
                }
            }
            "try_recv" => {
                // A1: non-blocking poll. Unlike `recv` it never touches `scheduler_stack` /
                // `native_reentry` / `suspend` / `ip` — it always returns immediately with an
                // `Option`: `Some(v)` if queued, `None` if empty. Mirrors `interp::eval_channel_method`.
                self.arity_err("try_recv", args, 0, span)?;
                let core = self.channel_core(h);
                let popped = core.q.lock().unwrap().pop();
                if popped.is_some() {
                    self.wake_senders(h); // a real pop freed a slot — wake a parked bounded sender
                }
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
                Ok(Value::nil())
            }
            // `trip()` flips the manual level-trigger latch (the primitive behind `std.cancel`'s
            // `done()`): the channel is then permanently ready (`recv`/`try_recv`/`wait` yield `true`).
            // Idempotent. Reuses `close()`'s exact wake fan-out so a parked `recv`/`wait` re-runs and
            // observes the latch — but does NOT set `closed` (a closed+empty `wait` arm is *skipped*;
            // we need it *ready*).
            "trip" => {
                self.arity_err("trip", args, 0, span)?;
                let core = self.channel_core(h);
                // W7-13r(b) — the store happens UNDER `core.q`, the same lock a blocked waiter
                // re-checks its readiness predicate under, and that is what makes the wake reliable.
                // `close()` has always set `closed` under this lock; `trip()` used a bare atomic, so a
                // waiter could evaluate "not tripped" while holding `q` and be notified before it had
                // atomically released `q` and enqueued on `cv` — a lost wakeup costing a full
                // `DEMOTE_POLL_BACKOFF`, the exact shape W7-13 fixed for values and closes. Holding
                // `q` across the store makes the two orderings the only possibilities: the waiter sees
                // the latch in its predicate, or it is already on the condvar when `notify_all` runs.
                // The guard is dropped before the wake fan-out below, which takes the sched lock —
                // `q` is never held across that.
                {
                    let _g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    core.done_latch.store(true, Ordering::Relaxed);
                }
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    core.cv.notify_all();
                    self.wake_on_send(h);
                }
                Ok(Value::nil())
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let n = self.channel_core(h).q.lock().unwrap().len();
                Ok(Value::int(n as i64))
            }
            // `cap()` reports the channel's capacity: `Channel[T](n)` → `n`; unbounded `Channel[T]()` → 0.
            "cap" => {
                self.arity_err("cap", args, 0, span)?;
                let cap = self.channel_core(h).cap.unwrap_or(0);
                Ok(Value::int(cap as i64))
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
            // W6-7/W6-10 — summarise OFF-LOCK (it is O(payload); see `ChanState::push`).
            let sum = crate::vm::core::wire_summary(&w);
            core.q.lock().unwrap().push(sum, w);
            core.cv.notify_all();
            self.wake_on_send(h);
        }
    }

    /// One `send` step for [`Vm::channel_method`]'s `send` (after its closed-channel guard). Unbounded
    /// channels enqueue immediately (byte-identical to the historical path). A bounded channel enqueues
    /// while it has space, else BLOCKS: it parks the fiber (an active scheduler resumes it once a
    /// sibling `recv` frees a slot — the send-side mirror of [`Vm::chan_recv_step`]) or, in a context
    /// that cannot park (top level with no nursery, or inside a native callback where the host stack
    /// can't be unwound), faults with the shared full-deadlock message. The space-check + enqueue is
    /// kept atomic under the sched lock (M:N: [`MnSched::send_wake_bounded`]) so concurrent senders
    /// can't over-fill; the cooperative engine is single-thread, so a bare core-lock check suffices.
    pub(super) fn chan_send_step(
        &mut self,
        h: GcRef,
        w: WireValue,
        orig: Value,
        span: Span,
    ) -> Result<SendStep, RuntimeError> {
        let core = self.channel_core(h);
        let Some(cap) = core.cap else {
            // Unbounded — never blocks. Byte-identical to the historical `send`.
            self.channel_send_wire(h, w);
            return Ok(SendStep::Sent);
        };
        // Bounded. A fiber woken by `cancel_drain` (its scope faulted) must fault here rather than
        // re-park (mirrors `chan_recv_step`'s checkpoint). `native_reentry == 0` gates it exactly as
        // the park gate does — inside a callback the host stack can't be unwound.
        if self.native_reentry == 0 && self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // EAGER `Executor` job: block until a receiver frees a slot, the mirror of the empty-`recv`
        // case in `chan_recv_step` ([`Vm::eager_wait_tick`]). Handled BEFORE the shared attempt below
        // because a retry needs the wire value again and the shared path moves it — the `clone` is
        // per attempt and only on this path, so an ordinary bounded `send` is untouched. Retries the
        // ONE atomic `enqueue_bounded` rather than check-then-enqueue, so a racing sender still can't
        // push either send past `cap`.
        if self.eager_core.is_some() {
            // First attempt OUTSIDE the block guard: `submit_result`'s cap-1 result channel always
            // has space, so every such job would otherwise bump `blocked` on its last instruction and
            // hand W7-12's predicate a free false positive.
            if self.enqueue_bounded(h, &core, cap, w.clone()) {
                return Ok(SendStep::Sent);
            }
            let _blocked = self.eager_block_guard();
            loop {
                // Ready == a slot freed up OR the channel closed. The retry below is still the one
                // atomic `enqueue_bounded`, so a racing sender that takes the slot first just
                // re-parks (W7-13).
                self.eager_wait_tick(&core, FULL_SEND_DEADLOCK, span, |g| {
                    g.len() < cap || g.closed
                })?;
                // W7-13r(c) — a `close()` while we are blocked means this send can NEVER complete, so
                // it must fault rather than wait for something else to notice. `enqueue_bounded` does
                // not consult `closed` and this loop never returns to the top-of-`send` guard, so
                // without this a blocked sender could not observe a close AT ALL: it reported
                // FULL_SEND_DEADLOCK — "no runnable task can receive" — about a channel that was
                // closed, and with no explicit `shutdown()` (so no W7-12 verdict) it hung outright.
                //
                // **Ordered AFTER the retry, and that order is load-bearing** — the reverse is a
                // regression, caught by adversarial review on the ordinary drain-then-close shape:
                //
                //     consumer:  a := ch.recv()   # frees the slot FOR the blocked sender
                //                ch.close()       # …then wins the race back to `core.q`
                //
                // Go completes that program (`sent both`) because its receive hands the value to a
                // waiting sender ATOMICALLY inside the recv — by the time `close` runs the send has
                // already happened. Chezzi's eager sender is retry-based, so it is only woken and must
                // re-take the slot; checking `closed` first let the close deterministically beat it
                // and faulted a send Go completes (measured 5/5 both ways). Retrying first restores
                // the handoff: a freed slot is taken, and `closed` is consulted only once the retry
                // has failed — which is also exactly the drain-before-close rule the top-of-`send`
                // guard documents at the head of this method.
                //
                // `closed` must also be acted on HERE rather than by the predicate alone: a predicate
                // that reports ready while `enqueue_bounded` keeps refusing would spin hot instead of
                // polling. It is in the predicate only to make the wake prompt.
                //
                // Fenced by `a_blocked_eager_send_still_completes_when_a_recv_frees_its_slot_before_
                // the_close` (the drain-then-close shape) and
                // `eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed` (the hang).
                // Swapping these two blocks fails the first and passes the second.
                if self.enqueue_bounded(h, &core, cap, w.clone()) {
                    self.eager_block_settled();
                    return Ok(SendStep::Sent);
                }
                if core.q.lock().unwrap_or_else(|e| e.into_inner()).closed {
                    self.eager_block_settled();
                    return Err(self.err(CLOSED_SEND.to_string(), span));
                }
            }
        }
        // Atomic space-check + enqueue + receiver-wake (shared with `try_send` — see
        // [`Vm::enqueue_bounded`]). On success the value is delivered; on `false` the channel was full
        // and we decide how to block.
        if self.enqueue_bounded(h, &core, cap, w) {
            return Ok(SendStep::Sent);
        }
        // FULL. Inside a native callback the caller's host-stack loop frame is not capturable, so we
        // cannot snapshot-park — fault for v1 (the `ponytail:` upgrade path is a demote-in-place send
        // block, like `demote_recv_block`).
        if self.native_reentry > 0 {
            return Err(self.err(FULL_SEND_DEADLOCK.to_string(), span));
        }
        // A real M:N WORKER snapshot-parks: the worker loop drives `send_suspend` → `Disp::SendPark`.
        if self.mn.is_some() {
            self.park_send(h, orig);
            return Ok(SendStep::Parked);
        }
        // `mn == None`. The INLINE outermost-`parallel:` builder (holds only `mn_enlist_sched`) has NO
        // worker loop to drive its `send_suspend` — parking there would leak it forever (`paused()`
        // stuck true → silent halt), so it must NOT park: fault (the inline-owner-never-parks
        // invariant, mirroring `chan_recv_step` gating its snapshot-park on `self.mn.is_some()` ONLY).
        // A cooperative nursery (no enlist sched, active `scheduler_stack`) DOES park via
        // `send_suspend` — `run_child` re-runs it on a sibling `recv`.
        if self.mn_enlist_sched.is_none() && !self.scheduler_stack.is_empty() {
            self.park_send(h, orig);
            return Ok(SendStep::Parked);
        }
        Err(self.err(FULL_SEND_DEADLOCK.to_string(), span))
    }

    /// Atomic space-check + enqueue + receiver-wake on a BOUNDED channel; returns whether the value
    /// was enqueued (`false` = full). Shared by the blocking `send` (parks on `false`) and `try_send`
    /// (declines on `false`) so BOTH route through the ONE atomic path — else a `try_send` that
    /// check-then-enqueues in two steps over-fills past `cap` when it races a concurrent sender. M:N /
    /// inline-enlist: atomic under the sched lock ([`MnSched::send_wake_bounded`], which re-checks
    /// space under the lock and wakes parked receivers). Cooperative / no-sched: single-thread, so a
    /// plain core-lock check is race-free.
    pub(super) fn enqueue_bounded(
        &mut self,
        h: GcRef,
        core: &Arc<ChannelCore>,
        cap: usize,
        w: WireValue,
    ) -> bool {
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
            let key = self.channel_core_ptr(h);
            return sched.send_wake_bounded(key, core, w, cap);
        }
        let sum = crate::vm::core::wire_summary(&w); // OFF-LOCK — see `ChanState::push`
        let enqueued = {
            let mut g = core.q.lock().unwrap();
            if g.len() < cap {
                g.push(sum, w);
                true
            } else {
                false
            }
        };
        if enqueued {
            core.cv.notify_all();
            self.wake_on_send(h); // wake a cooperative receiver parked on this channel's `recv`
        }
        enqueued
    }

    /// Park the running fiber on a full bounded `send`: re-root the receiver AND the value argument on
    /// the operand stack (send is 1-arg, unlike recv's 0-arg — both must be re-pushed or the rewound
    /// `CallMethod(send)` mis-reads the stack), rewind `ip` so the send re-executes on resume, and set
    /// the `send_suspend` sentinel. The scheduler / worker loop files the fiber into the channel's wait
    /// set; a sibling `recv` freeing a slot ([`Vm::wake_senders`]) wakes it. The value is re-serialized
    /// on the re-run (`to_wire_at(orig)` is idempotent), so the wire form built before parking is dropped.
    pub(super) fn park_send(&mut self, h: GcRef, value: Value) {
        self.push(Value::obj(h)); // receiver (deeper on the stack)
        self.push(value); // its one arg, back on top
        self.frames.last_mut().unwrap().ip -= 1;
        self.send_suspend = Some(h);
    }

    /// Bounded-channel backpressure — after a `recv` frees a slot on a BOUNDED channel, wake any fiber
    /// parked on a full `send` to it. No-op for an unbounded channel (no sender ever parks there — the
    /// common `recv` path pays only a `cap.is_none()` check). Routes exactly like `channel_send_wire`'s
    /// wake: an active sched (`mn` / `mn_enlist_sched`) → [`MnSched::recv_wake`]; else the cooperative
    /// engine drains `blocked_on[key]` (`wake_on_send`) + notifies the core condvar (a demoted sender).
    pub(super) fn wake_senders(&mut self, h: GcRef) {
        let core = self.channel_core(h);
        if core.cap.is_none() {
            return;
        }
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
            let key = self.channel_core_ptr(h);
            sched.recv_wake(key, &core);
        } else {
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
        if self.native_reentry == 0 && self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // A tripped latch (`trip()`) delivers `true` immediately and forever, on every engine — a
        // pending queued value (if any) still wins first. Checked before the timer/park logic so a
        // `done().recv()` on a manually-cancelled token never parks.
        {
            let core = self.channel_core(h);
            if core.done_latch.load(Ordering::Relaxed) {
                if let Some(w) = core.q.lock().unwrap().pop() {
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
                if let Some(w) = core.q.lock().unwrap().pop() {
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
            if let Some(w) = g.pop() {
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
        if let Some(w) = g.pop() {
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
        // An EAGERLY-dispatched `Executor` job has no scheduler either, but its submitter IS still
        // running and may send next statement — so it blocks rather than declaring a deadlock. It
        // raises this SAME fault if the submitter turns out to be waiting inside `shutdown()` for it
        // (W7-12).
        if let Some(exec) = self.eager_core.clone() {
            return self.eager_block_recv(&exec, &core, span);
        }
        // No scheduler (top level / single fiber) or a native callback on the cooperative engine: no
        // sibling could ever fill the channel — a real deadlock.
        Err(self.err(EMPTY_RECV_DEADLOCK.to_string(), span))
    }

    /// Park the running fiber on an empty `recv`: re-root the receiver on the operand stack, rewind
    /// `ip` so the current op (`CallMethod(recv)` or `ChanRecvOrClosed`) re-executes on resume, and
    /// set the `suspend` sentinel. The scheduler / worker loop files the fiber into the channel's
    /// wait set; a sibling `send`/`close` wakes it.
    pub(super) fn park_recv(&mut self, h: GcRef) {
        self.push(Value::obj(h));
        self.frames.last_mut().unwrap().ip -= 1;
        self.suspend = Some(h);
    }

    /// EAGER `Executor` job — one tick of a blocking wait: honour the halts, then wait up to
    /// [`DEMOTE_POLL_BACKOFF`]. Shared by [`Vm::eager_block_recv`] and the blocking full-`send`.
    ///
    /// A job dispatched by an eager `submit` has no nursery scheduler and no [`MnSched`], so its
    /// blocking ops fall to the "no scheduler" arms, which declare a deadlock. That verdict was TRUE
    /// while jobs only ran at the drain — the submitter was blocked inside `shutdown()`, so no runnable
    /// task could send — and became a LIE the moment jobs start at `submit`, because the submitter is
    /// still running and may send on the very next statement. So an eager job BLOCKS here instead,
    /// which is also what Python's `ThreadPoolExecutor` does. **No deadlock predicate reads this
    /// path** (decision D — an `Executor`-spanning deadlock is an accepted hang, matching Python).
    ///
    /// The wait is a BOUNDED poll, not an untimed `cv.wait`, for the same reason
    /// [`Vm::demote_recv_block`]'s is: a lost wakeup then costs latency instead of the whole run, and
    /// the two halts that must stay un-swallowable get re-checked every tick. `--timeout` is checked
    /// here explicitly because a blocked job never reaches `jump_checked`'s loop back-edge, which is
    /// where every other path observes the deadline — without this, `chezzi test --timeout` could not
    /// kill a job blocked forever on a channel, which is exactly the hang eager execution makes easier
    /// to write.
    ///
    /// **W7-13 — `ready` is re-checked under the SAME lock hold that the wait consumes, and that is
    /// the whole point of the parameter.** The caller has already tried its operation and failed, but
    /// it dropped `core.q` to do so and then ran [`Vm::eager_halt_check`] (which takes `exec_registry`
    /// and per-core `eager` locks — a wide window). A `notify_all` from a consumer landing anywhere in
    /// that gap reaches a condvar NOBODY IS ON YET and is simply lost, so the caller then slept the
    /// full [`DEMOTE_POLL_BACKOFF`] with its value already waiting. Measured on the 50-handoff cap-1
    /// pipeline: 7 of 15 runs paid a whole extra 5 ms tick, in exact 5 ms quanta.
    ///
    /// `Condvar::wait_timeout_while` closes it — it evaluates the predicate under the guard BEFORE
    /// sleeping, so a wakeup that arrived while the lock was free is observed instead of missed (and
    /// it re-checks on spurious wakeups for free). The wake it is waiting for was never missing:
    /// [`Vm::wake_senders`] already fires on all six pop paths, and for an eager job it lands on
    /// `core.cv`. `eager_halt_check` MUST stay before the lock — the no-lock-cycle argument on
    /// [`Vm::eager_join_deadlocked`] depends on the registry never being taken under `ChannelCore::q`.
    ///
    /// This only makes the CHANNEL conditions instant. The halts `eager_halt_check` acts on — the
    /// `--timeout` deadline, a cancel, the W7-12 verdict — are not in any predicate and are still
    /// observed once per tick, so cancellation is now the SLOWEST thing in this loop rather than the
    /// fastest. That bound is unchanged by this fix, not introduced by it.
    fn eager_wait_tick(
        &mut self,
        core: &Arc<ChannelCore>,
        deadlock_msg: &str,
        span: Span,
        mut ready: impl FnMut(&mut crate::vm::core::ChanState) -> bool,
    ) -> Result<(), RuntimeError> {
        self.eager_halt_check(deadlock_msg, span)?;
        let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
        let _ = core
            .cv
            .wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |g| !ready(g));
        Ok(())
    }

    /// W7-12 — is this eager job blocked on something ONLY its own joiner could deliver?
    ///
    /// `joining > 0` means a thread is inside an explicit `Executor.shutdown()` for this core, i.e.
    /// the submitter is parked waiting for this very job and can no longer reach the `send` that
    /// would free it. `outstanding == 1` means this blocked job is the executor's ONLY remaining work.
    /// Together: nobody left who could feed us — the pre-§2c verdict, restored for exactly the shape
    /// that regressed, without re-breaking the producer/consumer shape eager execution exists to
    /// support (there `joining` is 0 while `main` is still running).
    ///
    /// **Why `outstanding == 1` and not `blocked >= outstanding`.** The general form is WRONG, and
    /// measurably so: PARKED IS NOT UNFEEDABLE. Two jobs on opposite ends of a cap-1 channel — one
    /// parked on a full `send`, one parked on the momentarily-empty `recv` — are the healthy steady
    /// state of a bounded pipeline, and they feed EACH OTHER. `blocked >= outstanding` reads that
    /// handshake as a deadlock, and the debounce does not save it because two consecutive 5 ms
    /// observations land in successive parked windows. Caught by review, on a textbook program Go and
    /// CPython run to completion: `prod`/`cons` over `Channel[int](1)` faulted
    /// `send on a full channel: deadlock` in 2–7 of 30 runs. A nondeterministic wrong answer is the
    /// worst outcome available here, so the counters are only trusted where they cannot be
    /// misinterpreted — a single outstanding job has no sibling to hand off with.
    ///
    /// The cost is under-firing: any multi-job deadlock inside one executor now HANGS instead of
    /// faulting (residuals (a)–(c)). That is the correct direction — decline, never answer wrong —
    /// and telling those apart needs the wait-for graph in `future.md` §2d, not a cleverer counter.
    ///
    /// **And no OTHER live executor may still owe work.** Executors are independent, so a job of `y`
    /// can be the producer for a job of `x` — and `x.shutdown()` says nothing about `y`:
    ///
    /// ```text
    /// x.submit(consumer)   # blocks on ch.recv()
    /// y.submit(producer)   # sleeps, then ch.send(1)
    /// x.shutdown()
    /// ```
    ///
    /// Python's `ThreadPoolExecutor` runs that to completion (`got 1`), and so did Chezzi before this
    /// check was added — reporting a deadlock there is simply a WRONG ANSWER about a live program, not
    /// a tolerable engine difference. The registry sweep is what keeps the verdict honest: if any other
    /// executor still owes a job, somebody may yet send, and we wait. The price is the other direction
    /// — two mutually-deadlocked executors now HANG instead of faulting — which is decision D's
    /// accepted hang and the right way to be wrong: never answer, rather than answer incorrectly.
    ///
    /// Still not sound (that needs `future.md` §2d): a producer reachable through neither an executor
    /// nor the joiner is invisible here. It is merely no longer wrong about the shapes an ancestor
    /// language runs successfully.
    ///
    /// Lock discipline: takes `exec_registry`, then one `eager` per core, never both at once and never
    /// while holding `ChannelCore::q` (the halt check runs before `eager_wait_tick` locks the queue).
    /// `eager` is a suffix of this file's one nesting (`inner` → `eager`), and nothing anywhere takes
    /// the registry while holding a core lock, so this adds no cycle.
    fn eager_join_deadlocked(&self) -> bool {
        let Some(core) = &self.eager_core else {
            return false;
        };
        if core.joining.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let outstanding = core
            .eager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outstanding();
        if outstanding != 1 || core.blocked.load(Ordering::Relaxed) == 0 {
            return false;
        }
        // Snapshot the registry and DROP its lock before touching any core lock.
        let others: Vec<Arc<ExecutorCore>> = self
            .exec_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|c| !Arc::ptr_eq(c, core))
            .map(Arc::clone)
            .collect();
        !others.iter().any(|c| {
            c.eager
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .outstanding()
                > 0
        })
    }

    /// W7-12 — may THIS `shutdown()` caller's join license the "nobody can send" verdict?
    ///
    /// Only if the caller is the entire running program: the top-level `main` thread, with no M:N
    /// worker context, no cooperative nursery, no inline `parallel:` builder, and not itself an eager
    /// job. Any of those means a SIBLING is running concurrently and may be the very producer the
    /// blocked job is waiting for — and then the fault is simply wrong. It was measurably wrong
    /// before this gate: `parallel: { spawn: sleep; ch.send(42) } { spawn: ex.shutdown() }` succeeds
    /// on `--serial` and faulted on M:N, i.e. the fix re-opened, in a new place, exactly the kind of
    /// engine divergence W7-12 exists to close.
    ///
    /// This gate is one of three narrowings, none of which make the predicate SOUND — the registry
    /// sweep and `outstanding == 1` are the others, and everything they decline to judge hangs
    /// (gaps.md `W7-12r`). Soundness needs the wait-for graph (`future.md` §2d).
    fn join_has_no_live_siblings(&self) -> bool {
        self.mn.is_none()
            && self.mn_enlist_sched.is_none()
            && self.scheduler_stack.is_empty()
            && self.eager_core.is_none()
            && self.native_reentry == 0
    }

    /// W7-12 — arm [`ExecutorCore::blocked`] for as long as the returned guard lives, so a concurrent
    /// [`Vm::eager_join_deadlocked`] can see this job parked. `None` off the eager path (the guard is
    /// then a no-op), which keeps the three blocking sites free of an `eager_core` re-test.
    fn eager_block_guard(&self) -> Option<BlockGuard> {
        self.eager_core.as_ref().map(BlockGuard::new)
    }

    /// W7-12 — clear a pending deadlock suspicion because this block ENDED successfully. Called at
    /// every success exit of the three eager blocking sites rather than at their entries: the `wait:`
    /// arm has no loop to enter (it rewinds `ip` and re-runs the op), so clearing on entry there
    /// would reset the debounce every tick and that site could never reach a second observation.
    fn eager_block_settled(&mut self) {
        self.eager_block_suspect = false;
    }

    /// The halts an eagerly-dispatched job must observe while blocked. Split out of
    /// [`Vm::eager_wait_tick`] so the multi-channel `wait:` path — which polls N arms instead of
    /// waiting on one condvar, and so cannot share the tick — honours exactly the same two, rather
    /// than being the one blocking op a `--timeout` cannot reach.
    fn eager_halt_check(&mut self, deadlock_msg: &str, span: Span) -> Result<(), RuntimeError> {
        // Checked HERE because a blocked job never reaches `jump_checked`'s loop back-edge, which is
        // where every other path observes the deadline. Without it `chezzi test --timeout` could not
        // kill a job blocked forever on a channel — exactly the hang eager execution makes easier to
        // write. No `deadline_tick` throttle: this runs once per `DEMOTE_POLL_BACKOFF`, not per op.
        if let Some(dl) = self.deadline
            && std::time::Instant::now() >= dl
        {
            return Err(self
                .err(
                    format!("test exceeded --timeout ({}ms)", self.timeout_ms),
                    span,
                )
                .timed_out());
        }
        // `shutdown_now`'s cooperative stop (D4) and an enclosing scope's cancel both arrive here.
        if self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // W7-12, checked LAST so the two real halts still outrank it. Two CONSECUTIVE positives are
        // required: the caller re-attempts the operation between two ticks (`pop` / `enqueue` / a full
        // `wait:` re-poll), so a value that landed just before the first observation is taken instead
        // of being reported as a deadlock — which is exactly the `send(7); shutdown()` race in
        // `executor_job_blocking_recv_waits_for_a_later_send`.
        if self.eager_join_deadlocked() {
            if self.eager_block_suspect {
                return Err(self.err(deadlock_msg.to_string(), span));
            }
            self.eager_block_suspect = true;
        } else {
            self.eager_block_suspect = false;
        }
        Ok(())
    }

    /// EAGER `Executor` job — block on an empty `recv` until a value arrives, instead of declaring a
    /// deadlock (see [`Vm::eager_wait_tick`]). The settle order matches [`Vm::demote_recv_block`]'s
    /// exactly — a queued value beats a `trip()` latch, which beats closed-and-drained, which beats a
    /// cancel — so the two blocking-in-place paths cannot disagree about what a channel is saying.
    pub(super) fn eager_block_recv(
        &mut self,
        exec: &Arc<ExecutorCore>,
        core: &Arc<ChannelCore>,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        // W7-12 — armed once this job has actually FAILED to make progress, not on entry: `blocked`
        // must mean "parked", not "about to look". A job whose value is already queued but which is
        // descheduled between arming and its first `pop` would otherwise be counted as parked, and a
        // sibling could reach the deadlock verdict over a job that was about to succeed. Held for the
        // rest of the block, so a `shutdown()` joining this executor sees its last job parked (and
        // this job sees the join back, via `eager_halt_check`).
        let mut _blocked = None;
        loop {
            {
                let mut q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(w) = q.pop() {
                    self.eager_block_settled();
                    return Ok(RecvStep::Got(w));
                }
                let closed = q.closed;
                drop(q);
                if core.done_latch.load(Ordering::Relaxed) {
                    self.eager_block_settled();
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
                }
                if closed {
                    self.eager_block_settled();
                    return Ok(RecvStep::ClosedEmpty);
                }
            }
            if _blocked.is_none() {
                _blocked = Some(BlockGuard::new(exec));
            }
            // Ready == the same three settle conditions the loop head consumes, in the same order, so
            // the wait cannot sleep through a state the next iteration would immediately take (W7-13).
            //
            // The three are NOT equally well served, and the difference is the writer's lock, not this
            // predicate. A queued value and `closed` are both written under `core.q` (`ChanState::push`
            // and `close`'s `q.lock().closed = true`), so re-checking them under the guard the wait
            // consumes genuinely closes the window. `done_latch` closes it too **since W7-13r(b)**:
            // `trip()` now stores the latch while holding `core.q`, exactly as `close()` has always
            // set `closed`, so a `trip()` can no longer land between this evaluation and the wait's
            // atomic release-and-enqueue. (Before that it was a bare atomic written outside `q`, and
            // this term narrowed the race without closing it.)
            self.eager_wait_tick(core, EMPTY_RECV_DEADLOCK, span, |g| {
                !g.is_empty() || g.closed || core.done_latch.load(Ordering::Relaxed)
            })?;
        }
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
        if self.native_reentry == 0 && self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        let n = meta.n;
        // Per-arm stack width: a recv arm holds ONE slot (the channel handle), a SEND arm holds TWO
        // (channel THEN value). Walk a running `off` cursor from `base` so send arms are read correctly.
        let slot_width = |is_send: bool| if is_send { 2 } else { 1 };
        let total: usize = meta.is_send.iter().map(|&s| slot_width(s)).sum();
        let base = self.stack.len() - total;
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        let mut off = 0usize;
        for i in 0..n {
            let slot = base + off;
            let Some(h) = self.stack[slot].as_obj() else {
                unreachable!("wait arm operand is not a channel handle");
            };
            off += slot_width(meta.is_send[i]);
            let core = self.channel_core(h);
            if meta.is_send[i] {
                // SEND arm — ready when the channel can accept the value (bounded-with-space /
                // unbounded / closed). A closed channel is READY-and-FAULTS (Go's panic-on-send-to-
                // closed; the exact bare-`send` message), SELECTED not skipped — first-ready wins in
                // source order, so reaching it means no earlier arm was ready. A FULL bounded channel
                // is NOT ready (park until a receiver frees a slot). Value is serialized + enqueued
                // atomically here (source order, once selected), never on a not-ready poll.
                if core.q.lock().unwrap().closed {
                    return Err(self.err(CLOSED_SEND.to_string(), span));
                }
                let val = self.stack[slot + 1];
                let w = self.to_wire_crossable(val, span)?;
                let sent = match core.cap {
                    None => {
                        self.channel_send_wire(h, w);
                        true
                    }
                    Some(cap) => self.enqueue_bounded(h, &core, cap, w),
                };
                if sent {
                    self.take_wait_send_arm(base, meta.arm_targets[i]);
                    return Ok(());
                }
                all_closed = false; // full bounded send — live (wakes when a receiver frees a slot)
                continue;
            }
            let (popped, closed) = {
                let mut g = core.q.lock().unwrap();
                (g.pop(), g.closed)
            };
            if let Some(w) = popped {
                self.wake_senders(h); // a `wait:` arm freed a slot — wake a parked bounded sender
                let v = self.from_wire(w);
                self.take_wait_arm(base, v, meta.arm_targets[i]);
                return Ok(());
            }
            // A tripped latch (`trip()`) is ready like a fired timer — take the arm with `true`.
            if core.done_latch.load(Ordering::Relaxed) {
                self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
                return Ok(());
            }
            if let Some(deadline) = core.timer {
                // A timer channel is never closed and always eventually ready: fired now → take it;
                // otherwise a live waiter whose deadline we may sleep to below.
                if std::time::Instant::now() >= deadline {
                    self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
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
        // Block on all live arms. The arm operands are on the stack (they root the channels + re-supply
        // the poll on resume). A live timer arm (`soonest`) is just another arm bucket on the M:N paths.
        // `keys[i]` = (channel handle, is_send) so the park-gap re-check applies the right readiness
        // predicate per arm (recv wakes on a sender; send wakes on a receiver freeing a slot).
        let keys: Vec<(GcRef, bool)> = {
            let mut v = Vec::with_capacity(n);
            let mut off = 0usize;
            for &is_send in &meta.is_send {
                let h = self.stack[base + off]
                    .as_obj()
                    .expect("wait arm operand is not a channel handle");
                v.push((h, is_send));
                off += slot_width(is_send);
            }
            v
        };
        // v1 limit (§6d): a live SEND arm reaching the block section INSIDE a native callback
        // (`native_reentry > 0`) can only be a FULL bounded send (a ready arm — unbounded/closed/
        // free-slot — was taken at poll), and it cannot be parked or demoted on EITHER engine: the
        // M:N demote path POPS recv queues (`demote_wait_block`) and would wrongly steal a send-arm
        // channel's queued message as a received value, and the serial engine has no cooperative
        // yield inside a callback. Fault identically on BOTH engines here — before the engine split —
        // so serial and M:N emit byte-identical text (the FULL_SEND_DEADLOCK doc-comment's parity
        // contract), matching the plain in-callback full-send fault (`chan_send_step`, netio.rs:1383).
        // ponytail: upgrade path = a demote-in-place send block (mirror `demote_recv_block`).
        if self.native_reentry > 0 && keys.iter().any(|&(_, is_send)| is_send) {
            return Err(self.err(FULL_SEND_DEADLOCK.to_string(), span));
        }
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
                let key = self.channel_core_ptr(keys[i].0);
                let core_job = self.channel_core(keys[i].0);
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
            // (A live SEND arm reaching this demote path already faulted above, before the engine
            // split — on BOTH engines — so every arm here is a recv/timer arm the demote loop pops.)
            let arms: Vec<(usize, Arc<ChannelCore>)> = keys
                .iter()
                .map(|&(h, _)| (self.channel_core_ptr(h), self.channel_core(h)))
                .collect();
            let (arm_index, w) = self.demote_wait_block(arms, soonest, span)?;
            self.wake_senders(keys[arm_index].0); // demoted `wait:` freed a slot — wake a bounded sender
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
            self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
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
        // EAGER `Executor` job — block instead of declaring a deadlock, like the empty-`recv` and
        // full-`send` cases, then REWIND so the dispatch loop re-runs this `WaitPoll` and re-polls
        // every arm. Rewinding rather than looping in place also means the halts land on the ordinary
        // back-edge checkpoint.
        if self.eager_core.is_some() {
            // W7-12 — armed FIRST, before the halt check, so this job counts ITSELF as blocked; armed
            // after it, a LONE `wait:`-blocked job (the only shape the verdict now judges) would
            // forever see `blocked (0) < outstanding (1)` and never fault. Re-armed per invocation
            // because this arm rewinds instead of looping; harmless now that the verdict requires
            // `outstanding == 1`, so there is no sibling whose sample could catch the gap.
            let _blocked = self.eager_block_guard();
            self.eager_halt_check(EMPTY_WAIT_DEADLOCK, span)?;
            // W7-13r(a) — this used to be a bare `thread::sleep(DEMOTE_POLL_BACKOFF)`, so EVERY wake
            // cost a full tick no matter how fast the value arrived. There are N arm condvars and no
            // single one to block on, so it cannot be a plain targeted wait — but it does not have to
            // be a blind sleep either: wait on the FIRST arm's condvar with the tick as the timeout,
            // exactly as [`Vm::demote_wait_block`] already does. Arm 0 then wakes promptly and every
            // other arm is still observed within a tick. (An earlier note claimed fixing this needed a
            // new multi-channel wait primitive; that was wrong — the precedent was already in the
            // tree.) It is better than the sleep for every arm-0 wake and no slower otherwise — but
            // "strictly better, never worse" was the FIRST draft's claim and it was false, because a
            // wrong predicate makes it a live-lock rather than a slower sleep. See below.
            let (h0, is_send0) = keys[0]; // non-empty: an all-closed arm set returned above
            let first = self.channel_core(h0);
            let cap0 = first.cap;
            // Readiness for ARM 0 only, evaluated under the guard the wait consumes (W7-13's rule).
            // Every other arm is covered by the timeout, exactly as under the old blind sleep.
            //
            // **The predicate must mirror what the poll above SETTLES on, arm kind by arm kind — not
            // what merely "changed".** Getting this wrong is a live-lock, not a latency bug, and the
            // first version of this fix shipped it: a recv arm read `|| g.closed`, but the poll SKIPS
            // a closed+empty recv arm (the `else if !closed` branch), so the predicate said ready, the
            // wait returned instantly, `ip -= 1` re-polled, the arm was skipped again — a 100% CPU
            // spin on Go's ordinary `select { case <-done: ; case v := <-work: }` with `done` closed.
            // Measured 0.01 s user / 0% CPU before, 3.00 s user / 99% CPU after. That is the same
            // live-lock `MnSched::park_wait` already warns about ("the reverted parity-perf-0
            // live-lock") — the rule was written down, and the first draft broke it anyway.
            //
            // So, taken from the poll's own arms:
            //   * RECV ready == a queued value, or a `trip()` latch. NOT `closed` (the poll skips a
            //     dead arm), and a timer's deadline is left to the timeout.
            //   * SEND ready == space to enqueue, or `closed` — a closed send arm IS acted on: the
            //     poll faults `CLOSED_SEND` (Go's panic-on-send-to-closed).
            // An all-closed arm set costs one tick before the `wait: all channels closed` fault, which
            // is exactly what the blind sleep cost.
            //
            // No timer clamp here, deliberately: `soonest` is provably `None` at this point, because
            // the cooperative inline-sleep above returns for every `soonest.is_some()` case and an
            // eager job never takes the `self.mn.is_some()` branch. (A clamp WAS written here at
            // first; it was dead code. The inline-sleep's own limit — an eager `wait:` sleeps to the
            // timer deadline and cannot take a sibling value that arrives sooner, where Go's `select`
            // would — is filed as `docs/gaps.md` W7-14, not something this predicate can reach.)
            let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
            let _ = first.cv.wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |g| {
                let ready = if is_send0 {
                    cap0.is_none_or(|c| g.len() < c) || g.closed
                } else {
                    !g.is_empty() || first.done_latch.load(Ordering::Relaxed)
                };
                !ready
            });
            self.frames.last_mut().unwrap().ip -= 1;
            return Ok(());
        }
        // No scheduler (top level / single fiber) or inside a native callback: no sibling could ever
        // fill the channels — a real deadlock (mirrors `chan_recv_step`'s sequential `recv` fault).
        Err(self.err(EMPTY_WAIT_DEADLOCK.to_string(), span))
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push the received
    /// value, and jump to the arm body's target ip (the bind/assign/discard prologue).
    pub(super) fn take_wait_arm(&mut self, base: usize, value: Value, target: usize) {
        self.eager_block_settled(); // W7-12 — an arm fired, so any pending deadlock suspicion is stale
        self.stack.truncate(base);
        self.push(value);
        self.frames.last_mut().unwrap().ip = target;
    }

    /// Commit a chosen SEND `wait` arm: drop all arm operands (`stack[base..]`, including this arm's
    /// value, already enqueued by the poll) and jump to the arm body — which binds NOTHING, so unlike
    /// [`Vm::take_wait_arm`] this pushes no value.
    pub(super) fn take_wait_send_arm(&mut self, base: usize, target: usize) {
        self.eager_block_settled(); // W7-12 — see [`Vm::take_wait_arm`]
        self.stack.truncate(base);
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
                let w = self.to_wire_crossable(args[0], span)?;
                self.shared_core(h).store(w);
                Ok(Value::nil())
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
                self.push(Value::obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_crossable(next, span)?;
                core.store(stored);
                Ok(Value::nil())
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
                let w = self.to_wire_crossable(args[0], span)?;
                self.rwshared_core(h).store(w);
                Ok(Value::nil())
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
                self.push(Value::obj(h));
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
                self.push(Value::obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_crossable(next, span)?;
                core.store(stored);
                Ok(Value::nil())
            }
            // Zero-copy READ-view methods on a CONTAINER element of `RwShared[T]` — `List[E]`
            // (len/at/slice/for_each/fold), `Map[K,V]` (len/get_key/has/for_each_entry/fold_entries),
            // `Set[E]` (len/contains/for_each/fold). Checker-gated to the recognized container head.
            // They walk the stored heap-independent `WireValue::List`/`Map`/`Set` Vec and `from_wire`
            // ONE entry per step — O(1) memory, never materializing the whole inner (what `get`/`read`
            // do). serial ==
            // M:N is natural: the walk reads a heap-independent wire form, so both engines see identical
            // elements. The read-only `len`/`at`/`slice` take the shared guard only for the brief clone
            // (no user code under it). `for_each`/`fold` RE-ACQUIRE the shared guard PER ELEMENT, clone
            // one wire element, DROP the guard, then run the closure — mirroring `read`'s
            // clone-out-then-drop, per element. The guard is NEVER held across the user closure (or the
            // GC's mark of `Obj::RwShared`, which re-locks `core.v`), so a nested read/write of the SAME
            // box, an AB-BA cross-box walk, and a GC pass triggered inside the closure can't deadlock —
            // the write-preferring `std::sync::RwLock` never sees a recursive read behind a queued writer.
            //
            // W7-11 — every per-piece rebuild goes through [`Vm::from_wire_piece`] rather than
            // `from_wire`, because a piece whose cycle closes through the ROOT container is not
            // self-contained and used to ABORT THE HOST. The helper takes `&WireValue` (the caller's
            // live guard), never the core: it must not re-acquire `core.v`, and the guard must still be
            // held across the rebuild so its fallback resolves the piece against the SAME serialization
            // it was cloned from (a second acquisition is the torn read `docs/gaps.md` W7-4 round 2 hit).
            // Holding it across `from_wire*` is safe and is exactly the window `at`/`slice` already
            // held: it allocs and nothing else, and `Heap::alloc` never collects, so no GC can re-lock
            // `core.v` underneath. The guard is still DROPPED before any user code (closure/hash/eq).
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let n = match &*g {
                    WireValue::List { items, .. } => items.len() as i64,
                    WireValue::Map { entries, .. } => entries.len() as i64,
                    WireValue::Set { entries, .. } => entries.len() as i64,
                    _ => {
                        return Err(
                            self.err("RwShared.len requires a container element".into(), span)
                        );
                    }
                };
                Ok(self.make_int(n))
            }
            // `at(i) -> Option[E]` — out of range is `None`, not a fault, matching the language's
            // other named-accessor spellings: `get_key(k) -> Option[V]` below and
            // `std.json.at -> Option[Json]`. (`RwShared` itself has no `[]` — it does not satisfy the
            // `Index` protocol, since a view walks the stored wire and has no heap object to dispatch
            // a user `index()` on — so this is the ONLY read accessor here, and it reports absence
            // rather than faulting.) A wrong container HEAD is still a fault: that is a type error,
            // not a missing element. Negative indexing (`at(-1)`) is unchanged — `norm_index` first.
            "at" => {
                self.arity_err("at", args, 1, span)?;
                let i = self.int_of(args[0]);
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let ew = match &*g {
                    WireValue::List { items, .. } => match crate::slice::norm_index(i, items.len())
                    {
                        Some(u) => items[u].clone(),
                        None => {
                            drop(g);
                            return Ok(self.alloc_enum("Option", "None", vec![]));
                        }
                    },
                    _ => return Err(self.err("RwShared.at requires a list element".into(), span)),
                };
                let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                let v = self.from_wire_piece(&g, ew, &mut rb);
                drop(g); // the rebuild is done — release before the `Option` wrapper allocs
                Ok(self.alloc_enum("Option", "Some", vec![v]))
            }
            "slice" => {
                self.arity_err("slice", args, 2, span)?;
                let lo = self.int_of(args[0]);
                let hi = self.int_of(args[1]);
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let idxs = match &*g {
                    WireValue::List { items, .. } => {
                        crate::slice::slice_indices(Some(lo), Some(hi), None, items.len())
                            .map_err(|e| self.err(e.to_string(), span))?
                    }
                    _ => {
                        return Err(self.err("RwShared.slice requires a list element".into(), span));
                    }
                };
                // Materialize ONLY [lo:hi] into a fresh list, rooted on the operand stack across the
                // per-element `from_wire`s (defensive — `from_wire` only allocs and `alloc` never
                // collects, but rooting matches the list-HOF precedent and is future-proof).
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::obj(res_h));
                // W7-4: ONE rebuild map across the sliced-out elements — `slice` is a SINGLE crossing
                // that returns a container (like `get`), so two sliced-out closures over the same
                // captured local land on ONE cell. A per-element view (`at`, `for_each`) is its own
                // crossing and keeps its own copy.
                let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                // W7-11 — the whole-container decision is made ONCE, before the first element, and
                // that is load-bearing (adversarial review, round 2). `from_wire_piece`'s fallback
                // rebuilds the container into this shared map, and the container arms of
                // `from_wire_memo` have NO first-wins dedupe (only `Cell` does) — so a fallback taken
                // at element k OVERWRITES `rb` for elements 0..k, orphaning the copies already pushed
                // into the result. Identity then depended on element ORDER: with only element 1
                // cyclic, `sl[1].back[0]` was a different object than `sl[0]` (CPython: the same one).
                // Deciding up front means every element is served from one container, whichever of
                // them needs it.
                if idxs.iter().any(|&i| match &*g {
                    WireValue::List { items, .. } => !items[i].backrefs_resolvable(&rb),
                    _ => false,
                }) {
                    let _whole = self.from_wire_memo((*g).clone(), &mut rb);
                }
                for idx in idxs {
                    let ew = match &*g {
                        WireValue::List { items, .. } => items[idx].clone(),
                        _ => unreachable!(),
                    };
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(elem);
                    }
                }
                self.pop();
                Ok(Value::obj(res_h))
            }
            // `for_each(f: fn(E) -> _)` on a `List[E]` OR `Set[E]` — a per-element side-effect scan.
            "for_each" => {
                self.arity_err("for_each", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // Snapshot the length under a brief guard (dropped immediately).
                let n = match &*core.v.read().unwrap() {
                    WireValue::List { items, .. } => items.len(),
                    WireValue::Set { entries, .. } => entries.len(),
                    _ => {
                        return Err(self.err(
                            "RwShared.for_each requires a list or set element".into(),
                            span,
                        ));
                    }
                };
                self.push(Value::obj(h)); // root the receiver across nested GC
                for i in 0..n {
                    // RE-ACQUIRE the shared guard, clone ONE element, rebuild it, DROP the guard
                    // before the closure — never hold `core.v` across `invoke_value`/GC (see the
                    // arm's header comment). The rebuild stays INSIDE the guard so W7-11's fallback
                    // resolves the piece against the same serialization it was cloned from.
                    let g = core.v.read().unwrap();
                    let ew = match &*g {
                        WireValue::List { items, .. } => {
                            if i >= items.len() {
                                break; // the list shrank under a concurrent write — stop
                            }
                            items[i].clone()
                        }
                        WireValue::Set { entries, .. } => {
                            if i >= entries.len() {
                                break;
                            }
                            entries[i].1.clone()
                        }
                        _ => break, // replaced by a non-container under a concurrent set — stop
                    };
                    let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    drop(g);
                    self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                }
                self.pop();
                Ok(Value::nil())
            }
            // `fold(init, f: fn(R, E) -> R) -> R` on a `List[E]` OR `Set[E]`.
            "fold" => {
                self.arity_err("fold", args, 2, span)?;
                let init = args[0];
                let f = args[1];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::List { items, .. } => items.len(),
                    WireValue::Set { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.fold requires a list or set element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(init); // root the accumulator; its slot sits below every nested frame's base
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // RE-ACQUIRE per element, rebuild under the guard, DROP before the closure (see
                    // the arm's header comment).
                    let g = core.v.read().unwrap();
                    let ew = match &*g {
                        WireValue::List { items, .. } => {
                            if i >= items.len() {
                                break;
                            }
                            items[i].clone()
                        }
                        WireValue::Set { entries, .. } => {
                            if i >= entries.len() {
                                break;
                            }
                            entries[i].1.clone()
                        }
                        _ => break,
                    };
                    let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    drop(g);
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot receiver
                Ok(acc)
            }
            // Set[E] membership: hash the query element ONCE (guard NOT held — `hash_value` may
            // dispatch a user `hash`/GC), then LINEAR probe: per element re-lock, compare the cached
            // wire hash under the guard, and only on a hash-match clone the element wire, DROP the
            // guard, `from_wire`, and `values_equal_guarded` (collisions keep scanning). The guard is
            // NEVER held across hash/eq (same deadlock invariant as `for_each`/`fold`).
            "contains" => {
                self.arity_err("contains", args, 1, span)?;
                let needle = args[0];
                // Root the receiver `h` AND `needle` across the hash: for a struct/enum/newtype
                // element `hash_value` dispatches the user `hash` (re-enters the VM, may GC), and
                // `h`/`needle` are off the operand stack (popped at dispatch) so they'd be collectable
                // mid-hash → the following `rwshared_core(h)` would hit a freed slot. Mirrors the
                // non-RwShared Set `in` path (arith.rs:913).
                let qh = self.hash_key_rooted(needle, &[Value::obj(h), needle], span)?;
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Set { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.contains requires a set element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(needle); // root the query element across from_wire/eq (may GC)
                let mut found = false;
                for i in 0..n {
                    // Clone AND rebuild the element under ONE guard (W7-11 — see the arm header), then
                    // drop it at block end, before the `eq` probe re-enters the VM.
                    let e = {
                        let g = core.v.read().unwrap();
                        let entries = match &*g {
                            WireValue::Set { entries, .. } => entries,
                            _ => break,
                        };
                        if i >= entries.len() {
                            break;
                        }
                        if entries[i].0 != qh {
                            continue; // hash miss — keep scanning
                        }
                        let ew = entries[i].1.clone();
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        self.from_wire_piece(&g, ew, &mut rb)
                    };
                    if self.values_equal_guarded(e, needle, 0, span)? {
                        found = true;
                        break;
                    }
                }
                self.pop();
                self.pop();
                Ok(Value::bool(found))
            }
            // Map[K,V].has(k) -> bool — same hash-once + per-probe re-lock discipline as `contains`,
            // comparing the KEY (entry.1 is the key wire, entry.2 the value wire).
            "has" => {
                self.arity_err("has", args, 1, span)?;
                let key = args[0];
                // Root receiver `h` AND `key` across the hash — a struct/enum/newtype key's `hash`
                // re-enters the VM and may GC; both are off the operand stack here. Mirrors the
                // non-RwShared Map path (arith.rs:921).
                let qh = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => return Err(self.err("RwShared.has requires a map element".into(), span)),
                };
                self.push(Value::obj(h));
                self.push(key);
                let mut found = false;
                for i in 0..n {
                    // Clone AND rebuild the key under ONE guard (W7-11 — see the arm header).
                    let k = {
                        let g = core.v.read().unwrap();
                        let entries = match &*g {
                            WireValue::Map { entries, .. } => entries,
                            _ => break,
                        };
                        if i >= entries.len() {
                            break;
                        }
                        if entries[i].0 != qh {
                            continue;
                        }
                        let kw = entries[i].1.clone();
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        self.from_wire_piece(&g, kw, &mut rb)
                    };
                    if self.values_equal_guarded(k, key, 0, span)? {
                        found = true;
                        break;
                    }
                }
                self.pop();
                self.pop();
                Ok(Value::bool(found))
            }
            // Map[K,V].get_key(k) -> Option[V] — probe as `has`, but on an eq-match `from_wire` the
            // VALUE wire and return `Some(v)`; `None` on a full miss.
            "get_key" => {
                self.arity_err("get_key", args, 1, span)?;
                let key = args[0];
                // Root receiver `h` AND `key` across the hash — a struct/enum/newtype key's `hash`
                // re-enters the VM and may GC; both are off the operand stack here. Mirrors the
                // non-RwShared Map path (arith.rs:921).
                let qh = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.get_key requires a map element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h));
                self.push(key);
                let mut result: Option<Value> = None;
                for i in 0..n {
                    // Clone AND rebuild BOTH key and value on a hash-match under ONE lock acquire
                    // (W7-11 — the rebuild must see the same serialization the wires came from), DROP
                    // the guard, then eq the reconstructed key — if it matches, the value is in hand.
                    // The value is rebuilt eagerly rather than after the eq, which is what keeps this
                    // to one guard; the cost is one extra rebuild per hash COLLISION that then fails
                    // eq (rare by construction) and the wires were already cloned eagerly for the same
                    // reason. No rooting is needed across the eq: `values_equal_guarded` takes `&self`
                    // (`arith.rs:1752`), so it cannot allocate or collect.
                    let (k, v) = {
                        let g = core.v.read().unwrap();
                        let entries = match &*g {
                            WireValue::Map { entries, .. } => entries,
                            _ => break,
                        };
                        if i >= entries.len() {
                            break;
                        }
                        if entries[i].0 != qh {
                            continue;
                        }
                        let (kw, vw) = (entries[i].1.clone(), entries[i].2.clone());
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let k = self.from_wire_piece(&g, kw, &mut rb);
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        (k, self.from_wire_piece(&g, vw, &mut rb))
                    };
                    if self.values_equal_guarded(k, key, 0, span)? {
                        result = Some(v);
                        break;
                    }
                }
                self.pop();
                self.pop();
                Ok(match result {
                    Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            // Map[K,V].for_each_entry(f: fn(K, V) -> _) — per-entry side-effect scan (2-arg closure).
            "for_each_entry" => {
                self.arity_err("for_each_entry", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => {
                        return Err(self.err(
                            "RwShared.for_each_entry requires a map element".into(),
                            span,
                        ));
                    }
                };
                self.push(Value::obj(h));
                for i in 0..n {
                    // Clone AND rebuild both halves of the entry under ONE guard, dropped before the
                    // closure (W7-11 — see the arm header).
                    let (k, v) = {
                        let g = core.v.read().unwrap();
                        let (kw, vw) = match &*g {
                            WireValue::Map { entries, .. } => {
                                if i >= entries.len() {
                                    break;
                                }
                                (entries[i].1.clone(), entries[i].2.clone())
                            }
                            _ => break,
                        };
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let k = self.from_wire_piece(&g, kw, &mut rb);
                        // Root the reconstructed key while building the value (both alloc; `alloc`
                        // never collects, but rooting matches the receiver-rooting precedent).
                        self.push(k);
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let v = self.from_wire_piece(&g, vw, &mut rb);
                        (self.pop(), v)
                    };
                    self.guarded(|vm| vm.invoke_value(f, vec![k, v], span))?;
                }
                self.pop();
                Ok(Value::nil())
            }
            // Map[K,V].fold_entries(init, f: fn(R, K, V) -> R) -> R — per-entry reduce (3-arg closure).
            "fold_entries" => {
                self.arity_err("fold_entries", args, 2, span)?;
                let init = args[0];
                let f = args[1];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.fold_entries requires a map element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(init); // root the accumulator
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // One guard for the clone AND both rebuilds, dropped before the closure (W7-11).
                    let (k, v) = {
                        let g = core.v.read().unwrap();
                        let (kw, vw) = match &*g {
                            WireValue::Map { entries, .. } => {
                                if i >= entries.len() {
                                    break;
                                }
                                (entries[i].1.clone(), entries[i].2.clone())
                            }
                            _ => break,
                        };
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let k = self.from_wire_piece(&g, kw, &mut rb);
                        self.push(k); // root key while reconstructing value
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let v = self.from_wire_piece(&g, vw, &mut rb);
                        (self.pop(), v)
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, k, v], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot receiver
                Ok(acc)
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
                let w = self.to_wire_crossable(args[0], span)?;
                self.atomic_core(h).store(w);
                Ok(Value::nil())
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let new_w = self.to_wire_crossable(args[0], span)?;
                // Summarise BEFORE taking the value lock (the walk is O(payload)) — see
                // `SharedCore::store`.
                let sum = crate::vm::core::wire_summary(&new_w);
                let core = self.atomic_core(h);
                let old = {
                    let mut g = core.v.lock().unwrap();
                    core.store_guarded(&mut g, new_w, sum)
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
                // Propagate a cyclic-operand depth fault (`?`) instead of swallowing it — consistent
                // with `==` and every container membership site. The `?` runs BEFORE the store, so a
                // fault leaves the box unchanged (the lock guard `g` drops on the early return).
                let swapped = self.values_equal_guarded(cur, args[0], 0, span)?;
                if swapped {
                    // Reject a non-crossable store BEFORE the assignment — a failed store leaves the
                    // box unchanged (recoverable, no partial write). `ensure_crossable` borrows `&self`
                    // not the guard `g`, so it is safe to call under the value lock.
                    let next = self.to_wire_crossable(args[1], span)?;
                    let sum = crate::vm::core::wire_summary(&next);
                    core.store_guarded(&mut g, next, sum);
                }
                Ok(Value::bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                // Uniformity: route through the guard like every other store site. The checker gates
                // `add`/`sub` to numeric deltas, so the handle-reject arm is dead-but-harmless here —
                // it just means a future non-numeric delta path can't forget the guard.
                let delta = self.to_wire_crossable(args[0], span)?;
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
                let sum = crate::vm::core::wire_summary(&new);
                core.store_guarded(&mut g, new.clone(), sum);
                drop(g);
                Ok(self.from_wire(new))
            }
            _ => Err(self.err(format!("type Atomic has no method '{method}'"), span)),
        }
    }

    /// `AtomicInt` methods: `load`, `store`, `exchange`, `cas(expected, new) -> bool`, `add`/`sub`
    /// (returns the NEW value). Backed by a raw lock-free `AtomicI64` — every op uses `SeqCst` ordering
    /// (matches the sequential consistency `Atomic`'s Mutex gave, so serial == M:N is byte-identical).
    /// `add`/`sub` KEEP the i64-overflow fault via a CHECKED `compare_exchange` CAS-loop (NOT raw
    /// `fetch_add`/`fetch_sub`, which wrap silently) — error string byte-identical to `atomic_method`'s.
    pub(super) fn atomic_int_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use std::sync::atomic::Ordering::SeqCst;
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let cur = self.atomic_int_core(h).v.load(SeqCst);
                Ok(self.make_int(cur))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let n = self.int_of(args[0]);
                self.atomic_int_core(h).v.store(n, SeqCst);
                Ok(Value::nil())
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let n = self.int_of(args[0]);
                let old = self.atomic_int_core(h).v.swap(n, SeqCst);
                Ok(self.make_int(old))
            }
            "cas" => {
                self.arity_err("cas", args, 2, span)?;
                let expected = self.int_of(args[0]);
                let new = self.int_of(args[1]);
                let swapped = self
                    .atomic_int_core(h)
                    .v
                    .compare_exchange(expected, new, SeqCst, SeqCst)
                    .is_ok();
                Ok(Value::bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.int_of(args[0]);
                let core = self.atomic_int_core(h);
                let label = if method == "add" { "Add" } else { "Sub" };
                // Checked compare_exchange CAS-loop: raw fetch_add/fetch_sub wrap silently (behavior
                // regression vs Atomic's Mutex + checked_add). Retry on a racing writer.
                loop {
                    let cur = core.v.load(SeqCst);
                    let r = if method == "add" {
                        cur.checked_add(delta)
                    } else {
                        cur.checked_sub(delta)
                    };
                    let new =
                        r.ok_or_else(|| self.err(format!("integer overflow in {label}"), span))?;
                    if core.v.compare_exchange(cur, new, SeqCst, SeqCst).is_ok() {
                        return Ok(self.make_int(new));
                    }
                }
            }
            _ => Err(self.err(format!("type AtomicInt has no method '{method}'"), span)),
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
                // Cheap early reject so a shut executor costs no wiring work. Re-checked below under
                // the lock — this one is advisory, the one that decides is the atomic one.
                if core.inner.lock().unwrap().shut {
                    return Err(self.err(
                        "submit on a shut-down Executor (it no longer accepts work)".to_string(),
                        span,
                    ));
                }
                // The task closure crosses the airlock **by value** on BOTH engines
                // (`wire_callable` → `to_wire`: proto + deep-copied captures + home index), exactly
                // like plain `spawn` (`cross_spawn_callee`). This is the sole serial==M:N invariant:
                // routing coop through the SAME wire path runs the generator airlock
                // enforcement on the cooperative engine too, and isolates captures at submit time, so
                // serial and M:N behave identically for every submitted closure. (Earlier the coop
                // branch queued the callable's own `Handle` — captures shared by reference, bypassing
                // `to_wire` — to mirror the tree-walk `interp` oracle; that oracle has been removed, so
                // the by-handle preservation was pure serial-vs-M:N divergence and is retired.)
                // Under `--parallel` a pool thread runs the closure the moment it is submitted; under
                // the cooperative engine the inline drain (`from_wire` at `shutdown`) rebuilds an
                // isolated closure over this same heap home. Queued captures stay rooted via the
                // executor handle's `children()` (the `Closure` arm of `collect_core_gcrefs`) — which
                // on M:N now roots nothing, since nothing sits in the queue.
                let w = self.wire_callable(args[0], span)?;
                if self.parallel {
                    // EAGER (D1/D2): start the job NOW on the shared pool. The queue stays empty on
                    // this engine — the work lives in the core's `eager` slots instead.
                    //
                    // Building the worker happens with **NO executor lock held**, deliberately.
                    // `prepare_worker_from_wire` rebuilds the closure into the worker's heap, and a
                    // closure that captured this executor puts an `Obj::Executor` over THIS core into
                    // that heap — so a GC there marks the core, and `Heap`'s mark arm takes
                    // `core.inner.lock()`. `std::sync::Mutex` is not reentrant, so holding it across
                    // the rebuild would self-deadlock on `ex.submit(fn(): ex.…)` under GC pressure.
                    // Faulting here (`ensure_snapshot` on a frame-holding generator global) must also
                    // happen BEFORE a slot is reserved, or the reservation would leave `outstanding`
                    // permanently short and hang `shutdown` forever.
                    let rw = self.prepare_eager_job(&core, w, span)?;
                    // Now the atomic part: re-check `shut` and reserve the submission slot under ONE
                    // lock, so a job racing an `ex.shutdown()` is either rejected or is counted by
                    // that shutdown's join — never dispatched into a shut executor nobody waits for.
                    // Lock order is inner → eager; a finishing job takes only `eager`, so it can never
                    // contend here.
                    let g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)"
                                .to_string(),
                            span,
                        ));
                    }
                    crate::vm::sched::dispatch_eager_job(&core, rw);
                    drop(g);
                } else {
                    // `--serial` keeps queue-at-submit / drain-at-`shutdown` (decision D3).
                    let mut g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)"
                                .to_string(),
                            span,
                        ));
                    }
                    // Summarised under the executor's OWN lock (not the scheduler's) and right next
                    // to `wire_callable`'s much larger walk of the same closure — no hoist needed.
                    let sum = crate::vm::core::wire_summary(&w);
                    g.push(sum, w);
                }
                Ok(Value::nil())
            }
            "shutdown" => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                if self.parallel {
                    // EAGER (D1/D4): every job started at its `submit`, so there is no queue to
                    // drain — `shutdown` is purely the JOIN. Wait for in-flight work, then reduce the
                    // submission-ordered slots: output flushes in submission order and the
                    // lowest-index fault propagates, exactly as the drain did.
                    //
                    // W7-12 — the guard marks this core as being JOINED for the duration, which is
                    // what lets a job blocked on a channel only THIS caller could have filled fault
                    // instead of hanging both of us forever. Armed here, at the join the program
                    // actually wrote, and not inside `join_eager_jobs` — see [`JoinGuard`]. Armed
                    // ONLY when this caller is the whole program (`Vm::join_has_no_live_siblings`),
                    // because the verdict it unlocks is "nobody else can send".
                    let _joining = self
                        .join_has_no_live_siblings()
                        .then(|| JoinGuard::new(&core));
                    self.join_eager_jobs(&core)?;
                } else {
                    // Cooperative engine: inline FIFO drain. Root the executor handle across the drain
                    // (its remaining queue is traced via it); each popped task is rooted on the stack
                    // across its re-entrant call. A submitted task runs INLINE on the entry `Vm`, so
                    // it reads the one shared `host.stdin` — which is exactly the contract (the M:N
                    // drain's workers get the same source via `spawn_worker`).
                    //
                    // W7-5 — run EVERY queued job, then raise the FIRST fault in submission order.
                    // The loop breaks early only for a hard halt (`os.exit` via `pending_exit`, or
                    // `executor_hard_halt`), matching the M:N drain's cancel-flag rule exactly.
                    self.push(Value::obj(h));
                    let mut first_err: Option<RuntimeError> = None;
                    loop {
                        // Pop under the lock, then DROP the guard before the re-entrant call.
                        let task = core.inner.lock().unwrap().pop();
                        let Some(task) = task else { break };
                        // Task 1 — snapshot the module globals as of THIS DRAIN (`ensure_snapshot`,
                        // matching the M:N drain via `prepare_worker_from_wire`: both engines drain at the
                        // same program point, so the instant is parity-identical) and give this task its
                        // OWN deep-copied module-global view in the shared heap. Both the `from_wire` rebuild
                        // of the closure AND its `invoke_value` run under that view, so a module global
                        // mutated by the submitted task hits its private copy — invisible to the parent,
                        // `serial == M:N` by construction. `Shared`/`Atomic`/`Channel`/`Cffi` globals
                        // still cross by shared `Arc` (via `to_snap`), so the escape hatch is unaffected.
                        let snap = self.ensure_snapshot(span)?;
                        // gaps.md B4: the submitted task runs INLINE on this entry Vm, so its callee
                        // frames get captured into `fault_trace` while intact. We must drop ONLY the
                        // inline task's own frames — never a superseding OUTER fault already captured
                        // (e.g. `defer ex.shutdown()` while `main` is unwinding). So snapshot any
                        // pre-existing trace, give the task a clean capture slate, then restore the
                        // outer trace regardless of outcome. On the common `ex.shutdown()` path the
                        // outer trace is None, so the propagated fault re-captures at the shutdown
                        // call site (`at main`) in the enclosing `run_until` — matching M:N (which
                        // discards the isolated worker's `fault_trace`) and a plain nursery-task panic.
                        let outer_trace = self.fault_trace.take();
                        let outer_depth = std::mem::replace(&mut self.fault_trace_depth, 0);
                        let r = self.with_serial_child_modules(snap, |vm| {
                            let task = vm.from_wire(task);
                            vm.push(task);
                            let r = vm.guarded(|vm| vm.invoke_value(task, vec![], span));
                            vm.pop();
                            r
                        });
                        self.fault_trace = outer_trace;
                        self.fault_trace_depth = outer_depth;
                        if let Err(e) = r {
                            let hard = executor_hard_halt(&e);
                            // W7-5 review Fix 1: a hard halt must overwrite an earlier ordinary
                            // fault before breaking, or a later `--max-heap`/`--timeout` abort is
                            // silently discarded in favor of an earlier catchable fault — which lets
                            // `recover:` swallow a hard halt it must never be able to catch.
                            if hard {
                                first_err = Some(e);
                                break;
                            }
                            if first_err.is_none() {
                                first_err = Some(e);
                            }
                        }
                        if self.pending_exit.is_some() {
                            break; // a drained job called os.exit — hard halt
                        }
                    }
                    self.pop(); // the executor root
                    if let Some(e) = first_err {
                        return Err(e);
                    }
                }
                Ok(Value::nil())
            }
            "shutdown_now" => {
                self.arity_err("shutdown_now", args, 0, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    g.shut = true;
                    g.clear(); // the serial queue; empty on M:N (eager submit never fills it)
                }
                if self.parallel {
                    // D4 — "attempts to stop", COOPERATIVE not preemptive: trip the per-core cancel
                    // flag so a job already running dies at its next back-edge, and one the pool has
                    // not started yet observes it in its prologue. A job with no cancellation point
                    // still runs to completion; that is Java's contract too.
                    core.cancel
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    // …then JOIN, exactly like `shutdown`. Java's `shutdownNow` returns without
                    // waiting because it hands back the never-started tasks and you follow up with
                    // `awaitTermination`; Chezzi has no such follow-up call, so not waiting here
                    // would leave detached jobs running past a `shut` executor that the program-exit
                    // join deliberately SKIPS (`drain_live_executors` short-circuits on `shut`) — the
                    // program could exit mid-job. Cancelled jobs are swallowed by `reduce_task_slots`
                    // (their output still flushes at their slot), so this raises a fault only if a
                    // job genuinely faulted before the trip landed.
                    self.join_eager_jobs(&core)?;
                }
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Executor has no method '{method}'"), span)),
        }
    }

    /// Mirrors `interp::Interp::drain_live_executors` (C5 / A2): at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`/`shutdown_now`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, run-all — every queued job runs and the
    /// lowest-submission-index fault propagates, W7-5). A hard `std.os.exit` is not drained (the
    /// caller gates on `pending_exit`); a task that calls `os.exit` mid-drain stops the remaining
    /// drain.
    pub(super) fn drain_live_executors(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        if self.parallel {
            // EAGER (D1) — the executor is DETACHED: its work is already running, and this is where
            // the program waits for it. Walk the heap-independent registry, not `self.executors`:
            // that list is heap-keyed, so an executor created inside a task never reached it and its
            // work was silently lost (W7-5b). Creation order, matching the serial reap below.
            //
            // Re-scan from the top each round rather than snapshotting: a job joined here can itself
            // construct and submit to a NEW executor, which must also be joined. Marking `shut`
            // before joining is what makes that terminate — a joined executor is never re-picked.
            loop {
                let next = self
                    .exec_registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .find(|c| !c.inner.lock().unwrap_or_else(|e| e.into_inner()).shut)
                    .map(Arc::clone);
                let Some(core) = next else { break };
                core.inner.lock().unwrap_or_else(|e| e.into_inner()).shut = true;
                self.join_eager_jobs(&core)?;
                if self.pending_exit.is_some() {
                    break; // a joined job called os.exit — hard halt, stop joining
                }
            }
            return Ok(());
        }
        // `--serial` (D3) still queues at `submit` and drains at the reap point, through the HANDLE
        // (the inline drain re-roots it on the operand stack), so it keeps the handle list. Same
        // re-scan shape as the M:N join above, and for the same reason: a job run BY this drain can
        // itself create and submit to a new executor, and that one owes the same reap. This used to
        // iterate a SNAPSHOT of the list taken before the drain, so such an executor was missed and
        // its work silently vanished — pre-existing on both engines, and it would have become a live
        // serial-vs-M:N divergence the moment the M:N side learned to re-scan. Terminates because
        // `shutdown` marks the core `shut` before running anything, so a reaped executor is never
        // re-picked.
        loop {
            let next = {
                let mut found = None;
                for &h in &self.executors {
                    if !self.executor_core(h).inner.lock().unwrap().shut {
                        found = Some(h);
                        break;
                    }
                }
                found
            };
            let Some(h) = next else { break };
            self.executor_method(h, "shutdown", &[], span)?;
            if self.pending_exit.is_some() {
                break; // a drained task called os.exit — hard halt, stop draining
            }
        }
        Ok(())
    }
}
