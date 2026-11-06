//! VM tests: cross-thread channel delivery, bounded channels, `wait` send arms, pmap.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// Cross-thread channel delivery through one shared `ChannelCore`: a value `send`-queued on the
/// main thread is `recv`-popped on a **separate OS thread** running its own `Vm` — proving both
/// the `Arc<ChannelCore>` is shared across threads AND `Vm: Send` (it moves into
/// `thread::spawn`), the load-bearing fact for the whole thread-flip.
///
/// NOTE: the ORDER is send-then-spawn on purpose. The original B3.3-threads step-2 shape spawned
/// the recv first and relied on a bare-`parallel` `recv` **blocking on the core's `Condvar`** until
/// woken. That condvar-recv path was RETIRED in D2b (4ac1c1b) when the M:N scheduler replaced
/// thread-per-task blocking with fiber snapshot-park (`send_wake` + the predicate deadlock detector).
/// A bare `parallel = true` `Vm` with NO scheduler (`mn == None`) now takes the no-scheduler path, so
/// an empty top-level `recv` is a deadlock FAULT by design — spawning recv-first was racy (it only
/// passed when `send` won the race; CI caught the block-first interleaving). The real
/// block-until-woken contract lives in the scheduler `send_wake`/park tests above (and
/// `parallel_recv_parks_deep_in_flattened_frames_and_resumes`), which exercise the actual current
/// mechanism; this test now covers only the still-true cross-thread-delivery + `Vm: Send` facts.
#[test]
fn parallel_recv_blocks_until_send_wakes_it() {
    let core = Arc::new(ChannelCore::default());
    let mut sender = Vm::new(Arc::new(empty_program()));
    let sh = sender.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let mut worker = Vm::new(Arc::new(empty_program()));
    let wh = worker.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let sp = Span::RUNTIME;
    // Queue first, then hand the receiving `Vm` to another thread: the value is already in the
    // shared core, so the cross-thread `recv` pops it deterministically (no interleaving race).
    sender
        .channel_method(sh, "send", &[Value::int(42)], sp)
        .unwrap();
    let handle = std::thread::spawn(move || {
        let got = worker.channel_method(wh, "recv", &[], sp).unwrap();
        (worker, got)
    });
    let (worker, got) = handle.join().unwrap();
    assert_eq!(recv_ok(&worker, got), Value::int(42));
}

// ----- bounded Channel[T](cap): capacity + backpressure -----

/// `cap()` reports the bound (`Channel[T](n)` → n) or -1 for unbounded.
#[test]
fn bounded_channel_cap_method_both_engines() {
    let src = "b := Channel[int](3)\nprint(b.cap())\nu := Channel[int]()\nprint(u.cap())\n";
    let out = run(src);
    assert_eq!(out, "3\n-1\n");
}

/// A NEGATIVE capacity is a runtime fault (cap must be >= 0). `0` now constructs a rendezvous
/// channel (TICKET-028), so it is no longer part of this fault's repro set.
#[test]
fn negative_channel_cap_faults() {
    let src = "c := Channel[int](-1)\nprint(c.len())\n";
    let e = run_err(src);
    assert!(e.contains("Channel capacity must be >= 0"), "serial: {e}");
    let ep = run_capture(src).expect_err("M:N should fault").message;
    assert_eq!(e, ep, "serial and M:N fault text must match");
}

/// `Channel[T](0)` is Chezzi's rendezvous channel (Go's `make(chan T)`), while `Channel[T]()`
/// keeps its existing UNBOUNDED meaning (TICKET-028).
#[test]
fn rendezvous_channel_cap_zero_should_construct() {
    let src = "c := Channel[int](0)\nprint(c.cap())\n";
    let out = run(src);
    assert_eq!(out, "0\n");
}

/// A negative capacity still faults after TICKET-028's `n < 0` widening.
#[test]
fn rendezvous_negative_cap_still_faults() {
    let src = "c := Channel[int](-1)\nprint(c.len())\n";
    let e = run_err(src);
    assert!(e.contains("Channel capacity must be >= 0"), "{e}");
}

/// `try_send` on a FULL bounded channel returns `false` (not true — the old unbounded contract);
/// after a `recv` frees a slot it returns `true`. Single fiber, so fully deterministic on both.
#[test]
fn bounded_channel_try_send_full_returns_false_both_engines() {
    let src = "c := Channel[int](1)\n\
               print(c.try_send(1))\n\
               print(c.try_send(2))\n\
               print(c.recv())\n\
               print(c.try_send(3))\n\
               print(c.recv())\n";
    let out = run(src);
    assert_eq!(out, "true\nfalse\n1\ntrue\n3\n");
}

/// A `send` on a FULL bounded channel with NO possible consumer (top level, no nursery) is a
/// deadlock fault with a byte-identical message — NOT a silent over-fill.
#[test]
fn bounded_channel_full_send_top_level_deadlocks_both_engines() {
    let src = "c := Channel[int](1)\nc.send(1)\nc.send(2)\nprint(\"unreached\")\n";
    let e = run_err(src);
    assert!(e.contains("send() on a full channel"), "serial: {e}");
    let ep = err_text(&run_capture(src).expect_err("M:N should fault"));
    assert_eq!(e, ep, "serial and M:N deadlock text must match");
}

/// Bugs 3/4 — a bounded `send` that parks inside a nursery and then genuinely deadlocks must report a
/// diagnostic that names the ACTUAL stall (a full `send()`), not the recv-only wording. The stuck task
/// is a SENDER blocked on a FULL channel, not a receiver on an empty one; the generic nursery message
/// must name `full send()` so a debugger is not misdirected toward a nonexistent receiver. Parity:
/// this emits byte-identical text.
#[test]
fn bounded_channel_nursery_send_deadlock_names_full_send_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           c.send(1)\n\
               \x20           c.send(2)\n\
               main()\n";
    let e = run_err(src);
    assert!(
        e.contains("task 1: send() on a full channel"),
        "serial diagnostic must name the send stall: {e}"
    );
    let ep = err_text(&run_capture(src).expect_err("M:N should fault"));
    assert_eq!(e, ep, "serial and M:N deadlock text must match");
}

