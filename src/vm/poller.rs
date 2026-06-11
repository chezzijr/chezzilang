//! D6 — the netpoller: one process-wide thread owning a [`polling::Poller`] (epoll / kqueue / IOCP),
//! turning a would-block socket op into a cheap fiber-park instead of a pinned worker.
//!
//! When a non-blocking socket op (`read`/`write`/`accept`) returns `WouldBlock`, the VM rewinds its
//! `ip` (so the op re-executes on resume) and hands the parked [`Fiber`] + its fd to this service via
//! [`register`]. The poll thread `wait()`s on OS readiness; when the fd fires it injects the fiber
//! back onto its nursery scheduler via the **existing** [`MnSched::complete_offload`] (inflight →
//! runnable + `notify`), exactly like the blocking pool / timer completion. The op then **re-runs**
//! — there is no `resume_native` result to stash, unlike an offloaded native (which computes a value
//! off-thread); the socket op simply tries again on a now-ready fd.
//!
//! **Accounting.** A socket-parked fiber is counted in `MnSched::inflight` (it WILL be woken by the
//! OS), *not* in the `parked` buckets (which are deadlock-eligible). [`super::MnSched::poll_park_offload`]
//! does the running→inflight transition before calling [`register`]; [`MnSched::complete_offload`]
//! does inflight→runnable on the inject — so the deadlock predicate is unchanged (an in-flight socket
//! op vetoes a false deadlock; a lone `accept`-parked server with no client correctly never
//! self-terminates, Go-identical).
//!
//! **fd lifecycle.** `polling::Poller::add` requires the fd be `delete`d before it is dropped. Every
//! exit path here deletes first: the fire path deletes before injecting (the fd is still open — the
//! fiber hasn't resumed to `close` it yet); [`deregister`] (called by a socket `close` that races a
//! pending park) deletes then re-injects the stranded fiber so it resumes, finds the socket closed,
//! and faults cleanly — never lost, never an `inflight` leak.
//!
//! **Cancel/fault draining (D6b).** A poller-parked fiber lives in this service's registry, *not* in
//! the scheduler's `parked` buckets — so B3.4's `cancel_drain` (which walks `parked`) does not reach
//! it. [`drain_sched`] closes that gap: when a sibling faults / `os.exit`s, [`super::Vm::mn_worker_loop`]
//! calls it alongside `cancel_drain`, re-injecting every fiber parked on this nursery's sockets. The
//! re-injected fiber resumes and hits the cancel check at [`super::Vm::run_until`]'s loop-top BEFORE
//! its rewound socket op re-runs, so it unwinds as `cancelled` and the fault propagates — the
//! `parallel:` joins instead of wedging. A net server may now share a nursery with a fallible sibling.
//! (Deadlock detection still needs no drain: an `inflight` poller-parked fiber vetoes a false
//! deadlock, so the predicate can't fire while one is parked — only the cancel/fault path could
//! strand them, and that is now drained.)

use super::{Fiber, MnSched};
use polling::{Event, Events, Poller};
use std::collections::HashMap;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// The direction of readiness a socket op is waiting on.
#[derive(Clone, Copy, Debug)]
pub enum Interest {
    Read,
    Write,
}

/// One parked socket op: the fiber (moved in, injected back on readiness), which nursery scheduler to
/// inject it into, and the raw fd (to `delete` it from the pollset before the fiber can close it).
struct Parked {
    fiber: Fiber,
    sched: Arc<MnSched>,
    fd: RawFd,
    /// The owning socket's `in_flight` flag (see [`super::core::SocketCore::in_flight`]) — cleared
    /// here when the fiber is injected back or de-registered, so the resumed op can re-park.
    in_flight: Arc<AtomicBool>,
}

/// The poller + its registry, shared (by `Arc`) between the registering worker threads and the single
/// poll thread. `poller` is `Sync` — the crate explicitly allows `add`/`delete` from other threads
/// while one thread is blocked in `wait` — so it sits beside the `Mutex`'d registry, not inside it.
struct Inner {
    poller: Poller,
    registry: Mutex<HashMap<usize, Parked>>,
}

/// The process-wide netpoller handle (the `Arc<Inner>` is also held by the poll thread).
pub struct NetPoller {
    inner: Arc<Inner>,
}

static SERVICE: OnceLock<NetPoller> = OnceLock::new();

/// Register read/write interest on `fd` under `key`, parking `fiber` (to be injected back into
/// `sched`) until the OS reports readiness. Called by [`super::MnSched::poll_park_offload`] *after* it
/// has done running→inflight, so the fiber is accounted as in-flight before the OS can wake it.
pub fn register(key: usize, fd: RawFd, interest: Interest, fiber: Fiber, sched: Arc<MnSched>, in_flight: Arc<AtomicBool>) {
    SERVICE.get_or_init(NetPoller::new).register(key, fd, interest, fiber, sched, in_flight);
}

