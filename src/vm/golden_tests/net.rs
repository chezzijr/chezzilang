//! Golden tests: sockets and net timeouts under a watchdog.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

/// D6c — `conn.read(n, timeout_ms)` returns `Err("timeout")` when the peer accepts but never
/// writes. The server accepts the connection and then sleeps past the client's read timeout; the
/// client's `read(64, 100)` parks on the netpoller with a 100 ms deadline, the deadline fires
/// before any data, and the rewound op returns `Err` with `e.message() == "timeout"`.
#[test]
fn read_timeout_returns_err() {
    let src = "\
import std.net
import std.time

fn server(listener: net.Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(400)
    conn.close()
    listener.close()
    return ?0

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64, 100):
        ?s: print(\"GOT:\" + s)
        !e: print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print(\"\")
        !e: print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "read(64, 100) must surface !\"timeout\": {out:?}"
    );
    assert!(
        !out.contains("GOT:"),
        "no data should have been read: {out:?}"
    );
}

/// B1 — a `read(n)` whose chunk ends mid-codepoint must NOT corrupt the text. The client reads a
/// multibyte payload ONE BYTE AT A TIME (the ordinary read-in-a-loop idiom): the incomplete tail is
/// carried on the socket core and prepended to the next read, so the reassembly is byte-exact.
/// Before the fix `String::from_utf8_lossy` turned `é` into two U+FFFD.
#[test]
fn net_read_reassembles_split_codepoint_over_parallel() {
    let src = "\
import std.net

fn serve(server: net.Listener) -> int!:
    conn := server.accept()?
    conn.write(\"héllo\")?
    conn.close()
    server.close()
    return ?0

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    acc := \"\"
    while true:
        chunk := sock.read(1)?
        if chunk == \"\":
            break
        acc = acc + chunk
    print(\"GOT:\" + acc)
    sock.close()
    return ?0

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("split_codepoint", src);
    assert!(
        out.contains("GOT:héllo"),
        "a byte-at-a-time read must reassemble the multibyte text exactly: {out:?}"
    );
    assert!(
        !out.contains('\u{fffd}'),
        "no replacement chars — the seam must never lossily decode: {out:?}"
    );
}

/// B1 — a genuinely-invalid UTF-8 byte on the wire (a lone `0xFF` ⇒ `Utf8Error::error_len() ==
/// Some(1)`) must surface a clear, recoverable `Err` naming the str-only limitation, NOT a silent
/// U+FFFD. Peer is a raw Rust `TcpListener` so we can put arbitrary bytes on the wire.
///
/// Review #2/#5 — and the `Err` must NOT SHRED the stream. The first cut dropped the WHOLE chunk it
/// had already taken off the fd (`carry.clear()`), so the valid text before the bad byte (`"hi"`) and
/// everything after it vanished — a recoverable `Err` that silently eats up to `MAX_SOCKET_READ` of
/// payload is the same data-loss family B1 exists to kill. Now: the valid prefix is DELIVERED, the
/// undecodable remainder STAYS carried, and the `Err` is STICKY (a str-only seam can never hand those
/// bytes back, so every later read re-decodes them and re-errs identically — no byte is ever silently
/// consumed-and-dropped, and a log-and-continue caller cannot shred the stream).
#[test]
fn net_read_invalid_utf8_errs_not_replacement_chars() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"hi\xFFthere").unwrap();
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    i := 0
    while i < 3:
        match sock.read(64):
            ?s: print(\"GOT:[\" + s + \"]\")
            !e: print(\"ERR:\" + e.message())
        i = i + 1
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("invalid_utf8", &src);
    server.join().unwrap();
    assert!(
        out.contains("invalid utf-8 on the socket"),
        "an invalid byte sequence must surface the str-only Err: {out:?}"
    );
    assert!(
        out.contains("read_bytes"),
        "the Err must point at the real limitation/future path: {out:?}"
    );
    assert!(
        out.contains("GOT:[hi]"),
        "the valid text BEFORE the bad byte must still be delivered, never swallowed by the Err: \
         {out:?}"
    );
    assert!(
        out.matches("invalid utf-8 on the socket").count() >= 2,
        "the Err is sticky — the undecodable bytes stay carried, so a log-and-continue caller \
         re-errs instead of silently shredding the stream: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #1) — `read(0)` on a CLOSED socket must still be `Err("read on a closed socket")`. The
/// first cut early-returned `Ok("")` for `n == 0` BEFORE taking the stream lock, and the `guard.as_mut()`
/// `None` arm is the ONLY closed-socket detector on the read path — so `read(0)` (or any caller-computed
/// `read(want - have)` that lands on 0) silently converted a recoverable error into a value that is
/// indistinguishable from the EOF sentinel.
#[test]
fn net_read_zero_on_closed_socket_errs() {
    use std::net::TcpListener;

    // Bound-but-never-accepted: the connect completes out of the listen backlog, so no peer thread is
    // needed. `listener` is held alive for the whole test.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    sock.close()
    match sock.read(0):
        ?s: print(\"OK:[\" + s + \"]\")
        !e: print(\"ERR:\" + e.message())
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("read_zero_closed", &src);
    drop(listener);
    assert!(
        out.contains("ERR:read on a closed socket"),
        "read(0) must not mask the closed-socket Err as a success indistinguishable from EOF: {out:?}"
    );
}

/// B1 (review #3/#8) — a POLL-ONCE read (`read(n, 0)`) that DID take bytes off the fd but landed
/// mid-codepoint must not report `Err("timeout")`. `timeout` is documented as "no data within
/// timeout_ms"; here data arrived, it just did not complete a codepoint — and 1–3 bytes were
/// permanently removed from the wire into the socket's carry. The distinct `Err("incomplete utf-8:
/// …")` names what actually happened and says the bytes are retained, so a retry on the same socket
/// recovers them byte-exactly (asserted: the full `"éllo"` still arrives).
#[test]
fn net_read_poll_once_mid_codepoint_errs_incomplete_not_timeout() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // The lead byte of `é` alone, then a stall — the poll-once read takes it and can complete
        // nothing.
        stream.write_all(b"\xC3").unwrap();
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        stream.write_all(b"\xA9llo").unwrap();
        stream.flush().unwrap();
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    acc := \"\"
    seen := 0
    while true:
        match sock.read(8, 0):
            ?s:
                if s == \"\":
                    break
                acc = acc + s
            !e:
                m := e.message()
                if m != \"timeout\" and seen == 0:
                    print(\"ERR:\" + m)
                    seen = 1
    print(\"ACC:\" + acc)
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("poll_once_mid_codepoint", &src);
    server.join().unwrap();
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "a poll-once read that consumed bytes must not lie about it as a deadline expiry: {out:?}"
    );
    assert!(
        out.contains("ACC:éllo"),
        "the carried lead byte is retained across the Err and recovered by the next read: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #7) — `timeout_ms` must bound the WHOLE `read` call on the DEMOTE path too. A `read`
/// reached inside a native callback (`native_reentry > 0` — here a `list.map`) cannot snapshot-park
/// onto the netpoller, so it demotes to [`Vm::demote_block_socket`] — which took no deadline and
/// looped on `wait_fd_ready` until readiness/cancel/terminate. Once B1 routed a mid-codepoint read
/// back into that wait, `read(8, 200)` against a peer that sends the lead byte and STALLS blocked
/// indefinitely: the demoted op is accounted `inflight`, which VETOES the deadlock predicate, so it
/// hung with no fault and no `Err("timeout")` — while `docs/stdlib.md` promised the deadline bounds
/// the whole call. The deadline is now threaded into the demote loop.
#[test]
fn net_read_timeout_bounds_the_in_callback_demote_path() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"\xC3").unwrap(); // a lone lead byte…
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2)); // …then stall, far past the 200 ms budget
    });

    let src = format!(
        "\
import std.net

fn grab(s: net.Socket) -> str:
    match s.read(8, 200):
        ?v: return \"GOT:\" + v
        !e: return \"ERR:\" + e.message()

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    outs := [sock].map(grab)
    print(outs[0])
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("demote_read_timeout", &src);
    server.join().unwrap();
    // N3(a) — the demote loop took the lead byte `\xC3` off the wire before timing out, so its
    // timeout reports `Err("incomplete utf-8: …")` (the poll-once classification), NOT `Err("timeout")`
    // which is documented as "nothing arrived". The partial is retained on the socket for a retry.
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "the in-callback demote loop must honour timeout_ms AND classify a taken-partial timeout as \
         incomplete-utf-8, not 'timeout' (nothing arrived): {out:?}"
    );
}

/// A Rust peer that retry-dials `addr` (10 ms apart, ~10 s cap — so it cannot lose the race against
/// the chezzi `listen`), waits 50 ms so the chezzi side is provably BLOCKED in `accept`, sends
/// `"hi"`, and reads the echo back. Shared by the block-in-place tests below.
fn net_peer(addr: std::net::SocketAddr) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let mut s = None;
        for _ in 0..500 {
            match std::net::TcpStream::connect(addr) {
                Ok(c) => {
                    s = Some(c);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        // Never panic here — the chezzi-side stdout is the diagnostic that matters, and a peer panic
        // would poison the `join` and hide it. Report the failure as the echo string instead.
        let Some(mut s) = s else {
            return "<peer never reached the chezzi listener>".into();
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        if s.write_all(b"hi").and_then(|()| s.flush()).is_err() {
            return "<peer write failed>".into();
        }
        let mut buf = [0u8; 64];
        match s.read(&mut buf) {
            Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
            Err(e) => format!("<peer read failed: {e}>"),
        }
    })
}

/// Bind a Rust listener only to LEARN a free port, then drop it so chezzi can take the address.
fn free_addr() -> std::net::SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

/// W7 — a would-block socket op reached where the fiber CANNOT park (top-level `main`: `Vm::mn` is
/// `None`, because `mn` is set only on an M:N worker shell) must BLOCK IN PLACE, not return
/// `Err("accept would block: an Executor job doesn't own its thread …")` — which it did even
/// while running the default M:N engine, making the hello-world TCP server unwritable.
///
/// **Go is the ancestor and blocks.** `ln, _ := net.Listen("tcp","127.0.0.1:0")` on the MAIN
/// goroutine, a `go func()` dialing after 50 ms, `ln.Accept()` on main:
/// ```text
/// [   0.1ms] listening on 127.0.0.1:38541 (main goroutine)
/// [   0.1ms] calling ln.Accept() on the main goroutine...
/// [  50.5ms] dialed
/// [  50.5ms] Accept() returned: 127.0.0.1:58036
/// [  50.5ms] Read() -> "hi" err=<nil>
/// ```
/// Accept blocks for the full 50 ms and succeeds. CPython's `socket.accept()` on the main thread is
/// the same.
///
fn accept_from_main_blocks() {
    let addr = free_addr();
    let peer = net_peer(addr);
    let src = format!(
        "\
import std.net

fn serve() -> int!:
    l := net.listen(\"{addr}\")?
    print(\"listening\")
    conn := l.accept()?
    msg := conn.read(64)?
    print(\"GOT:\" + msg)
    conn.write(\"echo:\" + msg)?
    conn.close()
    l.close()
    return ?0

fn main():
    match serve():
        ?_: print(\"done\")
        !e: print(\"ERR:\" + e.message())

main()
"
    );
    let out = run_net_watchdog_engine("accept_from_main", &src);
    assert_eq!(out, "listening\nGOT:hi\ndone\n");
    assert_eq!(peer.join().unwrap(), "echo:hi");
}

#[test]
fn accept_from_main_blocks_instead_of_erroring_mn() {
    accept_from_main_blocks();
}

/// The `read_bytes` arm of the same fix — the fourth op, covered directly rather than by symmetry.
#[test]
fn read_bytes_from_main_blocks_instead_of_erroring() {
    let addr = free_addr();
    let peer = net_peer(addr);
    let src = format!(
        "\
import std.net

fn serve() -> int!:
    l := net.listen(\"{addr}\")?
    print(\"listening\")
    conn := l.accept()?
    b := conn.read_bytes(64)?
    print(\"GOT:\" + str(b.len()))
    conn.write(\"echo:hi\")?
    conn.close()
    l.close()
    return ?0

fn main():
    match serve():
        ?_: print(\"done\")
        !e: print(\"ERR:\" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("read_bytes_from_main", &src);
    assert_eq!(out, "listening\nGOT:2\ndone\n");
    assert_eq!(peer.join().unwrap(), "echo:hi");
}

/// B1 — an incomplete codepoint left over when the peer CLOSES (`b"ok\xC3"` ⇒ `error_len() == None`,
/// then EOF) is a real error, never a silent drop and never U+FFFD. The valid prefix is still
/// delivered; the dangling lead byte errors on the read that sees the close.
#[test]
fn net_read_incomplete_tail_at_eof_errs() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"ok\xC3").unwrap();
        stream
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown the peer");
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    while true:
        match sock.read(8):
            ?s:
                if s == \"\":
                    break
                print(\"GOT:\" + s)
            !e:
                print(\"ERR:\" + e.message())
                break
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("eof_tail", &src);
    server.join().unwrap();
    assert!(
        out.contains("GOT:ok"),
        "the valid prefix is delivered: {out:?}"
    );
    assert!(
        out.contains("ERR:invalid utf-8 at eof"),
        "a dangling partial codepoint at EOF must error, not be dropped: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #1/#4/#6) — a `read(0)` while a partial codepoint is CARRIED must return `Ok("")` at
/// once, never spin. `read(2)` over `"héllo"` hands back `"h"` and carries the `0xC3` lead byte; a
/// following `read(0)` (the caller-computed `read(want - have)` that lands on 0 is the real-world
/// shape) reads a ZERO-length buffer — which `Read::read` answers `Ok(0)` for unconditionally, so the
/// decode-retry loop could neither progress nor would-block: it spun at 100% CPU forever, un-cancelable
/// and un-timeout-able. The carry must SURVIVE the no-op read (the next `read` still gets `"éllo"`).
#[test]
fn net_read_zero_with_pending_carry_returns_empty_not_spin() {
    let src = "\
import std.net

fn serve(server: net.Listener) -> int!:
    conn := server.accept()?
    conn.write(\"héllo\")?
    conn.close()
    server.close()
    return ?0

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    first := sock.read(2)?          # \"h\" — the 0xC3 lead byte of \"é\" is carried
    print(\"FIRST:\" + first)
    zero := sock.read(0)?           # must NOT spin: no bytes wanted, carry untouched
    print(\"ZERO:[\" + zero + \"]\")
    rest := sock.read(64)?          # the carry is still owed — \"éllo\"
    print(\"REST:\" + rest)
    sock.close()
    return ?0

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_zero_carry", src);
    assert!(out.contains("FIRST:h"), "{out:?}");
    assert!(
        out.contains("ZERO:[]"),
        "read(0) is a no-op ?\"\", not a spin and not a false EOF error: {out:?}"
    );
    assert!(
        out.contains("REST:éllo"),
        "the carried lead byte survives the read(0) and is prepended to the next read: {out:?}"
    );
}

/// B1 (review #2/#5/#7) — TWO fibers reading ONE shared `Socket` (an `Arc`'d core — the `spawn
/// handle(conn)` idiom) must decode in WIRE order. The fd read and the carry update are one critical
/// section (carry lock OUTER); when they were split, fiber B could take the continuation bytes off the
/// fd and decode them BEFORE fiber A stored the lead byte it had taken — a leading continuation byte is
/// `error_len() == Some(1)`, so a perfectly valid UTF-8 text stream errored as `"invalid utf-8 on the
/// socket"` (and A's stale carry then poisoned the next read). Both readers poll-once (`read(n, 0)`) so
/// neither ever parks (a second PARKED op on a shared socket is a separate, deliberate fault).
#[test]
fn net_read_shared_socket_two_fibers_decode_in_wire_order() {
    let src = "\
import std.net
import Shared from std.concurrency

fn serve(server: net.Listener) -> int!:
    conn := server.accept()?
    i := 0
    while i < 300:
        conn.write(\"é\")?
        i = i + 1
    conn.close()
    server.close()
    return ?0

fn drain(sock: net.Socket, seen: Shared[int], bad: Shared[int]) -> int!:
    while seen.get() < 300 and bad.get() == 0:
        match sock.read(3, 0):
            ?s:
                got := s.chars().len()
                if got > 0:
                    seen.update(fn(v: int) -> int: v + got)
            !e:
                # \"timeout\" (nothing ready) and \"incomplete utf-8\" (this poll took a partial
                # codepoint — retained, the next read finishes it) are both benign poll-once retry
                # signals. \"invalid utf-8\" is THE bug this test guards: valid text mis-decoded
                # because two fibers took bytes out of wire order.
                if e.message().starts_with(\"invalid utf-8\"):
                    print(\"ERR:\" + e.message())
                    bad.update(fn(v: int) -> int: v + 1)
    return ?0

fn run() -> int!:
    server := net.listen(\"127.0.0.1:0\")?
    addr := server.addr()?
    seen := Shared(0)
    bad := Shared(0)
    parallel:
        spawn serve(server)
        spawn:
            sock := net.connect(addr)?
            parallel:
                spawn drain(sock, seen, bad)
                spawn drain(sock, seen, bad)
            sock.close()
            return ?0
    print(\"SEEN:\" + str(seen.get()))
    print(\"BAD:\" + str(bad.get()))
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
";
    let out = run_net_timeout_watchdog("shared_socket_carry", src);
    assert!(
        !out.contains("invalid utf-8"),
        "valid multibyte text must never error, whichever fiber takes which bytes: {out:?}"
    );
    assert!(
        out.contains("SEEN:300") && out.contains("BAD:0"),
        "every codepoint is delivered exactly once across the two readers: {out:?}"
    );
    assert!(!out.contains('\u{fffd}'), "no replacement chars: {out:?}");
}

/// B1 (review #3/#8) — `timeout_ms` bounds the WHOLE `read` call, not one park. A park rewinds `ip` and
/// re-executes the op, so the deadline used to be recomputed (`now + timeout_ms`) on every wake: a peer
/// that dribbles ONE byte of a multibyte codepoint per (timeout - ε) kept re-arming the budget and the
/// `read` never timed out. The deadline is now latched on the fiber (`Vm::poll_deadline`), so it fires.
/// Peer: a raw `TcpListener` sending the 4 bytes of `😀` 300 ms apart; `read(64, 400)` must `Err`.
#[test]
fn net_read_timeout_bounds_whole_call_across_codepoint_parks() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for b in "😀".as_bytes() {
            // The client hangs up on the timeout (that IS the fix), so a later byte can hit EPIPE.
            if stream
                .write_all(&[*b])
                .and_then(|()| stream.flush())
                .is_err()
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(64, 400):
        ?s: print(\"GOT:\" + s)
        !e: print(\"ERR:\" + e.message())
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("read_timeout_midcodepoint", &src);
    server.join().unwrap();
    // N3(a) — the read parked having taken 1-3 bytes of the emoji off the wire, so its latched-deadline
    // timeout classifies as `Err("incomplete utf-8: …")` (the poll-once classification), not the
    // `Err("timeout")` that means "nothing arrived". Still proves the deadline FIRED (both are only
    // reachable on the timeout path) — i.e. the budget did not re-arm at every park.
    assert!(
        out.contains("ERR:incomplete utf-8"),
        "a 400 ms read must time out against a 300 ms-per-byte dribbler (not re-arm its budget at \
         every park) AND classify the taken-partial timeout as incomplete-utf-8: {out:?}"
    );
}

/// N2 (B1 residual) — `write(s, timeout_ms)` honours the deadline when the send buffer is full. The
/// server accepts then never reads, so the client's writes fill the kernel send buffer and the next
/// `write` finds it full, parks on writability, and its deadline fires → `Err("timeout")`. This also
/// exercises the new `poll_deadline` LATCH + `drop_poll_latch` clear on the write path (N2): the
/// deadline is registered through the same fiber latch as `read`, and cleared on completion so the
/// following op gets a fresh budget. (A `write` sends every byte before `Ok` (W15-3), so a buffer-full
/// write parks and re-parks until its deadline; this test pins the ordinary timeout path.)
#[test]
fn net_write_timeout_when_buffer_full() {
    let src = "\
import std.net
import std.time

fn server(listener: net.Listener) -> int!:
    _ := listener.accept()?
    time.sleep_ms(3000)
    listener.close()
    return ?0

fn client(addr: str):
    sock := net.connect(addr)?
    payload := \"x\".repeat(4000000)
    total := 0
    i := 0
    while i < 200:
        match sock.write(payload, 300):
            ?n: total = total + n
            !e:
                print(\"ERR:\" + e.message())
                break
        i = i + 1
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("write_timeout_full", src);
    assert!(
        out.contains("ERR:timeout"),
        "a write(_, 300) against a full send buffer must surface !\"timeout\": {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined (no hang): {out:?}"
    );
}

/// N3(a) stale-latch guard — the taken-partial flag (`Vm::poll_partial`) that makes a mid-codepoint
/// timeout report `incomplete utf-8` MUST be cleared on op completion, or it corrupts the NEXT read.
/// First `read(8, 200)` takes the lone lead byte `\xC3` and times out → `Err("incomplete utf-8")`.
/// The lead byte stays carried; a SECOND `read(8, 200)` takes NOTHING new off the wire (the peer is
/// still stalled) and must report a plain `Err("timeout")` — proving `poll_partial` was cleared, not
/// left `Some` from the first read (which would wrongly say "incomplete" again).
#[test]
fn net_read_partial_timeout_then_clean_timeout_is_not_incomplete() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"\xC3").unwrap(); // one lone lead byte…
        stream.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2)); // …then stall past both budgets
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(8, 200):
        ?s: print(\"A-OK:\" + s)
        !e: print(\"A:\" + e.message())
    match sock.read(8, 200):
        ?s: print(\"B-OK:\" + s)
        !e: print(\"B:\" + e.message())
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error:\" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("partial_then_clean_timeout", &src);
    server.join().unwrap();
    assert!(
        out.contains("A:incomplete utf-8"),
        "the first read took the lead byte then timed out → incomplete utf-8: {out:?}"
    );
    assert!(
        out.contains("B:timeout"),
        "the second read took nothing new → plain timeout (poll_partial was cleared): {out:?}"
    );
    assert!(
        !out.contains("B:incomplete"),
        "a stale poll_partial must not make the second read lie about a partial: {out:?}"
    );
}

/// D6c — `server.accept(timeout_ms)` returns `Err("timeout")` when NO client ever connects, and the
/// program terminates (no hang). The lone acceptor parks on the netpoller with a deadline; the
/// deadline fires, the rewound `accept` returns `Err("timeout")`, and the nursery joins.
#[test]
fn accept_timeout_returns_err() {
    let src = "\
import std.net

fn server(listener: net.Listener):
    match listener.accept(100):
        ?_: print(\"ACCEPTED\")
        !e: print(\"ERR:\" + e.message())
    listener.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    parallel:
        spawn server(listener)
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("accept_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "accept(100) with no client must surface !\"timeout\": {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued (no hang): {out:?}"
    );
    assert!(!out.contains("ACCEPTED"), "nothing was accepted: {out:?}");
}

/// D6c regression — a `read(n)` with NO timeout still parks FOREVER (until data arrives): the
/// timeout machinery must not have made the untimed read return early. The server sleeps well past
/// any plausible deadline before writing; the client's untimed `read(64)` must wait for the bytes
/// (not time out) and print them.
#[test]
fn read_without_timeout_still_parks_forever() {
    let src = "\
import std.net
import std.time

fn server(listener: net.Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(300)
    conn.write(\"late\")?
    conn.close()
    listener.close()
    return ?0

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64):
        ?s: print(\"GOT:\" + s)
        !e: print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print(\"\")
        !e: print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_no_timeout", src);
    assert!(
        out.contains("GOT:late"),
        "an untimed read must block until data, not time out: {out:?}"
    );
    assert!(
        !out.contains("ERR:"),
        "no timeout/error on the untimed read: {out:?}"
    );
}

/// D6c — the bundled `examples/socket_timeout.chz` golden: a `--parallel` program that demonstrates
/// both an `accept(timeout_ms)` and a `read(n, timeout_ms)` timeout branch, run end-to-end against
/// its `.expected` output. Net examples need `--parallel` (fibers only park on the netpoller inside
/// a nursery), so — like `echo_server.chz` — this is exercised here directly rather than via a plain
/// golden-file comparison. Watchdog-guarded so a regression faults instead of hanging the test binary.
#[test]
fn example_socket_timeout_matches_expected() {
    let manifest = crate::native::std_embed::checkout_root();
    let entry = manifest.join("examples/socket_timeout.chz");
    let expected = std::fs::read_to_string(manifest.join("examples/socket_timeout.expected"))
        .expect("read examples/socket_timeout.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&entry, crate::native::HostConfig::default()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "socket_timeout.chz faulted: {res:?}");
            assert_eq!(
                out, expected,
                "socket_timeout.chz output diverged from its golden"
            );
        }
        Err(_) => panic!("socket_timeout.chz hung — D6c timeout regressed"),
    }
}

/// D6b — the production-ready gate (regression for the documented HANG): a `parallel:` runs a
/// fiber parked on `accept` (no client ever connects, so it parks on the netpoller forever) beside
/// a sibling that faults. Before D6b's `poller::drain_family`, the faulting sibling tripped cancel
/// but never reached the poller-parked acceptor — its task stayed `inflight`, the fault never
/// propagated, and the nursery wedged. Now the drain re-injects the acceptor, it unwinds on the
/// cancel flag, and the original fault surfaces. Watchdog-guarded: a regression re-hangs here.
#[test]
fn net_faulting_sibling_aborts_accept_parked_peer() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return ?(10 / z)

fn acceptor(server: net.Listener) -> int!:
    conn := server.accept()?
    conn.close()
    return ?0

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    parallel:
        spawn acceptor(server)
        spawn faulter(0)
    return ?0

fn main():
    match run():
        ?_: print("joined ok")
        !e: print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling's error must propagate, not hang the nursery");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b — non-blocking `connect` actually parks (and is drainable): a fiber connects to an
/// unroutable TEST-NET-1 address (RFC 5737 `192.0.2.0/24` — the SYN gets no reply, so the
/// non-blocking connect stays `EINPROGRESS` and the fiber parks on writability *indefinitely*),
/// while a sibling faults. A blocking v1 connect would have pinned a worker on the dead handshake;
/// the parked connect must instead be reached by `poller::drain_family` so the fault propagates and
/// the nursery joins. Deterministic (the address never completes) and watchdog-guarded.
#[test]
fn net_connect_parks_and_is_drained_on_fault() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return ?(10 / z)

fn dialer() -> int!:
    sock := net.connect("192.0.2.1:9")?
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn dialer()
        spawn faulter(0)
    return ?0

fn main():
    match run():
        ?_: print("joined ok")
        !e: print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling aborts the connect-parked dialer, no hang");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b/W7-59 — `net.connect` to a dead loopback port (bound-then-dropped) returns a clean `Err`
/// rather than hanging, and the engine answers with the message its contract says it should.
///
/// A dead LOOPBACK port still reports `EINPROGRESS` (measured: `connect_ex(('127.0.0.1', <closed>))`
/// → `115 EINPROGRESS`), so this really does reach the would-block gate rather than the synchronous
/// arm — which is what makes it the pin for `W7-59`. The pre-`W7-59` version asserted only that the
/// `Err` arm was taken and discarded `e.message()`, so it passed identically before and after the
/// change and would not have noticed either engine losing its answer.
///
/// The engine must report the kernel's refusal. `W7-59` narrowed the block-in-place gate to
/// the eager-`Executor` job alone — NOT to the siblings' `Socket` gate, which would have refused
/// the cooperative engine too, while it existed. The difference is load-bearing: `accept`/`read` wait
/// on a chezzi peer fiber that can only run on the very thread they would block, so refusing there
/// prevented
/// self-starvation (`W7-40` R1); a `connect` handshake is completed by the KERNEL, starves no chezzi
/// party, and both ancestors block on it (CPython `socket.connect` 0.1 ms, Go `net.Dial` 314 µs from
/// the main goroutine). So the cooperative engine kept its answer here, and got `demote_block_socket`'s
/// escapes on the way.
///
/// Guards the bounded-wait property too: a regression to an unbounded wait on a non-completing
/// handshake surfaces as the watchdog timeout below.
#[test]
fn net_connect_top_level_dead_port_errors_not_hangs() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let src = format!(
        "import std.net\nfn main():\n    match net.connect(\"{dead}\"):\n        ?_: print(\"connected\")\n        !e: print(\"ERR: {{e.message()}}\")\nmain()\n"
    );
    let want = "connect failed: ";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let (out, _e, res, _c) = run_file(&entry);
        let _ = tx.send((out, res));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok((out, res)) => {
            res.expect("top-level connect program runs");
            assert!(
                out.starts_with(&format!("ERR: {want}")),
                "dead port ⇒ Err branch (bounded, no hang); got {out:?}"
            );
        }
        Err(_) => panic!("hung: top-level connect to a dead port did not return"),
    }
}

/// D6 — the headline netpoller test: an echo server services **far more connections than there are
/// workers**, without a thread per connection. One acceptor fiber + N=100 client fibers run in a
/// single `parallel:` over a core-sized pool (100 ≫ cores). Every client parks on its `read` and
/// the acceptor parks on each `accept`/`read` — on the netpoller, not a pinned worker — so all 100
/// round-trips complete. Without the poller (thread-per-park) the bounded pool would starve and the
/// watchdog would fire. (N stays under the TCP backlog so the v1 *blocking* connect never pins a
/// worker waiting for backlog room — non-blocking connect is deferred to D6b; the per-connection
/// handler runs inline in the acceptor because M:N's fixed task `total` has no spawn-after-join.)
#[test]
fn net_echo_server_services_more_conns_than_workers() {
    let src = r#"import std.net
import AtomicInt from std.concurrency

fn acceptor(server: net.Listener, n: int) -> int!:
    for _ in 0..n:
        conn := server.accept()?
        msg := conn.read(64)?
        conn.write("echo:" + msg)?
        conn.close()
    server.close()
    return ?0

fn client(addr: str, served: AtomicInt) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        served.add(1)
    return ?0

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    served := AtomicInt(0)
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr, served)
    return ?served.load()

fn main():
    match run(100):
        ?served: print("all served: {served}")
        !e: print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "100-conn echo server must not fault: {res:?}");
    assert!(
        out.contains("all served: 100"),
        "every connection COMPLETED its round-trip + the nursery joined: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — the spec's canonical shape: the acceptor `spawn`s a `handle(conn)`
/// fiber PER connection inside its `parallel:` instead of serving inline, and the inner nursery
/// joins them. `#conns ≫ #workers` still completes (handlers multiplex over the core-sized pool).
/// Exercises the eager-nursery + `MnSched::inject` path end-to-end with the bytecode engine.
#[test]
fn net_echo_server_spawns_handler_per_connection() {
    // Nested socket nurseries (an acceptor's `parallel:` servicing outer-sibling clients) need
    // ≥2 hw threads: the inner join blocks the parent's outer worker (decision B), so on a single
    // core the outer clients can't progress to drain the echoes — a pre-existing M:N limit that
    // per-connection spawn is the first to exercise. Skip on 1 core (CI is ≥2 core) rather than hang.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net
import AtomicInt from std.concurrency

fn handle(conn: net.Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return ?0

fn acceptor(server: net.Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return ?0

fn client(addr: str, served: AtomicInt) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        served.add(1)
    return ?0

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    served := AtomicInt(0)
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr, served)
    return ?served.load()

fn main():
    match run(100):
        ?served: print("all served: {served}")
        !e: print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "per-connection-spawn echo server must not fault: {res:?}"
    );
    assert!(
        out.contains("all served: 100"),
        "every connection COMPLETED its round-trip in its own fiber: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — proves handlers run CONCURRENTLY with accepting (not queued-to-join).
/// A single client opens N connections SEQUENTIALLY: each reply must arrive before the next
/// connect. The acceptor `spawn`s a handler per connection. Under the old queue-at-join model the
/// handler never ran during the accept loop, so the client's first `read` blocked forever and the
/// acceptor's second `accept` had no incoming connection → hang (watchdog fires). The eager
/// inner nursery runs each handler immediately, unblocking the client so the loop advances.
#[test]
fn net_echo_sequential_client_needs_concurrent_handlers() {
    // Eager per-connection spawn requires ≥2 hardware threads (the inner join blocks the parent's
    // sole outer worker on a single core — see `Op::EnterNursery`). This test's whole point is a
    // handler running mid-loop to unblock the next connect, which a 1-core box cannot do; skip it
    // there rather than hang. CI runners are ≥2 core in practice.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net
import AtomicInt from std.concurrency

fn handle(conn: net.Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return ?0

fn acceptor(server: net.Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return ?0

fn client(addr: str, n: int, served: AtomicInt) -> int!:
    for _ in 0..n:
        sock := net.connect(addr)?
        sock.write("ping")?
        reply := sock.read(64)?
        sock.close()
        if reply == "echo:ping":
            served.add(1)
    return ?0

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    served := AtomicInt(0)
    parallel:
        spawn acceptor(server, n)
        spawn client(addr, n, served)
    return ?served.load()

fn main():
    match run(8):
        ?served: print("all served: {served}")
        !e: print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "sequential client must complete once handlers run concurrently: {res:?}"
    );
    assert!(
        out.contains("all served: 8"),
        "all 8 sequential round-trips serviced with a correct echo: {out:?}"
    );
    assert!(
        !out.contains("error"),
        "every reply was a correct echo: {out:?}"
    );
}

/// Per-connection spawn — a per-connection HANDLER fault propagates as the acceptor's fault and
/// tears the run down WITHOUT hanging. One injected handler faults (index-out-of-bounds — a real
/// runtime fault, since a spawned task's `Result` *return value* is discarded); the eager inner
/// nursery trips its own cancel (D6b `drain_family` reach sibling handlers), the
/// join surfaces the fault as the acceptor's body fault, and the OUTER nursery then cancels the
/// clients (so a client stranded without its echo unwinds instead of blocking on `read` forever).
#[test]
fn net_echo_handler_fault_cancels_acceptor() {
    // Nested socket nursery → needs ≥2 hw threads (see `net_echo_server_spawns_handler_per_connection`).
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: net.Socket, i: int) -> int!:
    msg := conn.read(64)?
    if i == 0:
        conn.close()
        boom := [1]
        return ?boom[10]
    conn.write("echo:" + msg)?
    conn.close()
    return ?0

fn acceptor(server: net.Listener, n: int) -> int!:
    parallel:
        for i in 0..n:
            conn := server.accept()?
            spawn handle(conn, i)
    server.close()
    return ?0

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    return ?1

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return ?0

fn main():
    match run(6):
        ?_: print("all served")
        !e: print("error: " + e.message())

main()
"#;
    // The whole point is "no hang": `run_parallel_watchdog` panics if the nursery never
    // terminates. A faulting handler must drive the run to a clean finish (fault surfaced via the
    // acceptor's `match`, or propagated), not deadlock the netpoller-parked siblings.
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_err() || out.contains("error"),
        "a per-connection handler fault must surface (faulted run or reported error), not be swallowed: res={res:?} out={out:?}"
    );
    assert!(
        !out.contains("all served"),
        "the run must not report success once a handler faulted: {out:?}"
    );
}

/// Per-connection spawn — the DEGENERATE eager nursery: a `parallel:` body (entered eagerly under
/// `--parallel` inside a fiber) that injects NOTHING. `activate_eager_nursery` builds a `total==0`
/// sched with `body_open`; `JoinNursery` must `close_body` and have the inline worker terminate
/// immediately (`done==0==total`) and join the drainer — not hang on the empty sched. Pins the
/// `body_open` → `close_body` → terminate handshake on the empty path.
#[test]
fn eager_nursery_with_zero_spawns_completes() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = "fn worker():\n    parallel:\n        print(\"eager body, no spawn\")\n    print(\"worker done\")\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "an empty eager nursery must join cleanly: {res:?}"
    );
    assert!(
        out.contains("eager body, no spawn"),
        "the body ran: {out:?}"
    );
    assert!(
        out.contains("worker done"),
        "the eager nursery joined and the worker continued: {out:?}"
    );
}

/// Per-connection spawn — CONCURRENT eager nurseries (the pool-exhaustion regression): four
/// independent servers each run their OWN eager per-connection-spawn nursery at once. Because each
/// eager nursery drains on a DEDICATED raw OS thread (not the bounded process pool), they do not
/// starve each other — with the earlier pool-farmed design, four long-running eager drainers would
/// exhaust a core-sized pool and hang (undetectably, since `body_open` vetoes the deadlock predicate).
#[test]
fn net_concurrent_eager_servers_do_not_exhaust_pool() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net
import AtomicInt from std.concurrency

fn handle(conn: net.Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return ?0

fn server_loop(server: net.Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return ?0

fn pinger(addr: str, served: AtomicInt) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        served.add(1)
    return ?0

fn one_server(n: int, served: AtomicInt) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn server_loop(server, n)
        for _ in 0..n:
            spawn pinger(addr, served)
    return ?0

fn run(servers: int, conns: int) -> int!:
    served := AtomicInt(0)
    parallel:
        for _ in 0..servers:
            spawn one_server(conns, served)
    return ?served.load()

fn main():
    match run(4, 12):
        ?served: print("all servers done: {served}")
        !e: print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "concurrent eager servers must not fault: {res:?}"
    );
    assert!(
        out.contains("all servers done: 48"),
        "every concurrent eager nursery completed all 12 round-trips: {out:?}"
    );
    assert!(!out.contains("error"), "no pinger saw a bad echo: {out:?}");
}

/// B3.5 — a task that finishes normally but strands a `recv`-blocked sibling (it never sent the
/// channel the sibling waits on) is a deadlock. Exercises the `task_finished` `live--` path:
/// dropping the finished task from the live count makes `blocked == live`, so the survivor faults.
#[test]
fn parallel_finished_task_leaves_sibling_deadlocked() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn quick():\n    print(\"quick\")\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn waiter(c)\n        spawn quick()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("a finished sibling that strands a recv-blocked task is a deadlock");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
    assert!(
        out.contains("quick"),
        "the finished task's output is still flushed: {out:?}"
    );
}