/// Bounded fan-out golden: a single producer sends 0..5 into a cap-2 channel while a consumer
/// drains 5 in order. Backpressure (producer parks when full) changes WHICH task runs WHEN but not
/// the value sequence — so the output is byte-identical serial vs M:N. Single producer ⇒ no
/// multi-sender contention nondeterminism.
#[test]
fn bounded_channel_fanout_golden_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](2)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           for i in 0..5:\n\
               \x20               c.send(i)\n\
               \x20           c.close()\n\
               \x20       spawn:\n\
               \x20           for v in c:\n\
               \x20               print(v)\n\
               main()\n";
    let out = run(src);
    assert_eq!(out, "0\n1\n2\n3\n4\n");
}

// ===== §6d `wait:` SEND-arms (Go-`select` symmetry) =====

/// The combined send-arm golden (`examples/wait_send.chz`): Phase A mixes recv + send + `else` at the
/// top level (source-order winner, deterministic single fiber); Phase B blocks a bounded send-arm in a
/// nursery until the consumer's `recv` frees the cap-1 slot (the M:N park + receiver-wake path). Single
/// producer/consumer + consumer-only output ⇒ byte-identical against `.expected`.
#[test]
fn golden_wait_send_both_engines() {
    let src = include_str!("../../../examples/wait_send.chz");
    let expected = include_str!("../../../examples/wait_send.expected");
    let out = run(src);
    assert_eq!(out, expected, "output drifted from wait_send.expected");
}

/// Stress the send-arm park + receiver-wake (the delicate M:N scheduler change): a bounded cap-1
/// send-arm producer that parks on every full slot, drained by a single consumer, run many trials on
/// the M:N engine. A lost wakeup (parked producer never re-scheduled after a `recv` frees a slot)
/// would hang or truncate the output — so every trial must reproduce the full ordered sequence.
#[test]
fn wait_send_arm_park_wake_stress_parallel() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           for i in 0..12:\n\
               \x20               wait:\n\
               \x20                   c.send(i): pass\n\
               \x20           c.close()\n\
               \x20       spawn:\n\
               \x20           for x in c:\n\
               \x20               print(x)\n\
               main()\n";
    let expected: String = (0..12).map(|i| format!("{i}\n")).collect();
    assert_eq!(run(src), expected); // serial oracle
    for trial in 0..40 {
        assert_eq!(
            run_capture(src).expect("M:N run"),
            expected,
            "M:N send-arm park/wake diverged on trial {trial}"
        );
    }
}

/// A send-arm on a CLOSED channel is SELECTED (ready) and FAULTS `send on a closed channel` — a
/// closed recv arm is selected too, but binds `!RecvError` — even with an `else` present (a closed send arm is always ready, so
/// `else` never runs). Byte-identical fault.
#[test]
fn wait_send_arm_closed_channel_faults_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   c.close()\n\
               \x20   wait:\n\
               \x20       c.send(1): print(\"sent\")\n\
               \x20       else: print(\"idle\")\n\
               main()\n";
    let e = run_err(src);
    assert!(
        e.contains("send on a closed channel"),
        "closed send arm must fault, got: {e}"
    );
    let ep = run_capture(src).expect_err("M:N should fault").message;
    assert_eq!(e, ep, "serial and M:N closed-send fault text must match");
}

/// A full bounded send-arm reached INSIDE a native callback (`list.map`, `native_reentry > 0`)
/// DEMOTES and blocks on its offer (TICKET-185 changed DEC-181's `Demote` × `Wait+send` cell from
/// `Refuse`). With no receiver anywhere, the deadlock verdict ends it, as Go's `all goroutines are
/// asleep` does; it used to fault `send on a full channel` without blocking.
#[test]
fn wait_send_arm_in_callback_blocks_until_the_deadlock_verdict() {
    let src = "c := Channel[int](1)\n\
               fn f(x: int) -> int:\n\
               \x20   wait:\n\
               \x20       c.send(9): pass\n\
               \x20   return x\n\
               fn main():\n\
               \x20   c.send(0)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           print([1].map(f))\n\
               main()\n";
    let e = run_capture(src).expect_err("a send nobody can take must end in the verdict");
    assert!(
        e.is_deadlock && e.message.starts_with("deadlock:"),
        "expected the deadlock verdict, got: {e:?}"
    );
}

/// An UNBOUNDED send-arm is always ready → placed first it wins immediately and `else` never runs.
#[test]
fn wait_unbounded_send_arm_always_ready_both_engines() {
    let src = "fn main():\n\
               \x20   u := Channel[int]()\n\
               \x20   wait:\n\
               \x20       u.send(1): print(\"sent\")\n\
               \x20       else: print(\"idle\")\n\
               \x20   print(\"q={u.recv()}\")\n\
               main()\n";
    assert_eq!(run(src), "sent\nq=1\n");
    assert_eq!(run_capture(src).expect("M:N run"), "sent\nq=1\n");
}