/// De-register a pending park on `key` (a socket `close` racing the park). If a fiber was still
/// parked, delete its fd interest and re-inject it (the re-run hits the now-closed socket → clean
/// fault) — returns `true`. `false` if nothing was registered (the common `close`-after-use case,
/// where the owning fiber is running, not parked).
pub fn deregister(key: usize) -> bool {
    SERVICE.get_or_init(NetPoller::new).deregister(key)
}

/// D6b — the cancel/fault hook: re-inject every fiber parked on a socket belonging to `sched` (a
/// nursery whose sibling faulted / `os.exit`ed), disarming each fd. A re-injected fiber resumes,
/// hits the cancel check at [`super::Vm::run_until`]'s loop-top (BEFORE its rewound socket op
/// re-runs), and unwinds as `cancelled` — so the fault propagates and the nursery joins instead of
/// hanging on a poller-parked peer. Called from [`super::Vm::mn_worker_loop`]'s abort branch beside
/// [`super::MnSched::cancel_drain`] (which drains the channel-`recv` park set); together they reach
/// every parked fiber. A no-op if this nursery has nothing parked on the poller (the common case).
pub fn drain_sched(sched: &Arc<MnSched>) {
    SERVICE.get_or_init(NetPoller::new).drain_sched(sched);
}

impl NetPoller {
    fn new() -> Self {
        let inner = Arc::new(Inner {
            poller: Poller::new().expect("failed to create the netpoller"),
            registry: Mutex::new(HashMap::new()),
        });
        let t = Arc::clone(&inner);
        // The poll thread only locks the registry + calls `complete_offload` (which re-enqueues a
        // fiber on its scheduler); it never runs interpreter code, so the default stack is ample.
        std::thread::Builder::new()
            .name("chezzi-netpoller".into())
            .spawn(move || poll_loop(&t))
            .expect("failed to spawn the chezzi netpoller thread");
        NetPoller { inner }
    }

    fn register(&self, key: usize, fd: RawFd, interest: Interest, fiber: Fiber, sched: Arc<MnSched>, in_flight: Arc<AtomicBool>) {
        // File the parked op BEFORE arming the fd: the poll thread removes from the registry under its
        // lock, so a readiness event can never observe an armed fd with no registry entry. The key is
        // never a duplicate: a second op on the same socket is rejected by the `in_flight` guard in
        // `park_on_fd` before it reaches here, so neither the `insert` nor the `add` below can collide.
        let prev = self.lock_registry().insert(key, Parked { fiber, sched, fd, in_flight });
        debug_assert!(prev.is_none(), "netpoller registry key reused — the in_flight guard was bypassed");
        let ev = match interest {
            Interest::Read => Event::readable(key),
            Interest::Write => Event::writable(key),
        };
        // SAFETY: `fd` is owned by the live `SocketCore` whose handle is rooted on the parked fiber's
        // operand stack, so it stays open until this op is `delete`d (the fire path or `deregister`),
        // both of which precede any stream drop — satisfying `add`'s delete-before-drop contract.
        unsafe { self.inner.poller.add(fd, ev) }.expect("netpoller add");
    }

    fn deregister(&self, key: usize) -> bool {
        let parked = self.lock_registry().remove(&key);
        if let Some(Parked { fiber, sched, fd, in_flight }) = parked {
            // SAFETY: still in the registry ⇒ never injected ⇒ fd still open (rooted on `fiber`).
            let _ = self.inner.poller.delete(unsafe { BorrowedFd::borrow_raw(fd) });
            in_flight.store(false, Ordering::Release); // the op is no longer parked
            sched.complete_offload(fiber); // inflight→runnable; the re-run faults on the closed socket
            true
        } else {
            false
        }
    }

    fn drain_sched(&self, sched: &Arc<MnSched>) {
        // Collect-and-remove the matching entries UNDER the registry lock, then release it before
        // touching the scheduler: `complete_offload` takes the sched lock, so doing it here (registry
        // lock held) would nest registry→sched, whereas the fire path nests sched alone — keep the
        // registry lock leaf-level to rule out any lock-order inversion. Selection is by `Arc::ptr_eq`
        // (same scheduler instance), not the deadlock-error/cancel token, so sibling nurseries are
        // never disturbed.
        let drained: Vec<Parked> = {
            let mut reg = self.lock_registry();
            let keys: Vec<usize> =
                reg.iter().filter(|(_, p)| Arc::ptr_eq(&p.sched, sched)).map(|(k, _)| *k).collect();
            keys.into_iter().filter_map(|k| reg.remove(&k)).collect()
        };
        for Parked { fiber, sched, fd, in_flight } in drained {
            // SAFETY: still in the registry ⇒ never injected ⇒ fd still open (rooted on `fiber`).
            let _ = self.inner.poller.delete(unsafe { BorrowedFd::borrow_raw(fd) });
            in_flight.store(false, Ordering::Release); // the op is no longer parked
            sched.complete_offload(fiber); // inflight→runnable; the re-run unwinds on the cancel flag
        }
    }

