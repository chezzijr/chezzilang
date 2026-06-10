//! `std.net` — minimal non-blocking TCP surface (D6).
//!
//! Unlike the other native modules, `std.net`'s members are **not** plain off-heap natives: a
//! `connect`/`listen` allocates a `Socket`/`Listener` *handle* (a heap object over an `Arc`'d core)
//! and the socket methods (`read`/`write`/`accept`) register interest with the netpoller and park the
//! fiber on a would-block — all of which need the VM (`&mut Vm` + the scheduler). So the engine
//! **intercepts** these by name in `Vm::invoke_native` / `Vm::do_method_call`; the `MEMBERS` entries
//! below exist only so the module member *resolves* (the placeholder fns never run). This module holds
//! just the pure, `Vm`-free socket helpers the VM calls.

use super::{Host, HostError, NativeFn, NativeRet};
use std::net::{TcpListener, TcpStream};

/// Connect a non-blocking TCP stream to `addr` (a `"host:port"` string). The TCP handshake itself is
/// done blocking here (v1 — a non-blocking connect parked on writability is deferred to D6b); for a
/// local peer this is instant. The returned stream is non-blocking, so the first `read`/`write` that
/// would block parks the fiber on the netpoller instead of pinning a worker.
pub fn connect_nonblocking(addr: &str) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect(addr)?;
    stream.set_nonblocking(true)?;
    Ok(stream)
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
        message: "std.net members are handled by the engine and must not run as off-heap natives".into(),
    })
}

/// Callable members. `connect`/`listen` are intercepted in the VM (see module docs); the entries
/// exist only so the member resolves to an `Obj::Native` the interception keys on by name.
pub const MEMBERS: &[(&str, NativeFn)] = &[("connect", intercepted), ("listen", intercepted)];

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

    /// `connect_nonblocking` reaches a live listener and yields a non-blocking stream.
    #[test]
    fn connect_reaches_a_live_listener() {
        let listener = listen_nonblocking("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = connect_nonblocking(&addr.to_string()).unwrap();
        // A non-blocking read on a connected-but-silent peer returns WouldBlock, not 0/blocked.
        let mut buf = [0u8; 1];
        match std::io::Read::read(&mut (&stream), &mut buf) {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("expected WouldBlock reading a silent peer, got {other:?}"),
        }
    }

    /// A bad address is a clean error, not a panic.
    #[test]
    fn bad_address_errors() {
        assert!(listen_nonblocking("not-an-address").is_err());
    }
}