/// Deterministic source-order tie-break: with BOTH an (earlier) ready recv-arm and a (later) ready
/// send-arm, the earlier recv-arm wins every run; flip the order and the earlier send-arm wins. Never
/// Go-random. Identical.
#[test]
fn wait_send_source_order_first_ready_wins_both_engines() {
    // recv-arm first, both ready → recv wins (the send never fires: `s` stays empty).
    let recv_first = "fn main():\n\
               \x20   r := Channel[int]()\n\
               \x20   s := Channel[int]()\n\
               \x20   r.send(7)\n\
               \x20   wait:\n\
               \x20       v := r.recv(): print(\"recv {v}\")\n\
               \x20       s.send(1): print(\"sent\")\n\
               \x20   print(\"slen={s.len()}\")\n\
               main()\n";
    assert_eq!(run(recv_first), "recv 7\nslen=0\n");
    assert_eq!(
        run_capture(recv_first).expect("M:N run"),
        "recv 7\nslen=0\n"
    );
    // send-arm first + ready (unbounded), recv-arm empty → send wins.
    let send_first = "fn main():\n\
               \x20   s := Channel[int]()\n\
               \x20   r := Channel[int]()\n\
               \x20   wait:\n\
               \x20       s.send(9): print(\"sent\")\n\
               \x20       v := r.recv(): print(\"recv {v}\")\n\
               \x20   print(\"q={s.recv()}\")\n\
               main()\n";
    assert_eq!(run(send_first), "sent\nq=9\n");
    assert_eq!(run_capture(send_first).expect("M:N run"), "sent\nq=9\n");
}

/// Bugs 1/3 — `try_send` on a bounded channel must be ATOMIC (check-space + enqueue under the same
/// lock the blocking `send` uses), or two concurrent M:N `try_send`s both see space and both push,
/// over-filling past `cap`. The invariant `len <= cap` must hold on every engine. Many trials of
/// 16 concurrent `try_send`s into a fresh cap-1 channel that no one drains: with the atomic path at
/// most ONE succeeds per trial, so `len` is never > 1. Buggy (non-atomic) M:N over-fills on ~1-in-5
/// trials → `over > 0` (reliably RED across 200 trials). Serial is single-thread so it is always 0.
#[test]
fn bounded_channel_try_send_atomic_no_overfill_both_engines() {
    let src = "fn producer(c: Channel[int]):\n\
               \x20   c.try_send(1)\n\
               fn main():\n\
               \x20   over := 0\n\
               \x20   for trial in 0..200:\n\
               \x20       c := Channel[int](1)\n\
               \x20       parallel:\n\
               \x20           for k in 0..16:\n\
               \x20               spawn producer(c)\n\
               \x20       if c.len() > 1:\n\
               \x20           over = over + 1\n\
               \x20   print(over)\n\
               main()\n";
    assert_eq!(run(src), "0\n");
    assert_eq!(run_capture(src).expect("M:N run"), "0\n");
}

/// Bug 2 — a full bounded `send` issued on the INLINE outermost-`parallel:` builder VM (`self.mn ==
/// None`, sched held only in `mn_enlist_sched`) has NO worker loop to drive `send_suspend` →
/// `Disp::SendPark`, so it must NOT snapshot-park: parking there leaks `send_suspend` set forever
/// (`paused()` stays true → the main VM silently halts / hangs). It must FAULT with the shared
/// full-channel deadlock message instead (the documented inline-owner-never-parks invariant).
///
/// `inner()`'s nested join early-enlists the OUTER nursery (seeding sibling O) and holds the sched in
/// `mn_enlist_sched`; the outer body then fills a cap-1 channel `t` (nobody drains it — O recvs a
/// DIFFERENT channel) and `t.send(42)` is full ⇒ a non-parkable inline-builder send ⇒ fault, not
/// hang. M:N-only (case A is M:N-only), 30s watchdog catches the buggy silent-halt/hang.
#[test]
fn bounded_channel_inline_builder_full_send_faults_not_hang() {
    let src = "fn inner():\n\
               \x20   spawn:\n\
               \x20       print(\"inner ran\")\n\
               fn main():\n\
               \x20   t := Channel[int](1)\n\
               \x20   g := Channel[int]()\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           g.recv()\n\
               \x20       inner()\n\
               \x20       t.try_send(7)\n\
               \x20       t.send(42)\n\
               \x20   print(\"done\")\n\
               main()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let e = r.expect_err("full inline-builder send must fault, not silently complete");
            assert!(
                err_text(&e).contains("send() on a full channel"),
                "unexpected fault: {}",
                err_text(&e)
            );
        }
        Err(_) => panic!("hung — inline-builder full send parked and leaked send_suspend (bug 2)"),
    }
}

// ----- std.concurrency.pmap: scoped parallel-map helpers (Item 2) -----

/// `pmap` returns results in SUBMISSION order (sort-by-index, never completion order) —
/// byte-identical serial vs M:N. Uses the real embedded std module, so it also proves discovery.
#[test]
fn pmap_submission_order_both_engines() {
    let src = "import pmap from std.concurrency.pmap\n\
               fn main():\n\
               \x20   print(pmap([1, 2, 3, 4], fn(x: int) -> int: x * 2))\n\
               main()\n";
    assert_eq!(pmap_both("pmap_order", src), "[2, 4, 6, 8]\n");
}

/// `pmap_limited` gives the same submission-order result as `pmap` regardless of the in-flight cap.
#[test]
fn pmap_limited_matches_pmap_both_engines() {
    let src = "import pmap_limited from std.concurrency.pmap\n\
               fn main():\n\
               \x20   print(pmap_limited([1, 2, 3, 4, 5, 6, 7, 8], fn(x: int) -> int: x + 100, 2))\n\
               main()\n";
    assert_eq!(
        pmap_both("pmap_limited", src),
        "[101, 102, 103, 104, 105, 106, 107, 108]\n"
    );
}

