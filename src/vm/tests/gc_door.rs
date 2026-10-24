//! VM tests: the GC door — a panic out of a borrowed joiner shell, and a requester that joins a
//! stop already running.

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

/// B requests a collection while A's stop is already running, so B joins it as a poller. B's own
/// heap must still be swept before its `gc_collect` returns: until M1.7 each thread's heap is its
/// own, and A's collection does not touch it. The order is forced: B is attached and blocked on a
/// channel with no door, so A's stop waits for B; B is woken only once the stop is requested.
#[test]
fn a_requester_that_joins_a_running_stop_still_sweeps_its_own_heap() {
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
