//! `std.net` — minimal non-blocking TCP surface (D6).
//!
//! Unlike the other native modules, `std.net`'s members are **not** plain off-heap natives: a
//! `connect`/`listen` allocates a `Socket`/`Listener` *handle* (a heap object over an `Arc`'d core)
//! and the socket methods (`read`/`write`/`accept`) register interest with the netpoller and park the
//! fiber on a would-block — all of which need the VM (`&mut Vm` + the scheduler). So the engine
//! **intercepts** them: `connect`/`listen` carry [`Kind::InterceptNet`] on their `MEMBERS` entry and
//! `Vm::invoke_native` runs them itself, and the socket METHODS are intercepted by name in
//! `Vm::do_method_call` (a method is not a module member, so it has no registry entry). The `MEMBERS`
//! entries below exist only so the module member *resolves* (the placeholder fns never run). This
//! module holds just the pure, `Vm`-free socket helpers the VM calls.
//!
//! **Timeouts (D6c).** `read`/`write`/`accept` take an OPTIONAL trailing `timeout_ms: int`
//! (`conn.read(n, timeout_ms)`, `sock.write(s, timeout_ms)`, `server.accept(timeout_ms)`): if no
//! data / writability / connection arrives within `timeout_ms`, the op returns the existing
//! `Result::Err` variant with message `"timeout"` (the return type is unchanged). `0` polls once
//! (never parks); a negative saturates to `0`. This is a worker-shell-fiber feature (the fiber parks
//! on the netpoller with a deadline); off a worker shell — top-level `main`, which has no fiber to
//! park and already fails loud on would-block — a non-zero `timeout_ms` is moot (the op never parks
//! there, so it can't time out), though a `timeout_ms == 0` would-block still surfaces `Err("timeout")`
//! since the poll-once check precedes the engine gate. Either way the top-level result is an `Err`.

use super::{Host, HostError, Kind, NativeFn, NativeRet};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};

/// Start a **non-blocking** TCP connect to `addr` (a `"host:port"` string). Returns the connecting
/// stream plus whether the handshake is still in flight: `(_, false)` ⇒ connected synchronously (the
/// common loopback case — ready to use); `(_, true)` ⇒ in progress (`EINPROGRESS`), the caller must
/// park the fiber on **writability** and then call [`finish_connect`] to read the result. The returned
/// stream is non-blocking, so a later `read`/`write` that would block also parks on the netpoller
/// rather than pinning a worker. (Address resolution itself is a brief blocking step — instant for the
/// literal IPs `std.net` is used with; a true async resolver is out of scope for D6b.)
pub fn connect_nonblocking(addr: &str) -> std::io::Result<(TcpStream, bool)> {
    let target = resolve(addr)?;
    let domain = if target.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_nonblocking(true)?;
    let in_progress = match socket.connect(&SockAddr::from(target)) {
        Ok(()) => false,
        // Non-blocking connect that hasn't finished: `WouldBlock` (Windows) or `EINPROGRESS` (unix).
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        #[cfg(unix)]
        Err(ref e) if e.raw_os_error() == Some(libc::EINPROGRESS) => true,
        Err(e) => return Err(e),
    };
    Ok((socket.into(), in_progress))
}

/// Complete an in-progress non-blocking connect after the socket reported **writable**: `SO_ERROR` is
/// cleared (`None`) on a successful handshake, or carries the connection error (e.g. refused). Reading
/// it via `take_error` is the canonical "did the async connect succeed?" check.
pub fn finish_connect(stream: &TcpStream) -> std::io::Result<()> {
    match socket2::SockRef::from(stream).take_error()? {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Resolve a `"host:port"` string to a single socket address (first resolved entry).
fn resolve(addr: &str) -> std::io::Result<std::net::SocketAddr> {
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no address resolved"))
}

/// Bind a non-blocking accepting socket to `addr` (a `"host:port"` string). Non-blocking, so an
/// `accept` with no pending connection parks the fiber rather than blocking the worker.
pub fn listen_nonblocking(addr: &str) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Placeholder for an engine-intercepted member: `Vm::invoke_native` handles `std.net.connect` /
/// `listen` directly (they allocate a handle + touch the scheduler), so this body must never run.
fn intercepted(_h: &mut dyn Host) -> Result<NativeRet, HostError> {
    Err(HostError {
        message: "std.net members are handled by the engine and must not run as off-heap natives"
            .into(),
    })
}

/// Callable members. `(name, fn, kind)`. Both are intercepted in the VM (see module docs); the entries
/// exist so the member resolves to an `Obj::Native` carrying the [`Kind::InterceptNet`] the
/// interception keys on.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("connect", intercepted, Kind::InterceptNet),
    ("listen", intercepted, Kind::InterceptNet),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    /// `listen_nonblocking` binds and leaves the listener non-blocking (an idle `accept` returns
    /// `WouldBlock` rather than blocking — the property the netpoller relies on).
    #[test]
    fn listen_binds_non_blocking() {
        let listener = listen_nonblocking("127.0.0.1:0").unwrap();
        // No pending connection ⇒ a non-blocking accept must return WouldBlock immediately.
        match listener.accept() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("expected WouldBlock on an idle non-blocking accept, got {other:?}"),
        }
    }

    /// `connect_nonblocking` reaches a live listener and yields a non-blocking stream that, once the
    /// handshake settles (immediately or after `finish_connect`), is connected — and silent-peer reads
    /// would-block rather than blocking.
    #[test]
    fn connect_reaches_a_live_listener() {
        let listener = listen_nonblocking("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (stream, in_progress) = connect_nonblocking(&addr.to_string()).unwrap();
        if in_progress {
            wait_connected(&stream);
            finish_connect(&stream).expect("loopback handshake succeeds");
        }
        assert!(
            stream.peer_addr().is_ok(),
            "stream is connected after the handshake settles"
        );
        // A non-blocking read on a connected-but-silent peer returns WouldBlock, not 0/blocked.
        let mut buf = [0u8; 1];
        match std::io::Read::read(&mut (&stream), &mut buf) {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("expected WouldBlock reading a silent peer, got {other:?}"),
        }
    }

    /// The async-connect error path: connecting to a port with no listener completes (via writability)
    /// with `finish_connect` reporting the refusal — not a hang, not a panic.
    #[test]
    fn connect_to_dead_port_reports_refused() {
        // Bind then drop to obtain a port nothing is listening on.
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let (stream, _in_progress) = connect_nonblocking(&dead.to_string()).unwrap();
        // Poll the SO_ERROR until the handshake settles; loopback refusal settles within the budget.
        let mut settled: Option<std::io::Result<()>> = None;
        for _ in 0..500 {
            match finish_connect(&stream) {
                Ok(()) if stream.peer_addr().is_ok() => {
                    settled = Some(Ok(()));
                    break;
                }
                Ok(()) => {} // not settled yet (SO_ERROR still clear, not yet connected)
                Err(e) => {
                    settled = Some(Err(e));
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        match settled {
            Some(Err(_)) => {} // expected: connection refused
            other => panic!("expected a connection error on a dead port, got {other:?}"),
        }
    }

    /// A bad address is a clean error, not a panic.
    #[test]
    fn bad_address_errors() {
        assert!(listen_nonblocking("not-an-address").is_err());
        assert!(connect_nonblocking("not-an-address").is_err());
    }

    /// Spin until a non-blocking connect completes (its peer address becomes available). Loopback
    /// settles in microseconds; the budget only guards against a broken handshake.
    fn wait_connected(stream: &TcpStream) {
        for _ in 0..500 {
            if stream.peer_addr().is_ok() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("non-blocking connect did not complete within budget");
    }
}