/// TICKET-211 (W1) — the grid: every nesting shape {flat, nested in a spawned task, two deep,
/// nested in an Executor job} x outer body {open, closed} x T {2, 4, 8} must reach exactly T runner
/// threads. A cell may stay below T only when the `NestedDrainerSlot` budget refused a start (another
/// test holds slots), and never exceed T. The thread count replaced a wall-time test of `nested_closed`
/// (`nested_nursery_in_spawned_task_uses_all_workers`), which failed 1 run in 10 under CPU load.
///
/// TICKET-230 — every shape, plus two Executors, an Executor beside a `parallel:`, an Executor
/// beside a busy main and a body that blocks once then burns (W15-9), at T {1, 2, 4, 8}: the run's
/// peak runner permit holders is at least 1 (main holds one from `Vm::run`) and at most T. The
/// thread assertion above stays on the first 8 shapes at T >= 2.
#[test]
fn runner_threads_reach_the_worker_count_in_every_nesting_shape() {
    #[rustfmt::skip]
    let in_parent = crate::vm::rerun_in_child(module_path!(), "runner_threads_reach_the_worker_count_in_every_nesting_shape");
    if in_parent {
        return;
    }
    struct Workers(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Workers {
        fn drop(&mut self) {
            crate::vm::set_worker_count(crate::vm::test_baseline_worker_count());
        }
    }
    let _workers = Workers(
        crate::vm::TEST_WORKER_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    let header = "
import std.concurrency
fn burn():
    i := 0
    while i < 200000:
        i += 1
fn burn_send(done: Channel[int]):
    burn()
    done.send(1)
fn fan():
    parallel:
        for _ in 0..8:
            spawn burn()
fn fan2():
    parallel:
        spawn fan()
fn fan_send(done: Channel[int]):
    fan()
    done.send(1)
fn fan2_send(done: Channel[int]):
    fan2()
    done.send(1)
";
    let shapes: [(&str, &str); 12] = [
        (
            "flat_closed",
            "
parallel:
    for _ in 0..8:
        spawn burn()
",
        ),
        (
            "flat_open",
            "
done := Channel[int](8)
parallel:
    for _ in 0..8:
        spawn burn_send(done)
    for _ in 0..8:
        done.recv()
",
        ),
        (
            "nested_closed",
            "
parallel:
    spawn fan()
",
        ),
        (
            "nested_open",
            "
done := Channel[int](1)
parallel:
    spawn fan_send(done)
    done.recv()
",
        ),
        (
            "twodeep_closed",
            "
parallel:
    spawn fan2()
",
        ),
        (
            "twodeep_open",
            "
done := Channel[int](1)
parallel:
    spawn fan2_send(done)
    done.recv()
",
        ),
        (
            "exec_closed",
            "
import Executor from std.concurrency
ex := Executor()
ex.submit(fn(): fan())
ex.shutdown()
",
        ),
        (
            "exec_open",
            "
import Executor from std.concurrency
done := Channel[int](1)
ex := Executor()
ex.submit(fn(): fan_send(done))
done.recv()
ex.shutdown()
",
        ),
        (
            "two_executors",
            "
import Executor from std.concurrency
a := Executor()
b := Executor()
for _ in 0..4:
    a.submit(fn(): burn())
    b.submit(fn(): burn())
a.shutdown()
b.shutdown()
",
        ),
        (
            "exec_and_parallel",
            "
import Executor from std.concurrency
ex := Executor()
for _ in 0..4:
    ex.submit(fn(): burn())
parallel:
    for _ in 0..4:
        spawn burn()
ex.shutdown()
",
        ),
        (
            "exec_busy_main",
            "
import Executor from std.concurrency
ex := Executor()
for _ in 0..4:
    ex.submit(fn(): burn())
for _ in 0..4:
    burn()
ex.shutdown()
",
        ),
        (
            "blocked_body",
            "
done := Channel[int](1)
parallel:
    spawn burn_send(done)
    for _ in 0..3:
        spawn burn()
    done.recv()
    burn()
",
        ),
    ];
    let mut failures = Vec::new();
    for t in [1usize, 2, 4, 8] {
        for (i, (shape, body)) in shapes.iter().enumerate() {
            crate::vm::set_worker_count(t);
            let entry = write_temp_chz(&format!("w1_grid_{shape}_{t}"), &format!("{header}{body}"));
            let ((_out, _err, res, _code), peak, denials, _spawns, permits) =
                crate::vm::run_file_counting_runners(&entry);
            let _ = std::fs::remove_file(&entry);
            assert!(res.is_ok(), "{shape} T={t} faulted: {res:?}");
            if i < 8 && t >= 2 && !(peak <= t && (peak == t || denials > 0)) {
                failures.push(format!(
                    "{shape} T={t}: peak runner threads {peak}, slot denials {denials}"
                ));
            }
            if !(1 <= permits && permits <= t) {
                failures.push(format!(
                    "{shape} T={t}: peak runner permit holders {permits}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "runner-thread grid: {failures:#?}");
}

/// TICKET-211 — a nursery's runners come from parked runner threads when one is free, not from a
/// new OS thread per claimed wid. Every task blocks on `gate.recv()` until the body sends, so each
/// of the 200 rounds claims runners at 4 workers; reused threads keep the starts far below 200.
#[test]
fn nursery_rounds_reuse_their_runner_threads() {
    if crate::vm::rerun_in_child(module_path!(), "nursery_rounds_reuse_their_runner_threads") {
        return;
    }
    struct Workers(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Workers {
        fn drop(&mut self) {
            crate::vm::set_worker_count(crate::vm::test_baseline_worker_count());
        }
    }
    let _workers = Workers(
        crate::vm::TEST_WORKER_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    crate::vm::set_worker_count(4);
    let entry = write_temp_chz(
        "w1_reuse_rounds",
        "import std.concurrency\nfn hold(gate: Channel[int]):\n    gate.recv()\nr := 0\nwhile r < 200:\n    gate := Channel[int](8)\n    parallel:\n        for _ in 0..8:\n            spawn hold(gate)\n        for _ in 0..8:\n            gate.send(1)\n    r += 1\n",
    );
    let ((_out, _err, res, _code), _peak, denials, spawns, _permits) =
        crate::vm::run_file_counting_runners(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "{res:?}");
    assert!(
        spawns < 100,
        "200 nursery rounds started {spawns} runner threads at 4 workers: runner threads are not reused across scheds (slot denials {denials})"
    );
}

/// TICKET-211 — a `parallel:` body that waits gives its runner slot (bit 0) to the claim, so its
/// queued tasks run on every worker while it waits: one cell per wait kind the body can reach. At
/// 2 workers the run's peak runner threads must reach 2 (the drainer plus the runner the body's
/// freed slot starts). A body that keeps its slot leaves the peak at 1.
#[test]
fn a_waiting_body_hands_its_runner_slot_over_in_every_wait() {
    if crate::vm::rerun_in_child(
        module_path!(),
        "a_waiting_body_hands_its_runner_slot_over_in_every_wait",
    ) {
        return;
    }
    struct Workers(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl Drop for Workers {
        fn drop(&mut self) {
            crate::vm::set_worker_count(crate::vm::test_baseline_worker_count());
        }
    }
    let _workers = Workers(
        crate::vm::TEST_WORKER_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    );
    crate::vm::set_worker_count(2);
    let header = "import std.time\nfn burn(n: int) -> int:\n    s := 0\n    for i in 0..n:\n        s = s + i % 7\n    return s\nch := Channel[int](3)\nparallel:\n    for _ in 0..3:\n        spawn:\n            ch.send(burn(400000))\n";
    let waits: [(&str, &str); 3] = [
        ("recv", "    for _ in 0..3:\n        _ := ch.recv()\n"),
        (
            "wait",
            "    got := 0\n    while got < 3:\n        wait:\n            _ := ch.recv(): got = got + 1\n",
        ),
        (
            "timed_wait",
            "    got := 0\n    while got < 3:\n        wait:\n            _ := ch.recv(): got = got + 1\n            _ := time.timer(60000).recv(): print(\"timeout\")\n",
        ),
    ];
    let mut failures = Vec::new();
    for (name, body) in waits {
        let entry = write_temp_chz(&format!("body_handover_{name}"), &format!("{header}{body}"));
        let ((_out, _err, res, _code), peak, denials, _spawns, _permits) =
            crate::vm::run_file_counting_runners(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "{name} faulted: {res:?}");
        if peak < 2 && denials == 0 {
            failures.push(format!("{name}: peak runner threads {peak} at 2 workers"));
        }
    }
    assert!(
        failures.is_empty(),
        "a waiting body kept its runner slot: {failures:#?}"
    );
}

/// `pmap_limited`'s token bucket actually BOUNDS in-flight tasks: an Atomic max-in-flight probe never
/// exceeds `limit` — the semaphore caps concurrent f-execution.
#[test]
fn pmap_limited_bounds_in_flight_both_engines() {
    // A nested named fn (multi-statement) captures the two Atomics by-ref; both cross a spawn as
    // shared handles, so every task sees the one live counter/max pair. `cur` is incremented on entry
    // and decremented on exit, so it never exceeds the number of tasks running `f` at once — which the
    // token bucket caps at `limit`. `mx` records the peak, so `mx <= limit` proves the bound holds.
    let src = "import std.concurrency\n\
               import pmap_limited from std.concurrency.pmap\n\
               fn main():\n\
               \x20   cur := concurrency.Atomic(0)\n\
               \x20   mx := concurrency.Atomic(0)\n\
               \x20   fn probe(x: int) -> int:\n\
               \x20       n := cur.add(1)\n\
               \x20       m := mx.load()\n\
               \x20       if n > m:\n\
               \x20           mx.store(n)\n\
               \x20       cur.sub(1)\n\
               \x20       return x\n\
               \x20   r := pmap_limited([1, 2, 3, 4, 5, 6, 7, 8], probe, 2)\n\
               \x20   print(mx.load() <= 2)\n\
               main()\n";
    assert_eq!(pmap_both("pmap_bound", src), "true\n");
}

/// `Executor.submit` returns a `Task[T]` handle whose `.get()`, awaited in SUBMISSION order, is
/// byte-identical serial vs M:N (the value is deterministic; only timing varies).
#[test]
fn task_submit_get_submission_order_both_engines() {
    let src = "import Executor, Task from std.concurrency\n\
               fn work(n: int) -> int:\n\
               \x20   return n * n\n\
               fn main():\n\
               \x20   ex := Executor()\n\
               \x20   ts: List[Task[int]] = []\n\
               \x20   for i in 1..6:\n\
               \x20       x := i\n\
               \x20       ts.push(ex.submit(fn() -> int: work(x)))\n\
               \x20   ex.shutdown()\n\
               \x20   for t in ts:\n\
               \x20       match t.get():\n\
               \x20           ?v: print(v)\n\
               \x20           !e: print(e.message())\n\
               main()\n";
    assert_eq!(pmap_both("task_order", src), "1\n4\n9\n16\n25\n");
}

/// `Task.get()` MEMOIZES: a second `.get()` returns the cached value (it must NOT `recv` the drained
/// one-shot channel again, which would block forever). `.done()` is true once the result is in hand.
#[test]
fn task_get_idempotent_and_done_both_engines() {
    let src = "import Executor from std.concurrency\n\
               fn main():\n\
               \x20   ex := Executor()\n\
               \x20   t := ex.submit(fn() -> int: 42)\n\
               \x20   ex.shutdown()\n\
               \x20   print(t.done())\n\
               \x20   match t.get():\n\
               \x20       ?v: print(v)\n\
               \x20       !e: print(e.message())\n\
               \x20   match t.get():\n\
               \x20       ?v: print(v)\n\
               \x20       !e: print(e.message())\n\
               \x20   print(t.done())\n\
               main()\n";
    assert_eq!(pmap_both("task_idem", src), "true\n42\n42\ntrue\n");
}

/// Call-flattening × M:N parking: a fiber that `recv`-parks **several flattened plain-function
/// frames deep** (`main → collect → deep_recv ×6`, all `Op::Call`, parking at `ip > 0`) must
/// suspend with its frames intact and, on a sibling `send`, resume through `run_until(0)` and
/// thread the received value back up the flattened chain. Pre-flatten each of those frames was a
/// nested Rust `run_until`; now they share one loop, so resume reads them straight from the heap
/// `frames` Vec. (Closes the coverage gap the review flagged: park deep in *bytecode* frames, not
/// just inside a native HOF callback.)
#[test]
fn parallel_recv_parks_deep_in_flattened_frames_and_resumes() {
    let src = "\
fn deep_recv(ch: Channel[int], depth: int) -> int:
    if depth <= 0:
        return ch.recv()
    return deep_recv(ch, depth - 1)

fn collect(ch: Channel[int], out: Channel[int]):
    out.send(deep_recv(ch, 5))

fn produce(ch: Channel[int], v: int):
    ch.send(v)

fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn collect(ch, out)
        spawn produce(ch, 99)
    print(out.recv())

main()
";
    assert_eq!(run_capture(src).expect("parallel run"), "99\n");
}

/// L7 round 1 — `Channel[int!]` (the `Error`-existential is sendable-bounded) runs identically:
/// a spawned task sends `Ok`/`Err` over the channel, the parent recvs and matches.
/// Byte-identical against the golden on the newly-admitted `Channel[Error-existential]` shape.
#[test]
fn channel_of_error_existential_ok_err_two_engine_parity() {
    let src = "\
fn worker(ch: Channel[int!]):
    ch.send(?7)
    ch.send(!\"boom\")

fn main():
    ch := Channel[int!]()
    parallel:
        spawn worker(ch)
    a := ch.recv() else: panic(\"ch closed\")
    b := ch.recv() else: panic(\"ch closed\")
    match a:
        ?v: print(\"ok {v}\")
        !e: print(\"err {e.message()}\")
    match b:
        ?v: print(\"ok {v}\")
        !e: print(\"err {e.message()}\")

main()
";
    let expected = "ok 7\nerr boom\n";
    assert_eq!(run_capture(src).expect("run"), expected);
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

/// L7 round 3, Test 1 (Option-B regression) — an INFERRED return (no `-> int!`/`-> Result[..]`
/// annotation) that yields both an `Ok(int)` and an `Err(GErr(..))` branch, where `GErr` satisfies
/// `Error` but is itself non-sendable (holds a non-`Error` protocol field `Odd`), stays legal used
/// PURELY IN-TASK (no channel/spawn): `T` pins to `int`, the `E` slot infers `GErr` and is PRESERVED
/// concrete (`Result[int, GErr]`), not widened/laundered to `Error`. This is the whole point of
/// Option B — inference never triggers the sendable-bounded widening (only an explicit `Error`/`int!`
/// annotation, or a `Channel.send`, does), so it must type-check clean and run byte-identically.
#[test]
fn inferred_non_sendable_error_in_task_ok_both_engines() {
    let src = "\
protocol Odd:
    fn tag(self) -> int

struct Impl:
    fn tag(self) -> int:
        return 1

struct GErr:
    w: Odd
    fn message(self) -> str:
        return \"x\"

fn f(x: int) -> int!GErr:
    if x == 0:
        return ?1
    return !GErr(Impl())

fn main():
    match f(0):
        ?v: print(v)
        !e: print(e.message())
    match f(1):
        ?v: print(v)
        !e: print(e.message())

main()
";
    let expected = "1\nx\n";
    assert_eq!(run_capture(src).expect("run"), expected);
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

/// D2b: an M:N fiber carries its own per-task SIDE state too — `out`/`stderr` (Decision-F output
/// buffers) and the `executors` roots (each a `GcRef` in the run's one heap). A task
/// switch (`swap_task`) round-trips all of them
/// for a context with `owns_side`. A task owns its output stream, so `swap_task` moves
/// `out`/`stderr` for every task context, `owns_side` or not; the `owns_side: false` side state is
/// [`swap_ctx_leaves_side_state_on_the_shell_without_owns_side`].
#[test]
fn swap_task_round_trips_output_and_owns_side_state() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"host-out");
    vm.stderr.extend_from_slice(b"host-err");
    let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
    vm.executors = vec![host_exec];
    // M19 Phase 3 — the intern cache holds the context's own strings; it must round-trip.
    let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
    vm.str_intern.insert(0x10, host_str);
    // M3.1 — the fn-value table is the run's, so it does NOT travel with a context.
    let host_fn = vm.heap.alloc(Obj::Str("host-fn".into()));
    vm.fn_values
        .lock()
        .unwrap()
        .insert(FnKey::Proto(0), host_fn);

    let fib_exec = vm.heap.alloc(Obj::Str("fiber-exec".into()));
    let fib_str = vm.heap.alloc(Obj::Str("fiber-str".into()));
    let mut ctx = FiberCtx {
        owns_side: true,
        out: b"fiber-out".to_vec(),
        stderr: b"fiber-err".to_vec(),
        executors: vec![fib_exec],
        str_intern: fxhash::FxHashMap::from_iter([(0x20usize, fib_str)]),
        ..FiberCtx::default()
    };

    // Schedule in: the fiber's side state becomes live; the shell's parks into the ctx.
    vm.swap_task(&mut ctx);
    assert_eq!(vm.out, b"fiber-out");
    assert_eq!(vm.stderr, b"fiber-err");
    assert_eq!(vm.executors, vec![fib_exec]);
    assert_eq!(vm.str_intern.get(&0x20), Some(&fib_str));
    assert_eq!(vm.str_intern.get(&0x10), None);
    assert_eq!(ctx.out, b"host-out");
    assert_eq!(ctx.str_intern.get(&0x10), Some(&host_str));
    assert_eq!(
        vm.fn_values.lock().unwrap().get(&FnKey::Proto(0)),
        Some(&host_fn)
    );

    // Park out: the shell's side state is restored; the fiber keeps its own.
    vm.swap_task(&mut ctx);
    assert_eq!(vm.out, b"host-out");
    assert_eq!(vm.stderr, b"host-err");
    assert_eq!(vm.executors, vec![host_exec]);
    assert_eq!(vm.str_intern.get(&0x10), Some(&host_str));
    assert_eq!(ctx.out, b"fiber-out");
    assert_eq!(ctx.str_intern.get(&0x20), Some(&fib_str));
    assert_eq!(
        vm.fn_values.lock().unwrap().get(&FnKey::Proto(0)),
        Some(&host_fn)
    );
}

/// Per-connection spawn: a fiber running an eager `parallel:` body can PARK (its acceptor blocks
/// on `accept`) between `EnterNursery` and `JoinNursery`, so the open eager scope — the live
/// inner sched + its monotonic spawn index — MUST travel with the fiber across `swap_ctx`, just
/// like `nurseries`. Otherwise the scope leaks onto whatever fiber the shell schedules next.
#[test]
fn eager_scope_round_trips_with_fiber_ctx() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let host_sched = Arc::new(mk_sched(0));
    vm.eager_scheds.push(Some(EagerScope {
        sched: Arc::clone(&host_sched),
        cancel: Arc::new(AtomicBool::new(false)),
        drainer: None,
        drainer_slot: None,
        scope: 0,
        fiber_owned: false,
        more_scopes: Vec::new(),
    }));

    let fiber_sched = Arc::new(mk_sched(0));
    let mut ctx = FiberCtx {
        eager_scheds: vec![Some(EagerScope {
            sched: Arc::clone(&fiber_sched),
            cancel: Arc::new(AtomicBool::new(false)),
            drainer: None,
            drainer_slot: None,
            scope: 0,
            fiber_owned: false,
            more_scopes: Vec::new(),
        })],
        ..FiberCtx::default()
    };

    // Schedule the fiber in: its eager scope becomes live; the host's parks into the ctx.
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.eager_scheds.len(), 1);
    assert!(
        Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &fiber_sched),
        "the fiber's eager scope is now live"
    );
    assert!(
        Arc::ptr_eq(&ctx.eager_scheds[0].as_ref().unwrap().sched, &host_sched),
        "the host's scope parked into the ctx"
    );

    // Park the fiber out: the host's scope is restored; the fiber keeps its own.
    vm.swap_ctx(&mut ctx);
    assert!(
        Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &host_sched),
        "host scope restored"
    );
    assert!(Arc::ptr_eq(
        &ctx.eager_scheds[0].as_ref().unwrap().sched,
        &fiber_sched
    ));
}

/// D2b companion to [`swap_ctx_leaves_heap_untouched_for_cooperative_fiber`]: `swap_ctx` never
/// moves `out` (only a task switch, `swap_task`, does), for any context, and never moves the
/// module table (M3.1: every context holds the run's one table). The `owns_side: false` side
/// state is [`swap_ctx_leaves_side_state_on_the_shell_without_owns_side`].
#[test]
fn swap_ctx_moves_neither_the_module_table_nor_out_without_owns_side() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"host-out");
    let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
    vm.module_objs = Arc::new(crate::vm::module_table::ModuleTable::of(&[host_mod]));
    let mut ctx = FiberCtx::default();
    vm.swap_ctx(&mut ctx);
    // `out` stays on the shell: `swap_ctx` does not move the output stream.
    assert_eq!(vm.out, b"host-out");
    assert!(
        ctx.out.is_empty(),
        "swap_ctx must not move the output stream"
    );
    assert_eq!(
        vm.module_objs.get(0),
        Some(host_mod),
        "the module table stays"
    );
    assert_eq!(ctx.module_objs.get(0), None);
}

/// M1.6 — a context with `owns_side: false` owns no side state, so `swap_ctx`
/// leaves the shell's `executors`, `str_intern`, `pending_connect` and `poll_*`
/// fields on the shell, and the context's own fields stay empty.
#[test]
fn swap_ctx_leaves_side_state_on_the_shell_without_owns_side() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
    let mut vm = Vm::new(Arc::new(empty_program()));
    let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
    let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
    vm.executors = vec![host_exec];
    vm.str_intern.insert(0x10, host_str);
    vm.pending_connect = Some(ConnectInProgress {
        stream,
        key: 7,
        in_flight: Arc::new(AtomicBool::new(false)),
        span: Span::RUNTIME,
    });
    vm.poll_timed_out = true;
    let deadline = std::time::Instant::now();
    vm.poll_deadline = Some(deadline);
    vm.poll_partial = Some(2);
    vm.poll_written = Some(3);
    let mut ctx = FiberCtx::default();
    assert!(!ctx.owns_side);
    for _ in 0..2 {
        vm.swap_ctx(&mut ctx);
        assert_eq!(vm.executors, vec![host_exec]);
        assert_eq!(vm.str_intern.get(&0x10), Some(&host_str));
        assert_eq!(vm.pending_connect.as_ref().map(|c| c.key), Some(7));
        assert!(vm.poll_timed_out);
        assert_eq!(vm.poll_deadline, Some(deadline));
        assert_eq!(vm.poll_partial, Some(2));
        assert_eq!(vm.poll_written, Some(3));
        assert!(ctx.executors.is_empty() && ctx.str_intern.is_empty());
        assert!(ctx.pending_connect.is_none());
        assert!(!ctx.poll_timed_out.load(Ordering::Relaxed));
        assert!(ctx.poll_deadline.is_none() && ctx.poll_partial.is_none());
        assert!(ctx.poll_written.is_none());
    }
}

/// M3.2 — a core holds the sent value itself, so a value whose only holder is a core survives
/// a stop of the world (`CHEZZI_GC_STRESS`: a collection at every instruction boundary, on every
/// thread). One row per holder, each value reachable from that holder alone while `churn` runs:
/// - `queue`: a channel buffer (the sender's frame is gone);
/// - `atomic`: an `Atomic`;
/// - `slot`: the slot of a parked receiver (the giver's frame is gone, the receiver is parked);
/// - `sealed`: a sealed Task channel, read by `t.get()` only after the churn;
/// - `door`: the slot of a receiver blocked in place on a popped temporary
///   (`cc.recv().recv()`): the GC door's pin of that handle is the only root, once the job's own
///   frame is gone. A job that sends before `main` waits parks with an offer instead, and its
///   frame then roots the value: the row covers the pin only on the slot path.
/// - `offer`: the offer of a parked sender. On the M:N path the offer cannot be the only root:
///   `park_send` keeps the value on the parked fiber's stack. The row covers the hand-off only.
///
/// Each row reads a size AND a key of a 51-key map, so a swept slot that a `churn` list reused
/// cannot pass. Fix round 1 checked each row RED with its one walk disabled (Channel arm of
/// `Heap::children`: queue, slot, sealed; Atomic arm: atomic; the door pin: door, at T=2). The
/// `door` and `slot` rows reach RED only on some runs with their pin off: the collection must land
/// in the window where the pin is the only root.
#[test]
fn a_value_held_only_by_a_core_survives_a_stop() {
    let src = r#"import Atomic, Executor from std.concurrency

fn churn() -> int:
    s := 0
    for i in 0..200:
        xs := [i, i + 1]
        s = s + xs[1]
    return s

fn big() -> Map[str, int]:
    m := {"a": 1}
    for i in 0..50:
        m["k{i}"] = i
    return m

fn fill(ch: Channel[Map[str, int]]):
    m := big()
    ch.send(m)
    m["b"] = 2

fn put(a: Atomic[Map[str, int]]):
    m := big()
    a.store(m)
    m["b"] = 2

fn give(rv: Channel[Map[str, int]]):
    m := big()
    rv.send(m)
    m["b"] = 2

fn give_then_open(rv: Channel[Map[str, int]], gate: Channel[int]) -> int:
    give(rv)
    _ := churn()
    gate.send(1)
    return 0

fn take(rv: Channel[Map[str, int]], gate: Channel[int], out: Channel[int]):
    r := rv.recv() else: panic("rv closed")
    _ := gate.recv()
    out.send(r.len() * 100 + r["k49"])

fn offer(rv: Channel[Map[str, int]], ready: Channel[int], done: Channel[int], out: Channel[int]):
    m := {"a": 1}
    ready.send(1)
    rv.send(m)
    _ := done.recv()
    out.send(m.len())

fn take_offer(rv: Channel[Map[str, int]], ready: Channel[int], done: Channel[int]):
    _ := ready.recv()
    _ := churn()
    r := rv.recv() else: panic("rv closed")
    r["b"] = 2
    done.send(1)

fn job_value() -> Map[str, int]:
    return big()

fn send_chan(cc: Channel[Channel[Map[str, int]]]):
    rv := Channel[Map[str, int]](0)
    cc.send(rv)
    _ := churn()
    rv.send(big())

fn door_job(cc: Channel[Channel[Map[str, int]]], gate: Channel[int]) -> int:
    send_chan(cc)
    _ := churn()
    gate.send(1)
    return 0

fn main():
    ch := Channel[Map[str, int]](1)
    fill(ch)
    _ := churn()
    q := ch.recv() else: panic("ch closed")
    print("queue", q.len(), q["k49"])
    a := Atomic({"z": 0})
    put(a)
    _ := churn()
    l := a.load()
    print("atomic", l.len(), l["k49"])
    rv := Channel[Map[str, int]](0)
    gate := Channel[int](1)
    out := Channel[int](1)
    parallel:
        spawn take(rv, gate, out)
        spawn give_then_open(rv, gate)
    got := out.recv() else: panic("out closed")
    print("slot", got)
    rv2 := Channel[Map[str, int]](0)
    ready := Channel[int](1)
    done := Channel[int](1)
    out2 := Channel[int](1)
    parallel:
        spawn offer(rv2, ready, done, out2)
        spawn take_offer(rv2, ready, done)
    got2 := out2.recv() else: panic("out2 closed")
    print("offer", got2)
    ex := Executor()
    t := ex.submit(fn(): job_value())
    ex.shutdown()
    _ := churn()
    match t.get():
        ?m: print("sealed", m.len(), m["k49"])
        !e: print(e.message())
    cc := Channel[Channel[Map[str, int]]](1)
    gate := Channel[int](1)
    ex2 := Executor()
    _t2 := ex2.submit(fn(): door_job(cc, gate))
    rv3 := cc.recv() else: panic("cc closed")
    r3 := rv3.recv() else: panic("rv3 closed")
    _ := gate.recv()
    _ := churn()
    print("door", r3.len(), r3["k49"])
    ex2.shutdown()

main()
"#;
    let (out, _) = run_with_cfg(src, true);
    assert_eq!(
        out.unwrap_or_else(|e| panic!("runtime error under GC stress: {e}")),
        "queue 52 49\natomic 52 49\nslot 5249\noffer 2\nsealed 51 49\ndoor 51 49\n"
    );
}
