//! VM tests: the GC door — a panic out of a borrowed joiner shell, a requester that joins a stop
//! already running, and the one collection that marks every root source of the run.

use super::*;
use std::sync::mpsc::{self, RecvTimeoutError};

use std::time::Duration;

/// Runs `f` on its own thread and fails the test if it does not finish in 20 s, so a hang is a
/// failure and never a stuck test binary.
fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(v) => {
            h.join().unwrap();
            v
        }
        Err(RecvTimeoutError::Timeout) => panic!("the GC door test hung"),
        Err(RecvTimeoutError::Disconnected) => match h.join() {
            Err(p) => std::panic::resume_unwind(p),
            Ok(()) => unreachable!("the test thread exited without a result"),
        },
    }
}

/// A panic that escapes the worker loop of a shell the join only borrows (the shell outlives the
/// door's closure) leaves the shell's token on the unwind. The door then reattaches the outer
/// `Vm` with one token on the thread, and another `Vm` can attach after the outer leaves.
#[test]
fn a_panic_out_of_a_borrowed_shell_loop_leaves_its_token() {
    within(|| {
        let mut outer = Vm::new(Arc::new(empty_program()));
        assert!(outer.gc_attach());
        let sched = Arc::new(mk_sched(1));
        let mut shell = outer.spawn_worker();
        super::worker_loop::PANIC_AT_LOOP_TOP.with(|p| p.set(true));
        let r = outer.gc_blocking(&[], |_| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                shell.mn_worker_loop(&sched, 0, SENTINEL_SCOPE)
            }))
        });
        assert!(r.is_err(), "the hook panics inside the loop");
        assert!(shell.attached.is_none(), "the shell left on the unwind");
        assert!(outer.attached.is_some(), "the outer reattached");
        outer.gc_leave();
        let mut again = outer.spawn_worker();
        assert!(
            again.gc_attach(),
            "the thread holds no token after the outer left"
        );
    });
}

/// B requests a collection while A's stop is already running, so B joins it as a poller. M1.7:
/// the run has one heap, so A's collection sweeps B's garbage too and serves B's request: B's
/// unrooted objects are gone when its `gc_collect` returns. The order is forced: B is attached and
/// blocked on a channel with no door, so A's stop waits for B; B is woken only once the stop is
/// requested.
#[test]
fn a_requester_that_joins_a_running_stop_is_served_by_it() {
    let left = within(|| {
        let root = Vm::new(Arc::new(empty_program()));
        let coord = Arc::clone(&root.gc_coord);
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let mut b = root.spawn_worker();
        let joiner = std::thread::spawn(move || {
            b.gc_attach();
            b.gc_collect();
            let base = b.heap.live();
            for _ in 0..8 {
                b.heap.alloc(Obj::List(Vec::new()));
            }
            ready_tx.send(()).unwrap();
            // Attached and not at a safepoint: A's stop cannot finish before B joins it.
            go_rx.recv().unwrap();
            b.gc_collect();
            b.heap.live() - base
        });
        ready_rx.recv().unwrap();
        let mut a = root.spawn_worker();
        let requester = std::thread::spawn(move || {
            a.gc_attach();
            a.gc_collect();
        });
        while !coord.stop_requested() {
            std::thread::yield_now();
        }
        go_tx.send(()).unwrap();
        requester.join().unwrap();
        joiner.join().unwrap()
    });
    assert_eq!(
        left, 0,
        "objects left in B's heap after a gc_collect that joined A's stop"
    );
}

/// M1.7 — one collection marks the union of the run's roots and sweeps the one heap. Two handles
/// share one core (`a` and the worker `b`); the roots are split over a thread stopped at a
/// safepoint (`b`, root `s` on its stack), a detached thread (published root `d`) and a parked
/// context in the run's registry (root `r`). `a`'s collection keeps all three and frees `u`,
/// which nothing roots.
#[test]
fn one_collection_marks_every_root_source_and_frees_the_rest() {
    use std::sync::atomic::{AtomicBool, Ordering};
    within(|| {
        let mut a = Vm::new(Arc::new(empty_program()));
        let mut b = a.spawn_worker();
        let coord = Arc::clone(&a.gc_coord);
        let s = b.heap.alloc(Obj::Str("stopped".into()));
        b.stack.push(Value::obj(s));
        let u = b.heap.alloc(Obj::Str("garbage".into()));
        let d = a.heap.alloc(Obj::Str("detached".into()));
        let r = a.heap.alloc(Obj::Str("parked".into()));
        let parked = a.root_registry.register(FiberCtx {
            module_objs: vec![r],
            ..FiberCtx::default()
        });
        let done = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let stopped = {
            let (done, ready_tx) = (Arc::clone(&done), ready_tx.clone());
            std::thread::spawn(move || {
                b.gc_attach();
                ready_tx.send(()).unwrap();
                while !done.load(Ordering::SeqCst) {
                    b.gc_poll();
                    std::thread::yield_now();
                }
                b.gc_leave();
            })
        };
        let detached = {
            let (coord, done) = (Arc::clone(&coord), Arc::clone(&done));
            std::thread::spawn(move || {
                let ticket = coord.attach().detach(vec![d]);
                ready_tx.send(()).unwrap();
                while !done.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                drop(ticket.reattach());
            })
        };
        ready_rx.recv().unwrap();
        ready_rx.recv().unwrap();
        a.gc_attach();
        a.gc_collect();
        let kept = [s, d, r].map(|h| a.heap.holds(h));
        let freed = !a.heap.holds(u);
        done.store(true, Ordering::SeqCst);
        stopped.join().unwrap();
        detached.join().unwrap();
        a.gc_leave();
        drop(parked);
        assert_eq!(
            kept, [true; 3],
            "the stopped, detached and parked roots survive"
        );
        assert!(freed, "the unrooted object is freed");
    });
}