    fn lock_registry(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Parked>> {
        self.inner.registry.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The poll thread's lifetime: block until at least one fd is ready, then for each event remove its
/// parked op, delete the fd from the pollset (oneshot fired; the fd is still open), and inject the
/// fiber back onto its scheduler. A process daemon — it ends only when the process exits (like the
/// timer / blocking-pool threads); no explicit shutdown.
fn poll_loop(inner: &Inner) {
    let mut events = Events::new();
    loop {
        events.clear();
        // `None` timeout: block until a fd is ready (or a spurious wake). A failed `wait` (already
        // retried past EINTR internally) just loops.
        if inner.poller.wait(&mut events, None).is_err() {
            continue;
        }
        for ev in events.iter() {
            let parked = inner.registry.lock().unwrap_or_else(|e| e.into_inner()).remove(&ev.key);
            if let Some(Parked { fiber, sched, fd, in_flight }) = parked {
                // Oneshot fired: remove the fd before injecting (delete-before-drop; the fd is still
                // open — the fiber resumes only after this, and only it can close the socket).
                // SAFETY: fd open until the injected fiber resumes (see `register`).
                let _ = inner.poller.delete(unsafe { BorrowedFd::borrow_raw(fd) });
                in_flight.store(false, Ordering::Release); // op no longer parked → the resumed op may re-park
                // Inject EXACTLY like a blocking-pool / timer completion: inflight→runnable + wakep.
                // No `resume_native` stash — the socket op re-runs (its `ip` was rewound on park).
                sched.complete_offload(fiber);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::vm::core::new_in_flight;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn dl_err() -> super::super::RuntimeError {
        super::super::RuntimeError { message: "deadlock".into(), span: Span { line: 1, col: 1 } }
    }

    fn mk_sched() -> Arc<MnSched> {
        Arc::new(MnSched::new(1, 1, Arc::new(AtomicBool::new(false)), dl_err()))
    }

    fn mk_fiber() -> Fiber {
        Fiber {
            ctx: super::super::FiberCtx::default(),
            state: super::super::FiberState::Ready,
            task_index: 0,
            span: Span { line: 1, col: 1 },
            resume_native: None,
        }
    }

    /// A connected loopback pair; returns (client, accepted-server-stream set non-blocking).
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        (client, server)
    }

    fn wait_until<F: Fn() -> bool>(f: F, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "{what} did not happen within 5s");
            std::thread::yield_now();
        }
    }

    /// register → OS readiness → the parked fiber is injected back (inflight→runnable, lands on the
    /// global run queue). The core D6 contract.
    #[test]
    fn register_then_event_injects_fiber() {
        let (mut client, server) = loopback_pair();
        let sched = mk_sched();
        sched.inflight.fetch_add(1, Ordering::Relaxed); // simulate poll_park_offload's running→inflight
        register(usize::MAX - 1, server.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched), new_in_flight());

        // Nothing written yet → fd not readable → fiber stays parked.
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 1, "fiber parked before any data");

        client.write_all(b"x").unwrap(); // make the server fd readable
        // Wait on the lock-synchronized global queue, not the bare `inflight` atomic: `complete_offload`
        // drops `inflight` and bumps `runnable` under the core lock, so observing the fiber on `global`
        // (taken under that lock) guarantees both counters have settled.
        wait_until(|| sched.lock().global.len() == 1, "inject");
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 0, "inflight→runnable on inject");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 1, "injected fiber is runnable");
        drop(server);
    }

    /// The `in_flight` guard's correctness hinges on the poller CLEARING it on inject: only then can
    /// the resumed op re-park (a still-set flag would make the owner fault itself), while a *concurrent*
    /// fiber sharing the socket sees it set and faults. Set the flag (as `park_on_fd` would), park,
    /// fire, and assert the flag is cleared exactly when the fiber is injected.
    #[test]
    fn inject_clears_in_flight() {
        let (mut client, server) = loopback_pair();
        let sched = mk_sched();
        let in_flight = new_in_flight();
        in_flight.store(true, Ordering::Release); // simulate park_on_fd's swap(true)
        sched.inflight.fetch_add(1, Ordering::Relaxed);
        register(usize::MAX - 4, server.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched), Arc::clone(&in_flight));
        assert!(in_flight.load(Ordering::Acquire), "flag stays set while the op is parked");

        client.write_all(b"x").unwrap();
        wait_until(|| sched.lock().global.len() == 1, "inject");
        assert!(!in_flight.load(Ordering::Acquire), "inject clears in_flight so the resumed op may re-park");
        drop(server);
    }

    /// No readiness event → no inject: a registered-but-not-ready fd leaves the fiber parked.
    #[test]
    fn no_event_does_not_inject() {
        let (_client, server) = loopback_pair();
        let key = usize::MAX - 2;
        let sched = mk_sched();
        sched.inflight.fetch_add(1, Ordering::Relaxed);
        register(key, server.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched), new_in_flight());

        // No write → give the poller a chance to (wrongly) fire, then assert it did not. NOTE: this
        // fixed sleep can only mask a real bug (a false inject under load), never flake — do not
        // "tighten" it into a race.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 1, "no data ⇒ fiber stays parked");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 0, "no spurious inject");

        deregister(key); // clean up the registration (delete-before-drop) before `server` drops
        drop(server);
    }

    /// D6b — `drain_sched` (the cancel/fault hook): every fiber parked on `target`'s sockets is
    /// re-injected (so it resumes, observes the nursery cancel flag at `run_until`'s loop-top, and
    /// unwinds) and its fd disarmed; a fiber parked on a *different* sched is untouched. This is the
    /// fix for the documented hang — a faulting sibling can now abort an `accept`/`read`-parked peer.
    #[test]
    fn drain_sched_reinjects_matching_and_disarms() {
        let (mut client_a1, server_a1) = loopback_pair();
        let (_client_a2, server_a2) = loopback_pair();
        let (_client_b, server_b) = loopback_pair();
        let sched_a = mk_sched();
        let sched_b = mk_sched();
        let (k_a1, k_a2, k_b) = (usize::MAX - 10, usize::MAX - 11, usize::MAX - 12);
        let (if_a1, if_a2, if_b) = (new_in_flight(), new_in_flight(), new_in_flight());
        for f in [&if_a1, &if_a2, &if_b] {
            f.store(true, Ordering::Release);
        }
        sched_a.inflight.fetch_add(2, Ordering::Relaxed);
        sched_b.inflight.fetch_add(1, Ordering::Relaxed);
        register(k_a1, server_a1.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched_a), Arc::clone(&if_a1));
        register(k_a2, server_a2.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched_a), Arc::clone(&if_a2));
        register(k_b, server_b.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched_b), Arc::clone(&if_b));

        drain_sched(&sched_a);

        // sched_a's two parked fibers were re-injected (inflight→runnable) and their guards cleared.
        assert_eq!(sched_a.lock().global.len(), 2, "both sched_a fibers re-injected exactly once");
        assert_eq!(sched_a.inflight.load(Ordering::Relaxed), 0, "sched_a inflight drained");
        assert!(!if_a1.load(Ordering::Acquire) && !if_a2.load(Ordering::Acquire), "drain clears in_flight");
        // sched_b's fiber is a different nursery — left parked.
        assert_eq!(sched_b.lock().global.len(), 0, "sched_b fiber untouched");
        assert_eq!(sched_b.inflight.load(Ordering::Relaxed), 1, "sched_b still parked");

        // Disarmed: making a drained fd readable must NOT inject a second time.
        client_a1.write_all(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(sched_a.lock().global.len(), 2, "drained fd did not double-inject");

        deregister(k_b); // clean up sched_b's registration before its server drops
        drop((server_a1, server_a2, server_b));
    }

    /// deregister (a `close` racing a pending park) re-injects the stranded fiber and disarms the fd,
    /// so a later readiness event does NOT inject it a second time. Pins the close-while-parked path
    /// (Risk #1: no lost fiber, no double-inject, no `inflight` leak).
    #[test]
    fn deregister_reinjects_and_disarms() {
        let (mut client, server) = loopback_pair();
        let key = usize::MAX - 3;
        let sched = mk_sched();
        let in_flight = new_in_flight();
        in_flight.store(true, Ordering::Release);
        sched.inflight.fetch_add(1, Ordering::Relaxed);
        register(key, server.as_raw_fd(), Interest::Read, mk_fiber(), Arc::clone(&sched), Arc::clone(&in_flight));

        assert!(deregister(key), "deregister found the pending park");
        assert!(!in_flight.load(Ordering::Acquire), "deregister also clears in_flight");
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 0, "re-injected (inflight→runnable)");
        assert_eq!(sched.lock().global.len(), 1, "fiber back on the run queue exactly once");

        // The fd is now disarmed: making it readable must NOT inject a second fiber.
        client.write_all(b"x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(sched.lock().global.len(), 1, "disarmed fd did not double-inject");
        assert!(!deregister(key), "second deregister finds nothing");
        drop(server);
    }
}
