//! VM tests: MnSched mechanics on hand-built fibers, flat scheduler scopes, local queues, channel
//! state.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ---- D2b MnSched scheduler mechanics (Step 2 — hand-built fibers, no bytecode) ----

/// TICKET-096 review finding — a delivered cut's floor indexes the RUNNING fiber's `nurseries`
/// stack (per-fiber, swapped by `swap_ctx`), but `Vm::cut` lives on the worker-shell `Vm` and is
/// not carried in `FiberCtx`. A floor left by the fiber that just died on this shell must not
/// leak into the next fiber scheduled in here, or an unrelated `recover:` in that fiber is wrongly
/// bypassed by `run_until`'s `owner_bypass` check. `run_one_fiber` must reset it on every swap-in,
/// exactly like a `Cut::Cancelled` (TICKET-195 merged both into `Vm::cut`).
#[test]
fn run_one_fiber_resets_a_delivered_cut_left_by_the_previous_fiber() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    // Simulate a floor left dangling by a fiber that died via the `owner_bypass` path (uncaught —
    // never reached the catch arm that clears it).
    vm.cut = Some(block::Cut::Delivered { floor: Some(3) });
    let mut fiber = mk_pending_fiber(0);
    vm.run_one_fiber(&mut fiber, Span::RUNTIME, Slice::Fresh);
    assert_eq!(
        vm.cut, None,
        "a stale delivered cut must not survive into the next fiber scheduled on this shell"
    );
}

/// TICKET-185 — a receiver-side (`recv_wake`) wake requeues exactly the parked parties whose
/// `Pending` left QUEUED, whatever their entry type: a committed sender and a receiver whose slot
/// was filled wake; a queued receiver and a queued sender stay parked (DEC-028's pair-spin guard).
#[test]
fn a_settled_wake_requeues_only_committed_parties() {
    use crate::vm::core::{Pending, PendingOp};
    let with = |mut f: Fiber, committed: bool| {
        let p = Pending::new();
        if committed {
            assert!(p.try_commit(0));
        }
        f.pending = Some(PendingOp::new(p, Vec::new()));
        f
    };
    let sched = mk_sched(4);
    let core = cap0_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2), mk_fiber(3)]);
    let f0 = with(take_run(&sched), true);
    let f1 = with(take_run(&sched), false);
    let f2 = with(take_run(&sched), true);
    let f3 = with(take_run(&sched), false);
    {
        let mut c = sched.lock();
        let b = c.parked.entry(key).or_default();
        b.push(ParkedEntry::Send(f0));
        b.push(ParkedEntry::Recv(f1));
        b.push(ParkedEntry::Recv(f2));
        b.push(ParkedEntry::Send(f3));
        c.parked_n += 4;
        c.running -= 4;
    }
    sched.recv_wake(key, &core);
    assert_eq!(sched.lock().parked_n, 2);
    let mut woke = vec![take_run(&sched).task_index, take_run(&sched).task_index];
    woke.sort();
    assert_eq!(woke, vec![0, 2], "only the committed parties wake");
    let c = sched.lock();
    let kept = &c.parked[&key];
    assert_eq!(kept.len(), 2);
    assert!(matches!(&kept[0], ParkedEntry::Recv(f) if f.task_index == 1));
    assert!(matches!(&kept[1], ParkedEntry::Send(f) if f.task_index == 3));
}

/// TICKET-185 — `MnSched::park` on a rendezvous channel publishes a SLOT bound to the parked
/// fiber, in the same hold as its gap check: a sender then sees a receiver
/// (`send_ready_for(Some(0), None)`), one `give` fills that slot, and the fiber's `pending`
/// settles to `Got` with the value. No anonymous counter, and no wake of a parked sender.
#[test]
fn rendezvous_park_publishes_a_bound_slot() {
    let sched = mk_sched(2);
    let core = cap0_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    sched.park(key, Arc::clone(&core), f0);
    assert_eq!(sched.lock().parked_n, 1);
    assert!(
        core.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send_ready_for(Some(0), None)
    );
    assert!(
        core.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .give(Value::int(5), None)
    );
    let op = {
        let mut c = sched.lock();
        match c.parked.get_mut(&key).and_then(|b| b.pop()) {
            Some(ParkedEntry::Recv(mut f)) => f.pending.take().expect("the park bound a slot"),
            _ => panic!("the receiver is parked as a Recv entry"),
        }
    };
    assert!(matches!(
        op.settle(),
        crate::vm::core::Settled::Got(0, v) if v == Value::int(5)
    ));
}

/// TICKET-028 — the pair-spin guard: two `wait:` fibers each holding ONE recv arm on the same
/// cap-0 channel must NOT wake each other (that would livelock forever with no `deadlock` fault).
#[test]
fn rendezvous_wait_recv_arms_do_not_wake_each_other() {
    let sched = mk_sched(2);
    let core = cap0_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched);
    let f1 = take_run(&sched);
    sched.park_wait(vec![(key, Arc::clone(&core), false)], f0);
    sched.park_wait(vec![(key, Arc::clone(&core), false)], f1);
    assert_eq!(sched.lock().parked_n, 2);
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 0);
}

/// TICKET-028 — a `send` on a rendezvous `Channel[int](0)` blocks until a receiver is already
/// waiting, and the run completes rather than false-`deadlock`ing.
#[test]
fn rendezvous_send_blocks_until_a_receiver_arrives() {
    let src = "\
fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            i := 0
            while i < 3:
                ch.send(i)
                print(\"sent {i}\")
                i = i + 1
        spawn:
            i := 0
            while i < 3:
                v := ch.recv()
                print(\"got {v}\")
                i = i + 1

main()
";
    let out = run_capture(src).expect("rendezvous send/recv should not deadlock");
    for i in 0..3 {
        assert!(
            out.contains(&format!("sent {i}")),
            "missing 'sent {i}' in {out:?}"
        );
        assert!(
            out.contains(&format!("got {i}")),
            "missing 'got {i}' in {out:?}"
        );
    }
}

/// D2b/U1: `take_runnable` pops the shared run queue in FIFO order and marks each popped fiber
/// `running`.
#[test]
fn mnsched_take_runnable_pops_in_order_and_counts_running() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    assert_eq!(take_run(&sched).task_index, 0);
    assert_eq!(take_run(&sched).task_index, 1);
    assert_eq!(sched.lock().running, 2);
}

/// D4a: `runnable` is the authoritative count of runnable (queued, not running/parked/done)
/// fibers — in D2b's single-queue world it mirrors `runq.len()` exactly, but it is maintained as
/// an atomic so D4b's per-worker split (local rings + global, no single queue to `.len()`) can
/// keep using it for the deadlock predicate. This pins the bump/decrement discipline: seed +N,
/// pop −1 (runnable→running), park unchanged (running→parked), send_wake +woken (parked→ready),
/// finish unchanged (running→done).
#[test]
fn mnsched_runnable_tracks_single_queue() {
    let sched = mk_sched(3);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2)]);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        3,
        "seed bumps runnable"
    );
    let f0 = take_run(&sched);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "pop transitions runnable→running"
    );
    sched.park(key, Arc::clone(&core), f0);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "park transitions running→parked (no change)"
    );
    sched.send_wake(key, &core, Value::int(7));
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        3,
        "send_wake transitions parked→ready"
    );
    let f = take_run(&sched);
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 2);
    sched.finish(
        f.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "finish transitions running→done (no change)"
    );
    // The invariant: with no per-worker locals populated, runnable == global.len() at quiescence.
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        sched.lock().global.len()
    );
}

/// Per-connection spawn: `inject` adds a task to a LIVE sched — it grows `total` + `slots`
/// (so the dynamically-spawned handler gets a Decision-F outcome slot) and queues the fiber
/// runnable, all under one core lock (the `complete_offload` twin). This is what lifts the
/// "fixed total — no spawn-after-join" restriction.
#[test]
fn mnsched_inject_grows_total_and_slots() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]); // total 1, slots.len 1, runnable 1
    sched.inject(mk_pending_fiber(1), 0);
    let c = sched.lock();
    assert_eq!(c.scopes[0].total, 2, "inject grows the scope's total");
    assert_eq!(c.slots.len(), 2, "inject grows the outcome-slot vec");
    assert_eq!(c.global.len(), 2, "the injected fiber is queued runnable");
    drop(c);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "inject runnable-accounts the new fiber"
    );
}

/// Per-connection spawn: injecting a runnable fiber into a sched where every existing fiber is
/// parked must VETO the deadlock predicate — `total += 1` is paired with `runnable += 1` under
/// one lock, so the new fiber is immediately accounted and `is_deadlocked` sees `runnable > 0`.
#[test]
fn mnsched_inject_does_not_false_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1, runnable 0
    let core = empty_core();
    sched.park(core_key(&core), Arc::clone(&core), f0); // parked 1, running 0, runnable 0 → deadlock
    {
        let c = sched.lock();
        assert!(
            sched.is_deadlocked(&c),
            "all parked, nothing runnable/inflight = deadlock"
        );
    }
    sched.inject(mk_pending_fiber(1), 0); // runnable 1
    {
        let c = sched.lock();
        assert!(
            !sched.is_deadlocked(&c),
            "an injected runnable fiber vetoes the deadlock fire"
        );
    }
}

// ----- Cross-nursery flat scheduler (M:N): JoinScope / scope-scoped owner stop -----

/// `register_scope` appends a `JoinScope` and grows the flat slots: scope 0 (built by `new`) at
/// base 0, the next at base = previous total, slots length = sum of totals.
#[test]
fn mn_register_scope_appends_and_offsets_slots() {
    let sched = mk_sched(2); // scope 0: total 2, base 0
    let s1 = sched.register_scope(3, Arc::new(AtomicBool::new(false)), Vec::new());
    assert_eq!(s1, 1, "second scope id");
    let c = sched.lock();
    assert_eq!(c.scopes.count(), 2);
    assert_eq!(c.scopes[0].base_index, 0);
    assert_eq!(
        c.scopes[1].base_index, 2,
        "scope 1 starts after scope 0's 2 slots"
    );
    assert_eq!(c.slots.len(), 5, "flat slots grew to 2 + 3");
}

/// TICKET-199 (H1) — a retired scope id is never reissued and reads as absent. A worker that still
/// holds it after dropping the core lock (`mn_worker_loop`'s `cancel_drain` after `finish`) must
/// neither panic nor drain the NEXT nursery's parked fibers. Under the old `id = len()` plus
/// pop rule, the second registration reissued `a`.
#[test]
fn retired_scope_id_is_never_reissued_and_reads_as_absent() {
    let sched = mk_sched(1);
    let a = sched.register_scope_seeded(Arc::new(AtomicBool::new(false)), Vec::new(), Vec::new());
    sched.retire_scope(a);
    let b = sched.register_scope_seeded(Arc::new(AtomicBool::new(false)), Vec::new(), Vec::new());
    assert_ne!(b, a, "a retired id must not be reissued");
    assert!(sched.lock().scopes.get(a).is_none());
    assert!(sched.lock().scope_family(a).is_empty());
    // A parked fiber of a live scope (b) stays parked when a stale holder drains `a`.
    let mut f = mk_fiber(0);
    f.scope_id = b;
    {
        let mut c = sched.lock();
        c.parked.insert(1, vec![ParkedEntry::Recv(f)]);
        c.parked_n = 1;
    }
    sched.cancel_drain(a);
    let c = sched.lock();
    assert_eq!(
        c.parked_n, 1,
        "draining a retired id must not drain another scope"
    );
    assert!(c.global.is_empty());
}

/// TICKET-199 — `retire_scope` removes any done scope, not only the last one (scope order on one
/// sched is NOT LIFO, DEC-103). A retired middle scope leaves a hole in `slots`; retiring the last
/// scope truncates `slots` to the newest live scope's end.
#[test]
fn retire_scope_removes_a_done_scope_that_is_not_last() {
    let sched = mk_sched(1); // root: slots 0..1
    let a = sched.register_scope(2, Arc::new(AtomicBool::new(false)), Vec::new()); // 1..3
    let b = sched.register_scope(3, Arc::new(AtomicBool::new(false)), Vec::new()); // 3..6
    sched.lock().scopes[a].done = 2;
    sched.retire_scope(a);
    {
        let c = sched.lock();
        assert!(c.scopes.get(a).is_none(), "a done middle scope is retired");
        assert!(c.scopes.get(b).is_some(), "the later scope stays");
        assert_eq!(c.slots.len(), 6, "a middle removal leaves a hole");
    }
    sched.retire_scope(b);
    assert!(
        sched.lock().scopes.get(b).is_some(),
        "an unfinished scope is not retired"
    );
    sched.lock().scopes[b].done = 3;
    sched.retire_scope(b);
    let c = sched.lock();
    assert!(c.scopes.get(b).is_none());
    assert_eq!(c.slots.len(), c.scopes.slot_end());
    assert_eq!(c.slots.len(), 1, "slots shrink back to the root's range");
}

/// Scope-scoped owner stop: an owner whose OWN scope is done returns `Stop` (queue empty) even
/// while ANOTHER scope is still in flight (a running fiber → no global terminate / deadlock) — the
/// load-bearing case-A behavior (the nested owner returns the instant its scope completes, having
/// drained the global queue meanwhile). The owner of the NOT-done scope, by contrast, does not stop
/// on the same state (it would park to wait for its scope) — asserted via the fast-path scalars.
#[test]
fn mn_owner_stops_on_own_scope_not_global() {
    let sched = mk_sched(1); // scope 0: total 1
    let _s1 = sched.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: total 1
    {
        let mut c = sched.lock();
        c.scopes[0].done = 1; // scope 0 complete
        c.running = 1; // scope 1's fiber is running on some worker → not terminate, not deadlock
    }
    // The owner of the DONE scope 0 stops immediately (queue empty, its scope done).
    assert!(
        matches!(sched.take_runnable_unattached(0, 1, 0), Take::Stop),
        "owner of done scope 0 stops"
    );
    // The global terminate was NOT set (scope 1 still in flight), so the stop was scope-scoped, not
    // a global teardown — a SENTINEL would keep going (verified: terminate is still false).
    assert!(
        !sched.lock().terminate,
        "owner stop is scope-scoped, not a global terminate"
    );
}

/// `finish` routes the per-fiber outcome to the FIBER's scope `done` + the flat slot, and sets
/// global `terminate` only when EVERY scope is done.
#[test]
fn mn_finish_routes_done_to_scope_via_fiber() {
    let sched = mk_sched(1); // scope 0: total 1, base 0
    let _s1 = sched.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: total 1, base 1
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let _f0 = take_run(&sched); // scope 0's fiber (task_index 0)
    let _f1 = take_run(&sched); // queued as task_index 1
    sched.finish(
        1,
        1,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    ); // finish scope 1's fiber (flat slot 1)
    {
        let c = sched.lock();
        assert_eq!(c.scopes[1].done, 1, "scope 1 done bumped");
        assert_eq!(c.scopes[0].done, 0, "scope 0 untouched");
        assert!(c.slots[1].is_some(), "flat slot 1 set");
        assert!(!c.terminate, "not all scopes done yet");
    }
    sched.finish(
        0,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert!(
        sched.lock().terminate,
        "global terminate once every scope is done"
    );
}

/// `take_scope_slots` drains only ONE scope's contiguous sub-range in task order, leaving others.
#[test]
fn mn_take_scope_slots_drains_only_its_range() {
    let sched = mk_sched(2); // scope 0: base 0, total 2
    let _s1 = sched.register_scope(2, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: base 2, total 2
    {
        let mut c = sched.lock();
        for i in 0..4 {
            c.slots[i] = Some(TaskOutcome::Cancelled {
                out: Vec::new(),
                stderr: Vec::new(),
            });
        }
    }
    let s0 = sched.take_scope_slots(0);
    assert_eq!(s0.len(), 2, "scope 0 sub-range");
    assert!(s0.iter().all(|x| x.is_some()));
    {
        let c = sched.lock();
        assert!(
            c.slots[0].is_none() && c.slots[1].is_none(),
            "scope 0 slots taken"
        );
        assert!(
            c.slots[2].is_some() && c.slots[3].is_some(),
            "scope 1 slots intact"
        );
    }
}

/// §6d M:N wait-park (TDD step 2): a plain `recv` park (the 1-key `ParkedEntry::Recv` case)
/// round-trips through the refactored `parked` map — park a fiber, `send_wake` it, and it lands
/// back on `global` as Ready with `parked_n` returned to 0. Pins that the refactor keeps the
/// recv-park path byte-identical at the scheduler level.
#[test]
fn mn_parked_entry_recv_roundtrips() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let core = empty_core();
    let key = core_key(&core);
    sched.park(key, Arc::clone(&core), f0); // parked 1, running 0
    assert_eq!(
        sched.lock().parked_n,
        1,
        "recv park accounts one parked fiber"
    );
    sched.send_wake(key, &core, Value::int(5));
    assert_eq!(
        sched.lock().parked_n,
        0,
        "send_wake un-parks the recv fiber"
    );
    let woke = take_run(&sched);
    assert_eq!(
        woke.task_index, 0,
        "the recv-parked fiber is back on the run queue"
    );
}

/// §6d M:N wait-park (TDD step 3): a wait fiber filed under keys [k1,k2,k3] as ONE shared
/// `WaitPark` token. A `send_wake` on the middle key claims it exactly once (moves the single
/// fiber to Ready, `claimed==true`, `parked_n` 1→0) AND sweeps the stale tokens out of k1 and k3.
/// A follow-up `send_wake` on either swept key is a no-op (no double-wake, no panic).
#[test]
fn mn_wait_park_first_waker_claims_and_sweeps() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let c1 = empty_core();
    let c2 = empty_core();
    let c3 = empty_core();
    let (k1, k2, k3) = (core_key(&c1), core_key(&c2), core_key(&c3));
    sched.park_wait(
        vec![
            (k1, Arc::clone(&c1), false),
            (k2, Arc::clone(&c2), false),
            (k3, Arc::clone(&c3), false),
        ],
        f0,
    );
    assert_eq!(
        sched.lock().parked_n,
        1,
        "a wait fiber on N keys counts as ONE parked fiber"
    );
    // Wake on the MIDDLE key.
    sched.send_wake(k2, &c2, Value::int(99));
    {
        let c = sched.lock();
        assert_eq!(
            c.parked_n, 0,
            "claiming the wait fiber returns parked_n to 0"
        );
        assert!(
            c.parked.get(&k1).is_none_or(|v| v.is_empty()),
            "k1 token swept"
        );
        assert!(
            c.parked.get(&k3).is_none_or(|v| v.is_empty()),
            "k3 token swept"
        );
    }
    assert_eq!(
        sched.lock().global.len(),
        1,
        "exactly one fiber re-queued by the wake"
    );
    let woke = take_run(&sched);
    assert_eq!(
        woke.task_index, 0,
        "the wait fiber is back on the run queue exactly once"
    );
    // A later send to a swept key must be a clean no-op (the token is gone): no new runnable fiber.
    sched.send_wake(k1, &c1, Value::int(1));
    sched.send_wake(k3, &c3, Value::int(3));
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no double-wake from swept buckets");
    assert_eq!(
        c.global.len(),
        0,
        "no second wake of the already-moved fiber"
    );
}

/// WAIT-2/WAIT-3 unit guard — the timed-park files the timer channel as an ORDINARY arm bucket,
/// so the timer's own deadline `send_wake` claims+sweeps the fiber exactly like a data send. A
/// LATE `send_wake` on the swept data key (a sibling that landed after the alarm already won) is a
/// clean no-op. Pins the "timer-arm-as-bucket" design invariant the VM timed-park relies on.
#[test]
fn mn_wait_park_timer_self_send_claims_then_late_send_noop() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let timer_core = empty_core(); // stands in for the timer channel's bucket
    let data_core = empty_core();
    let (timer_key, data_key) = (core_key(&timer_core), core_key(&data_core));
    sched.park_wait(
        vec![
            (timer_key, Arc::clone(&timer_core), false),
            (data_key, Arc::clone(&data_core), false),
        ],
        f0,
    );
    assert_eq!(
        sched.lock().parked_n,
        1,
        "wait on [timer,data] is ONE parked fiber"
    );
    // The timer's deadline job fires: `send_wake(true)` on the timer key claims + sweeps.
    sched.send_wake(timer_key, &timer_core, Value::bool(true));
    {
        let c = sched.lock();
        assert_eq!(
            c.parked_n, 0,
            "the timer deadline send claims the wait fiber"
        );
        assert!(
            c.parked.get(&data_key).is_none_or(|v| v.is_empty()),
            "data_key token swept by the timer win"
        );
        assert_eq!(c.global.len(), 1, "exactly one fiber re-queued");
    }
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "the wait fiber is back on the run queue exactly once"
    );
    // A LATE send on the swept data key (a sibling that lost the race) is a clean no-op.
    sched.send_wake(data_key, &data_core, Value::int(9));
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no double-wake from the swept data bucket");
    assert_eq!(
        c.global.len(),
        0,
        "the late send does not re-wake the already-moved fiber"
    );
}

/// §6d M:N wait-park: `close_wake` claims+sweeps a wait fiber identically to `send_wake` (a close
/// has no value but must still unblock the waiter so it re-polls and skips the closed arm).
#[test]
fn mn_wait_park_close_wake_claims_and_sweeps() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    let c1 = empty_core();
    let c2 = empty_core();
    let (k1, k2) = (core_key(&c1), core_key(&c2));
    sched.park_wait(
        vec![(k1, Arc::clone(&c1), false), (k2, Arc::clone(&c2), false)],
        f0,
    );
    assert_eq!(sched.lock().parked_n, 1);
    sched.close_wake(k1, &c1);
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "close_wake un-parks the wait fiber");
        assert!(
            c.parked.get(&k2).is_none_or(|v| v.is_empty()),
            "k2 token swept by close"
        );
    }
    assert_eq!(take_run(&sched).task_index, 0);
}

/// §6d M:N wait-park: a lone wait-parked fiber on N empty keys with nothing running/runnable/
/// inflight IS a deadlock — `park_wait` must increment `parked_n` so `is_deadlocked` fires (the
/// accounting choice: one fiber, +1 regardless of key count).
#[test]
fn mn_wait_park_lone_fiber_is_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    use crate::vm::core::{Pending, PendingOp};
    let mut f0 = take_run(&sched);
    let c1 = empty_core();
    let c2 = empty_core();
    // TICKET-236: the verdict asks a parked fiber through its `PendingOp`, as production sets it.
    f0.pending = Some(PendingOp::new(
        Pending::new(),
        vec![(Arc::clone(&c1), 0, false), (Arc::clone(&c2), 1, false)],
    ));
    sched.park_wait(
        vec![
            (core_key(&c1), Arc::clone(&c1), false),
            (core_key(&c2), Arc::clone(&c2), false),
        ],
        f0,
    );
    let c = sched.lock();
    assert!(
        sched.is_deadlocked(&c),
        "a lone wait-parked fiber with no sender is a deadlock"
    );
}

/// W7-2 — the `close()`-vs-`wait:`-park lost wakeup. `op_wait_poll` treats a closed+EMPTY recv arm
/// as DEAD (skipped, counted only toward `all_closed`), so if EVERY arm is dead the poll faults
/// "wait: all channels closed". `park_wait`'s gap re-check must classify arms the SAME way: a
/// `close()` landing between the empty poll and the park runs `close_wake` against a bucket that is
/// still empty, so the re-check is the only thing left that can see it. Pre-fix the recv predicate
/// was `!g.is_empty()` alone → not ready → park on a key nothing will ever wake → the deadlock
/// detector (correctly) reaps a genuinely unreachable fiber, i.e. a SPURIOUS `deadlock:` fault.
#[test]
fn mn_park_wait_all_recv_arms_closed_requeues_instead_of_parking() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let c1 = empty_core();
    let c2 = empty_core();
    for c in [&c1, &c2] {
        c.q.lock().unwrap().closed = true;
    }
    sched.park_wait(
        vec![
            (core_key(&c1), Arc::clone(&c1), false),
            (core_key(&c2), Arc::clone(&c2), false),
        ],
        f0,
    );
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "an all-dead wait must NOT park");
        assert_eq!(c.global.len(), 1, "it is requeued Ready to re-poll");
    }
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "the fiber re-runs WaitPoll, which faults `wait: all channels closed`"
    );
}

/// W7-2 anti-over-fire fence (green before AND after the fix): ONE closed arm among LIVE arms must
/// still PARK. Treating a closed+empty recv arm as *ready* was tried and reverted (parity-perf-0):
/// the re-poll SKIPS that arm, finds the live one still empty, and re-parks → requeue→re-poll→re-park
/// live-lock. Only an ALL-dead wait may requeue (its re-poll terminates in the all-closed fault).
/// Also pins that the deadlock detector stays sharp on the partially-closed park.
#[test]
fn mn_park_wait_one_closed_one_live_still_parks_and_is_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    use crate::vm::core::{Pending, PendingOp};
    let mut f0 = take_run(&sched);
    let closed = empty_core();
    closed.q.lock().unwrap().closed = true;
    let live = empty_core();
    // TICKET-236: the verdict asks a parked fiber through its `PendingOp`, as production sets it.
    f0.pending = Some(PendingOp::new(
        Pending::new(),
        vec![
            (Arc::clone(&closed), 0, false),
            (Arc::clone(&live), 1, false),
        ],
    ));
    sched.park_wait(
        vec![
            (core_key(&closed), Arc::clone(&closed), false),
            (core_key(&live), Arc::clone(&live), false),
        ],
        f0,
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 1, "a live arm remains — the fiber parks");
    assert!(
        sched.is_deadlocked(&c),
        "a lone wait-parked fiber with no possible waker is still a deadlock"
    );
}

/// TICKET-236 — a parked fiber is asked about its channel, as a demoted waiter and a blocked
/// party are. A close that landed after the park (its wake has not run yet) makes the fiber
/// resumable, so the sched is not deadlocked.
#[test]
fn a_parked_recv_whose_channel_closed_is_not_judged_stuck() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    let c1 = empty_core();
    sched.park(core_key(&c1), Arc::clone(&c1), f0);
    assert!(
        sched.is_deadlocked(&sched.lock()),
        "a lone parked fiber on an open, empty channel is a deadlock"
    );
    c1.q.lock().unwrap().closed = true;
    assert!(
        !sched.is_deadlocked(&sched.lock()),
        "the channel closed: the parked fiber can resume"
    );
}

/// W7-2 end-to-end (RACY — loop it): a `close()` racing a sibling's `wait:` park must never produce
/// a spurious `deadlock:` fault. Pre-fix the M:N engine lost the wakeup whenever the `close` landed
/// inside the poll→park window (the since-removed cooperative engine never did, which is how the
/// divergence was spotted).
///
/// PRE-FIX MEASURED (this exact 200-iteration loop, `park_wait` reverted to the pre-fix predicate,
/// `cargo test --lib -- --test-threads=1`, 12-core box): **43 / 45 / 50 / 56 failures out of 200**
/// over four consecutive runs — ~25%, so ≥1 failure is essentially certain and the fence really is
/// RED without the fix. It is a RATE, not a fixed count; quote the range, not a single number.
#[test]
fn wait_park_close_race_no_spurious_deadlock_parallel() {
    let src = "fn w(a: Channel[int]):\n    r := recover:\n        wait:\n            v := a.recv(): print(\"got\", v)\n    print(\"waiter done\")\nfn main():\n    a := Channel[int]()\n    parallel:\n        spawn w(a)\n        spawn: a.close()\n    print(\"end\")\nmain()\n";
    let mut failures = Vec::new();
    for i in 0..200 {
        match run_capture(src) {
            Ok(out) => {
                if !out.contains("waiter done") || !out.contains("end") {
                    failures.push(format!("iter {i}: missing lines in {out:?}"));
                }
            }
            Err(e) => failures.push(format!("iter {i}: {}", e.message)),
        }
    }
    assert!(
        failures.is_empty(),
        "close() racing a wait: park lost the wakeup (pre-fix this loop failed 43-56/200 across four \
         runs with `deadlock: every task in this parallel: block is blocked...` on a 12-core box); \
         {} failures, first: {}",
        failures.len(),
        failures.first().map_or("", |s| s.as_str())
    );
}

/// D4b: a `LocalQ` pops `runnext` first (locality), then the ring in FIFO order, then `None`.
#[test]
fn localq_runnext_then_ring_order() {
    let mut q = LocalQ::new();
    q.ring.push_back(mk_fiber(1));
    q.ring.push_back(mk_fiber(2));
    q.runnext = Some(mk_fiber(0));
    assert_eq!(q.pop().unwrap().0.task_index, 0, "runnext runs first");
    assert_eq!(q.pop().unwrap().0.task_index, 1, "then ring FIFO");
    assert_eq!(q.pop().unwrap().0.task_index, 2);
    assert!(q.pop().is_none());
}

/// TICKET-199 (H2) — Go `inheritTime`: a `runnext` pick runs on the current slice (`Inherit`), a
/// ring pick on a fresh one (`Fresh`). `pop_seeded` reports `Inherit` exactly when it returns the
/// `runnext` fiber (DEC-167: the seeded pick follows the same rule as `pop`).
#[test]
fn localq_pop_inherits_the_slice_only_from_runnext() {
    let mut q = LocalQ::new();
    q.ring.push_back(mk_fiber(1));
    q.runnext = Some(mk_fiber(0));
    let (f, s) = q.pop().unwrap();
    assert_eq!((f.task_index, s), (0, Slice::Inherit));
    let (f, s) = q.pop().unwrap();
    assert_eq!((f.task_index, s), (1, Slice::Fresh));
    let rng = sched_seed::SeedRng::new();
    let mut inherited = 0;
    for _ in 0..200 {
        q.ring.push_back(mk_fiber(1));
        q.ring.push_back(mk_fiber(2));
        q.runnext = Some(mk_fiber(0));
        while let Some((f, s)) = q.pop_seeded(&rng) {
            let want = if f.task_index == 0 {
                inherited += 1;
                Slice::Inherit
            } else {
                Slice::Fresh
            };
            assert_eq!(s, want, "task {}", f.task_index);
        }
    }
    assert_eq!(
        inherited, 200,
        "every runnext fiber is picked once per round"
    );
}

/// D4b: `take_runnable(wid)` drains the worker's own `locals[wid]` BEFORE the shared global queue.
/// (In D4b nothing populates a local at runtime; this drives it directly to pin the search order
/// the D4c requeue/steal paths depend on.)
#[test]
fn take_runnable_prefers_local_over_global() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(1)]); // task 1 → global, runnable == 1
    sched.lock_local(0).ring.push_back(mk_fiber(0)); // task 0 → worker 0's local
    sched.runnable.fetch_add(1, Ordering::Relaxed); // keep the counter consistent (==2)
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "own local drained before the global queue"
    );
    assert_eq!(take_run(&sched).task_index, 1, "then the global queue");
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 0);
    assert_eq!(sched.lock().running, 2);
}

/// A trivial blocking-shaped native for the offload tests: double the first int arg. Off-heap-safe
/// (reads only a primitive arg, returns a primitive), so it can run on an [`OffloadHost`].
fn double_native(
    h: &mut dyn crate::native::Host,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    Ok(crate::native::NativeRet::Int(h.arg_int(0)? * 2))
}

/// A native that panics — stands in for a misclassified blocking fn that hits an `OffloadHost`
/// `unreachable!`, or any panic inside an offloaded call.
fn panic_native(
    _h: &mut dyn crate::native::Host,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    panic!("boom inside offloaded native")
}

/// D5 — a panic inside an offloaded native must NOT lose the fiber. If the pool job lets the panic
/// escape, `complete_offload` never runs: `inflight` stays pinned, the fiber's slot stays empty,
/// and the nursery hangs forever (the deadlock predicate is vetoed by `inflight > 0`). The job
/// must catch the panic, surface it as a fault on the fiber, and always re-enqueue → `inflight`
/// returns to 0 and the resumed fiber faults like an inline native panic.
#[test]
fn offload_native_panic_still_completes_and_faults() {
    let sched = Arc::new(mk_sched(1));
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running == 1
    let req = OffloadReq {
        func: panic_native,
        args: vec![],
        span: Span::RUNTIME,
        timer: None,
    };
    sched.offload(f0, req);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "panic in offloaded native lost the fiber (inflight pinned → hang)"
        );
        std::thread::yield_now();
    }
    // The fiber came back runnable carrying a fault to raise on resume.
    let f0 = match sched.take_runnable_unattached(0, 1, 0) {
        Take::Run(f, _) => f,
        Take::Stop => panic!("fiber not requeued after panicking offload"),
    };
    assert!(
        matches!(f0.resume_native, Some(Err(_))),
        "panic surfaced as a fault on the fiber"
    );
}

/// D5: an in-flight blocking offload must SUPPRESS the deadlock fire. The predicate is
/// `running==0 && runnable==0 && parked_n>0 && done<total` — but a fiber off in the blocking pool
/// (counted by `inflight`) is neither running, runnable, nor parked, and *will* come back runnable,
/// so `inflight>0` must veto the deadlock declaration (else a program that parks everyone while one
/// fiber blocks in `read_file` would falsely fault `deadlock`).
#[test]
fn deadlock_predicate_suppressed_by_inflight_offload() {
    let sched = mk_sched(2);
    let c = sched.lock();
    // running==0, runnable==0, one parked, none done: a real deadlock with no in-flight work.
    let mut c = c;
    c.parked_n = 1;
    assert!(
        sched.is_deadlocked(&c),
        "all-parked + nothing in flight = deadlock"
    );
    sched.inflight.fetch_add(1, Ordering::Relaxed);
    assert!(
        !sched.is_deadlocked(&c),
        "an in-flight blocking offload vetoes the deadlock fire"
    );
}

/// D5 owe #3 Path C (#1) — the deadlock false-positive fix. A demoted (blocked-in-callback) fiber
/// polls its OWN channel queue; a value a sibling already queued there is invisible to the
/// counter-only predicate (a `send` `push_back`s + notifies the channel condvar — it does NOT bump
/// `runnable`). Registering the demoted channel lets `is_deadlocked` peek it: a non-empty queue
/// means that fiber WILL pop + make progress (possibly waking a parked sibling), so an apparent
/// all-blocked quiesce is NOT a deadlock — don't fault an innocent parked sibling.
#[test]
fn deadlock_predicate_vetoed_by_queued_value_on_demoted_channel() {
    let sched = mk_sched(2);
    let core = empty_core();
    let mut c = sched.lock();
    // The #1 race: one demoted fiber (a registered waiter) + one parked sibling, nothing running /
    // runnable / inflight — the counter-only predicate fires (the false positive).
    c.parked_n = 1;
    let tok = c.register_waiter(recv_waiter(&core, vec![]));
    // A sibling already queued a value on the demoted fiber's channel: it will pop + progress.
    core.q.lock().unwrap().push(Value::int(7));
    assert!(
        !sched.is_deadlocked(&c),
        "a queued value on a demoted channel must veto the deadlock fire (#1 false-positive)"
    );
    // Drain it: now the demoted fiber truly has nothing queued → a real all-blocked deadlock.
    core.q.lock().unwrap().pop();
    assert!(
        sched.is_deadlocked(&c),
        "an empty demoted channel with all fibers blocked IS a genuine deadlock"
    );
    // Un-register restores the pre-demote predicate (no stale registry entry vetoing forever).
    c.unregister_waiter(tok);
    assert!(
        sched.is_deadlocked(&c),
        "after un-register the predicate is unchanged (still all-blocked)"
    );
}

/// W15-6 (TICKET-176) — a demoted recv settles `ClosedEmpty` on a closed channel, so a closed
/// channel under it is progress, not a deadlock.
#[test]
fn a_closed_channel_under_a_demoted_recv_vetoes_the_deadlock_verdict() {
    let sched = mk_sched(2);
    let core = empty_core();
    let mut c = sched.lock();
    c.parked_n = 1;
    let tok = c.register_waiter(recv_waiter(&core, vec![]));
    assert!(
        sched.is_deadlocked(&c),
        "an open empty demoted channel is a genuine deadlock"
    );
    core.q.lock().unwrap().closed = true;
    assert!(
        !sched.is_deadlocked(&c),
        "a closed channel under a demoted recv settles it (ClosedEmpty): no deadlock"
    );
    c.unregister_waiter(tok);
    assert!(
        sched.is_deadlocked(&c),
        "after the demoted recv leaves, the closed channel vetoes nothing"
    );
}

/// W15-6 (TICKET-176) — a demoted recv or `wait:` settles on a tripped `done_latch`.
#[test]
fn a_tripped_latch_under_a_demoted_recv_vetoes_the_deadlock_verdict() {
    let sched = mk_sched(2);
    let core = empty_core();
    let mut c = sched.lock();
    c.parked_n = 1;
    c.register_waiter(recv_waiter(&core, vec![]));
    core.done_latch.store(true, Ordering::Relaxed);
    assert!(
        !sched.is_deadlocked(&c),
        "a tripped latch under a demoted recv settles it: no deadlock"
    );
}

/// W15-6 (TICKET-176) — a demoted `wait:` returns "wait: all channels closed" once EVERY arm is
/// closed, so that state is progress.
#[test]
fn a_demoted_wait_with_every_arm_closed_vetoes_the_deadlock_verdict() {
    let sched = mk_sched(2);
    let (a, b) = (empty_core(), empty_core());
    let mut c = sched.lock();
    c.parked_n = 1;
    c.register_waiter(Waiter {
        wait: Arc::new(quiesce::PartyWait::Wait(
            vec![(Arc::clone(&a), false), (Arc::clone(&b), false)],
            None,
        )),
        cancel: vec![],
        fiber: true,
        scope: None,
        cut: false,
    });
    a.q.lock().unwrap().closed = true;
    b.q.lock().unwrap().closed = true;
    assert!(
        !sched.is_deadlocked(&c),
        "every arm of a demoted wait closed settles it (all channels closed): no deadlock"
    );
}

/// W15-6 (TICKET-176) — a demoted `wait:` SKIPS a closed arm, so one closed arm of two is not
/// progress. Goes red if the closed veto asks `any` arm instead of `all`, or asks per channel.
#[test]
fn a_demoted_wait_with_one_arm_still_open_reads_as_deadlocked() {
    let sched = mk_sched(2);
    let (a, b) = (empty_core(), empty_core());
    let mut c = sched.lock();
    c.parked_n = 1;
    c.register_waiter(Waiter {
        wait: Arc::new(quiesce::PartyWait::Wait(
            vec![(Arc::clone(&a), false), (Arc::clone(&b), false)],
            None,
        )),
        cancel: vec![],
        fiber: true,
        scope: None,
        cut: false,
    });
    a.q.lock().unwrap().closed = true;
    assert!(
        sched.is_deadlocked(&c),
        "a demoted wait with an open empty arm is still a genuine deadlock"
    );
}

/// D5 owe #3 Path C (#1) — the registry is REFCOUNTED so 2+ fibers demoted on the SAME channel each
/// register/unregister independently (one `unregister` must not drop the channel while a second
/// demoted fiber still waits on it). Drives refcount 0→1→2→1→0 and asserts the veto survives the
/// single `unregister` (the entry is still present at refcount 1) and only the empty/fully-removed
/// state declares deadlock. Catches a refcount-direction regression (remove-at-1 / wrong increment)
/// that the single-fiber test cannot — exactly the "stale entry permanently vetoes a real deadlock"
/// vs "premature removal re-opens the false-positive" failure modes.
#[test]
fn demoted_channel_registry_is_refcounted_for_two_fibers_on_one_channel() {
    let sched = mk_sched(3);
    let core = empty_core();
    let mut c = sched.lock();
    // Two fibers demoted on the SAME channel + one parked sibling; nothing else running.
    c.parked_n = 1;
    let t1 = c.register_waiter(recv_waiter(&core, vec![]));
    let t2 = c.register_waiter(recv_waiter(&core, vec![]));
    // A value queued on the shared channel → at least one demoted fiber pops + progresses.
    core.q.lock().unwrap().push(Value::int(7));
    assert!(
        !sched.is_deadlocked(&c),
        "queued value on the shared demoted channel vetoes deadlock"
    );
    // One fiber pops + un-registers (refcount 2→1); the OTHER is still demoted on this channel, so
    // its waiter must remain. Queue now empty → but the waiter's presence alone does NOT veto; the
    // peek is queue-driven, so an empty registered channel is a genuine all-blocked deadlock.
    core.q.lock().unwrap().pop();
    c.unregister_waiter(t1); // one of two waiters leaves; the other stays registered
    assert!(
        sched.is_deadlocked(&c),
        "refcount 1 + empty queue = genuine deadlock (the surviving demoted fiber has nothing)"
    );
    // A fresh value for the surviving fiber re-vetoes via the retained entry (proves it wasn't
    // dropped at the first unregister).
    core.q.lock().unwrap().push(Value::int(9));
    assert!(
        !sched.is_deadlocked(&c),
        "the surviving waiter still peeks the queue (unregistering one waiter kept the other)"
    );
    core.q.lock().unwrap().pop();
    c.unregister_waiter(t2); // the last waiter leaves
    assert!(c.waiters.is_empty(), "both waiters are gone");
    assert!(
        sched.is_deadlocked(&c),
        "all demoted fibers gone, still all-blocked = deadlock"
    );
}

fn t185_core(cap: Option<usize>) -> Arc<crate::vm::core::ChannelCore> {
    Arc::new(crate::vm::core::ChannelCore {
        cap,
        ..Default::default()
    })
}

/// TICKET-185: a parked sender's offer commits exactly once. The first `pop` takes it and CASes
/// its `Pending` to `DONE + arm`; a second `pop` finds nothing, and the sender's `settle` reads
/// `Sent`. `len()` never counts an offer (Go's `len: 0` with a parked sender).
#[test]
fn chan_state_offer_commits_exactly_once() {
    use crate::vm::core::{ChanState, Pending, PendingOp, SendOutcome, Settled};
    let core = t185_core(Some(0));
    let p = Pending::new();
    let out = core
        .q
        .lock()
        .unwrap()
        .send(Some(0), Value::int(7), Some((&p, 0)));
    assert!(matches!(out, SendOutcome::Offered));
    assert_eq!(core.q.lock().unwrap().len(), 0);
    assert_eq!(core.q.lock().unwrap().pop(), Some(Value::int(7)));
    assert_eq!(p.state(), crate::vm::core::PENDING_DONE);
    assert!(core.q.lock().unwrap().pop().is_none());
    let op = PendingOp::new(p, vec![(Arc::clone(&core), 0, true)]);
    assert!(matches!(op.settle(), Settled::Sent(0)));
    // A withdrawn offer can never commit afterwards.
    let mut cs = ChanState::default();
    let q = Pending::new();
    cs.offer(&q, 0, Value::int(1));
    assert!(q.try_cancel().is_ok());
    assert!(cs.pop().is_none(), "a cancelled offer is dead");
}

/// TICKET-185 — a `recv` whose own slot a sender's `give` filled wakes no sender: the giver
/// committed and already returned, and a slot fill commits nobody else. The wake it used to run
/// took the registry and sched locks on the hand-off's critical path while the other side ran,
/// which is most of `rendezvous_pingpong`'s cost at T>=2. The `wait:` settle path already skips it.
#[test]
fn a_recv_settled_from_its_own_filled_slot_wakes_no_sender() {
    use crate::vm::core::{Pending, PendingOp, SendOutcome};
    let mut vm = Vm::new(Arc::new(empty_program()));
    let sched = Arc::new(mk_sched(1));
    vm.mn = Some(Arc::clone(&sched));
    let core = t185_core(Some(0));
    let h = vm.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let p = Pending::new();
    core.q.lock().unwrap().slot(&p, 0);
    let out = core.q.lock().unwrap().send(Some(0), Value::int(5), None);
    assert!(matches!(out, SendOutcome::Sent), "the give fills the slot");
    vm.pending = Some(PendingOp::new(p, vec![(Arc::clone(&core), 0, false)]));
    let v = vm.channel_method(h, "recv", &[], Span::RUNTIME).unwrap();
    assert_eq!(v.as_int(), 5);
    assert_eq!(
        sched.recv_side_wakes.load(Ordering::Relaxed),
        0,
        "a recv from its own filled slot must wake no sender"
    );
}

/// TICKET-185 — the neighbour that must keep its wake: a `recv` that takes a parked sender's offer
/// commits that sender, so it wakes it.
#[test]
fn a_recv_that_takes_an_offer_still_wakes_its_sender() {
    use crate::vm::core::{PENDING_DONE, Pending};
    let mut vm = Vm::new(Arc::new(empty_program()));
    let sched = Arc::new(mk_sched(1));
    vm.mn = Some(Arc::clone(&sched));
    let core = t185_core(Some(0));
    let h = vm.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let q = Pending::new();
    core.q.lock().unwrap().offer(&q, 0, Value::int(7));
    let v = vm.channel_method(h, "recv", &[], Span::RUNTIME).unwrap();
    assert_eq!(v.as_int(), 7);
    assert_eq!(q.state(), PENDING_DONE, "the recv committed the offer");
    assert_eq!(
        sched.recv_side_wakes.load(Ordering::Relaxed),
        1,
        "a recv that took an offer must wake its sender"
    );
}

/// TICKET-185: `give` fills only a live, unfilled slot that is not the caller's own. A cancelled
/// slot is skipped (and dropped), a filled one is skipped, and the caller's own slot — a `wait:`
/// with a send arm and a recv arm on one cap-0 channel — is never committed.
#[test]
fn chan_state_give_fills_only_a_live_slot() {
    use crate::vm::core::{ChanState, PENDING_DONE, Pending};
    let mut cs = ChanState::default();
    assert!(!cs.give(Value::int(1), None));
    let dead = Pending::new();
    cs.slot(&dead, 0);
    assert!(dead.try_cancel().is_ok());
    let me = Pending::new();
    cs.slot(&me, 1);
    assert!(
        !cs.give(Value::int(2), Some(&me)),
        "a party never fills its own slot"
    );
    assert!(me.is_queued());
    let live = Pending::new();
    cs.slot(&live, 3);
    assert!(cs.give(Value::int(4), Some(&me)));
    assert_eq!(live.state(), PENDING_DONE + 3);
    assert!(
        !cs.give(Value::int(5), Some(&me)),
        "a filled slot takes no second value"
    );
    assert_eq!(cs.take(&live), Some(Value::int(4)));
    assert_eq!(cs.len(), 0, "a filled slot is not buffered");
}

/// TICKET-185 (H2): on a bounded channel, a `pop` that frees a buffer place moves the first live
/// parked offer into it in the same hold (Go's recv refill), so that sender's `send` has happened
/// before any later `close()`.
#[test]
fn chan_state_pop_refills_the_buffer_from_a_parked_offer() {
    use crate::vm::core::{Pending, PendingOp, SendOutcome, Settled};
    let core = t185_core(Some(1));
    let mut g = core.q.lock().unwrap();
    assert!(matches!(
        g.send(Some(1), Value::int(1), None),
        SendOutcome::Sent
    ));
    assert!(matches!(
        g.send(Some(1), Value::int(2), None),
        SendOutcome::Full
    ));
    let p = Pending::new();
    assert!(matches!(
        g.send(Some(1), Value::int(2), Some((&p, 0))),
        SendOutcome::Offered
    ));
    assert_eq!(g.pop(), Some(Value::int(1)));
    assert_eq!(g.len(), 1, "the offer moved into the buffer");
    assert!(!p.is_queued());
    g.close();
    drop(g);
    let op = PendingOp::new(p, vec![(Arc::clone(&core), 0, true)]);
    assert!(
        matches!(op.settle(), Settled::Sent(0)),
        "close came after the send"
    );
    assert_eq!(core.q.lock().unwrap().pop(), Some(Value::int(2)));
}

/// TICKET-185: `close()` closes every live offer (that sender faults) and drops unfilled slots,
/// but keeps a filled slot, whose value was delivered before the close.
#[test]
fn chan_state_close_closes_offers_and_keeps_filled_slots() {
    use crate::vm::core::{ChanState, PENDING_CLOSED, Pending, SendOutcome};
    let mut cs = ChanState::default();
    let sender = Pending::new();
    cs.offer(&sender, 0, Value::int(1));
    let filled = Pending::new();
    cs.slot(&filled, 0);
    let empty = Pending::new();
    cs.slot(&empty, 0);
    assert!(cs.give(Value::int(2), None));
    cs.close();
    assert_eq!(sender.state(), PENDING_CLOSED);
    assert!(cs.pop().is_none(), "a closed offer is never received");
    assert!(!cs.give(Value::int(3), None), "the unfilled slot is gone");
    assert_eq!(cs.take(&filled), Some(Value::int(2)));
    assert!(matches!(
        cs.send(Some(0), Value::int(4), None),
        SendOutcome::Closed
    ));
}

/// TICKET-185: readiness ignores the caller's own entries, so a `wait:` with a send arm and a recv
/// arm on one cap-0 channel neither takes back its own value nor reads itself as ready.
#[test]
fn chan_state_readiness_ignores_the_callers_own_entries() {
    use crate::vm::core::{ChanState, Pending};
    let mut cs = ChanState::default();
    let me = Pending::new();
    cs.offer(&me, 0, Value::int(1));
    cs.slot(&me, 1);
    assert!(!cs.recv_ready_for(Some(&me)));
    assert!(!cs.send_ready_for(Some(0), Some(&me)));
    assert!(cs.pop_for(Some(&me)).is_none(), "never my own offer");
    let other = Pending::new();
    assert!(cs.recv_ready_for(Some(&other)));
    assert!(cs.send_ready_for(Some(0), Some(&other)));
    assert!(cs.send_ready_for(None, Some(&me)));
    assert_eq!(cs.pop_for(Some(&other)), Some(Value::int(1)));
}

/// D6: `poll_park_offload` hands a fiber whose socket op `WouldBlock`ed to the netpoller —
/// running→inflight — so a socket-parked fiber is accounted as in-flight (it WILL be woken by the
/// OS) and vetoes a false deadlock, exactly like a blocking-pool offload. Uses a real loopback fd
/// (never written) so the fiber genuinely stays parked; `deregister` cleans up (delete-before-drop).
#[test]
fn poll_park_offload_moves_running_to_inflight() {
    use std::os::fd::AsRawFd;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let _client = std::net::TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    let key = usize::MAX - 10;

    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running == 1, runnable == 1
    assert_eq!(sched.lock().running, 1);

    sched.poll_park_offload(
        f0,
        PollPark {
            key,
            fd: server.as_raw_fd(),
            interest: poller::Interest::Read,
            in_flight: core::new_in_flight(),
            closed: core::new_closed(),
            deadline: None,
        },
    );
    assert_eq!(
        sched.lock().running,
        0,
        "poll-park freed the worker (running decremented)"
    );
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        1,
        "running → inflight on poll-park"
    );

    // The in-flight socket op vetoes a deadlock even with the sibling still queued drained off.
    let mut c = sched.lock();
    c.parked_n = 1;
    assert!(
        !sched.is_deadlocked(&c),
        "a socket op in flight on the poller vetoes a false deadlock"
    );
    drop(c);

    // Clean up: deregister disarms the fd + re-injects (inflight→runnable), before `server` drops.
    assert!(
        poller::deregister(key),
        "the parked socket op was registered"
    );
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        0,
        "deregister re-injected the fiber"
    );
}

/// D5: `offload` hands a fiber to the blocking pool (running→inflight); when the pool finishes the
/// native it `complete_offload`s the fiber back onto the run queue (inflight→runnable) with the
/// raw [`NativeRet`] stashed for the worker to lower + push on resume.
#[test]
fn offload_runs_native_and_requeues_fiber_with_result() {
    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running==1, runnable==1
    assert_eq!(sched.lock().running, 1);

    let req = OffloadReq {
        func: double_native,
        args: vec![crate::native::NativeArg::Int(21)],
        span: Span::RUNTIME,
        timer: None,
    };
    sched.offload(f0, req);

    // The job runs asynchronously on the blocking pool; wait (bounded) for it to complete.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "offloaded native never completed"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        sched.lock().running,
        0,
        "offload freed the worker (running decremented)"
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "f1 still queued + f0 requeued on completion"
    );

    // The requeued fiber carries the lowered-pending native result (Int(21)*2 == Int(42)).
    let mut found = None;
    while let Take::Run(f, _) = sched.take_runnable_unattached(0, 1, 0) {
        if f.task_index == 0 {
            found = Some(f);
            break;
        }
    }
    let f0 = found.expect("offloaded fiber requeued");
    assert_eq!(
        f0.resume_native,
        Some(Ok(crate::native::NativeRet::Int(42)))
    );
}

/// D5 owe #2: a `timer_ms` offload parks the fiber on the *timer* thread (not the blocking pool):
/// running→inflight at submit (so it vetoes the deadlock predicate while sleeping), then the timer
/// fires at the deadline and `complete_offload`s the fiber back (inflight→runnable) carrying
/// `Ok(Nil)` — the native is never run on this path (a sleep computes nothing). Guards the timer
/// branch + that the sleeping fiber can't fault a false deadlock.
#[test]
fn timer_offload_parks_then_requeues_fiber_with_nil() {
    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running == 1, runnable == 1
    assert_eq!(sched.lock().running, 1);

    // `func`/`args` are intentionally ignored on the timer path (the fiber resumes with `Nil`);
    // `double_native` is just a stand-in to satisfy the struct.
    let req = OffloadReq {
        func: double_native,
        args: vec![],
        span: Span::RUNTIME,
        timer: Some(crate::vm::TimerSleep {
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(40),
            wake: crate::vm::block::WakeSet::default(),
            run_deadline: None,
            timeout_ms: 0,
        }),
    };
    sched.offload(f0, req);

    // While the timer holds it the fiber is `inflight` — neither running, runnable, nor parked —
    // and must veto a deadlock fire (it WILL come back).
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        1,
        "timer offload moved the fiber to inflight"
    );
    {
        let c = sched.lock();
        assert!(
            !sched.is_deadlocked(&c),
            "a timer-parked (inflight) fiber must not fault a false deadlock"
        );
    }

    // The timer fires at the deadline and requeues the fiber.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "timer never fired the parked fiber back (lost wakeup?)"
        );
        std::thread::yield_now();
    }
    assert_eq!(sched.lock().running, 0, "timer offload freed the worker");
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "f1 still queued + f0 requeued by the timer"
    );

    let mut found = None;
    while let Take::Run(f, _) = sched.take_runnable_unattached(0, 1, 0) {
        if f.task_index == 0 {
            found = Some(f);
            break;
        }
    }
    let f0 = found.expect("timer-parked fiber requeued");
    assert_eq!(
        f0.resume_native,
        Some(Ok(crate::native::NativeRet::Nil)),
        "sleep resumes with Nil, native not run"
    );
}

// ---- M1.4: a fiber context roots itself (one walker, the run's root registry) ----

fn sorted_roots(roots: Vec<GcRef>) -> Vec<u32> {
    let mut v: Vec<u32> = roots.into_iter().map(|h| h.0).collect();
    v.sort_unstable();
    v
}

/// M1.4 — every root `Vm::gc_roots` finds in the per-fiber fields travels into the `FiberCtx` on a
/// swap-out, and `FiberCtx::roots` finds the same multiset there: the walker is one, so a field
/// cannot be rooted live and lost parked.
#[test]
fn fiber_roots_equal_vm_roots_after_swap() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let obj = |vm: &mut Vm, s: &str| vm.heap.alloc(Obj::Str(s.into()));
    let on_stack = obj(&mut vm, "stack");
    let fl = vm.heap.alloc(Obj::FloatBox(1.5));
    vm.stack = vec![Value::obj(on_stack), Value::int(3), Value::float_ref(fl)];
    let (home, clo) = (obj(&mut vm, "home"), obj(&mut vm, "closure"));
    let (d_callee, d_arg) = (obj(&mut vm, "defer-callee"), obj(&mut vm, "defer-arg"));
    vm.frames.push(CallFrame {
        proto: 0,
        ip: 0,
        base: 0,
        home,
        closure: Some(clo),
        counted: false,
        is_toplevel: false,
        deferred: vec![Deferred::Call {
            callee: Value::obj(d_callee),
            args: vec![Value::obj(d_arg)],
            span: Span::RUNTIME,
        }],
        defer_markers: Vec::new(),
        nursery_len: 0,
        has_implicit_nursery: false,
        call_span: Span::RUNTIME,
        argc: 0,
    });
    let (t_callee, t_arg) = (obj(&mut vm, "task-callee"), obj(&mut vm, "task-arg"));
    vm.nurseries.push(vec![QueuedTask {
        call: PendingCall::Call {
            callee: Value::obj(t_callee),
            args: vec![Value::obj(t_arg)],
            span: Span::RUNTIME,
        },
    }]);
    vm.executors = vec![obj(&mut vm, "executor")];
    let interned = obj(&mut vm, "interned");
    vm.str_intern.insert(1, interned);

    let before = sorted_roots(vm.gc_roots());
    let live = roots_per_field(&vm.root_view());
    for (field, roots) in &live {
        // `start` is a parked fiber's alone: `pending_fiber_roots_its_start` covers it.
        // `start` is a parked fiber's alone; the module table is the run's (M3.1), rooted by
        // every context and never swapped, so neither travels here.
        if *field == "start" || *field == "module_objs" {
            assert!(roots.is_empty(), "the live `Vm` has no `{field}` to move");
        } else {
            assert!(!roots.is_empty(), "the test fills RootView field `{field}`");
        }
    }
    let mut ctx = FiberCtx {
        owns_side: true,
        ..FiberCtx::default()
    };
    vm.swap_ctx(&mut ctx);
    assert!(
        vm.gc_roots().is_empty(),
        "every root above travels in FiberCtx"
    );
    assert_eq!(sorted_roots(ctx.roots()), before);
    assert_eq!(
        roots_per_field(&ctx.root_view()),
        live,
        "each RootView field holds the same roots live and parked"
    );
}

/// The roots of each [`RootView`] field on its own, by field name. The destructures name every field
/// with no `..`, so a field added to `RootView` or `HeapCaches` fails to compile here until
/// `fiber_roots_equal_vm_roots_after_swap` fills it.
fn roots_per_field(v: &super::root_registry::RootView) -> Vec<(&'static str, Vec<u32>)> {
    use super::root_registry::{HeapCaches, RootView};
    let RootView {
        stack,
        frames,
        nurseries,
        start,
        module_objs,
        executors,
        caches,
    } = v;
    let HeapCaches { str_intern } = caches.as_ref().expect("a Vm or fiber view has caches");
    let walk = |one: RootView| {
        let mut out = Vec::new();
        one.walk(&mut out);
        sorted_roots(out)
    };
    vec![
        (
            "stack",
            walk(RootView {
                stack,
                ..RootView::default()
            }),
        ),
        (
            "frames",
            walk(RootView {
                frames,
                ..RootView::default()
            }),
        ),
        (
            "nurseries",
            walk(RootView {
                nurseries,
                ..RootView::default()
            }),
        ),
        (
            "start",
            walk(RootView {
                start: *start,
                ..RootView::default()
            }),
        ),
        (
            "module_objs",
            sorted_roots(module_objs.map(|t| t.iter().collect()).unwrap_or_default()),
        ),
        ("executors", sorted_roots(executors.to_vec())),
        (
            "str_intern",
            sorted_roots(str_intern.values().copied().collect()),
        ),
    ]
}

/// M1.4 — a spawned, not-yet-started fiber holds its task (callee and args) in its own context, so
/// the context alone roots them, and the run's root registry lists them until the fiber drops.
#[test]
fn pending_fiber_roots_its_start() {
    let vm = Vm::new(Arc::new(empty_program()));
    let mut worker = vm.spawn_worker();
    let callee = worker.heap.alloc(Obj::Str("callee".into()));
    let arg = worker.heap.alloc(Obj::Str("arg".into()));
    let fl = worker.heap.alloc(Obj::FloatBox(2.5));
    let fiber = ReadyWorker {
        worker,
        call: ReadyCall::Invoke {
            callee: Value::obj(callee),
            args: vec![Value::obj(arg), Value::int(1), Value::float_ref(fl)],
        },
        span: Span::RUNTIME,
        payload: crate::vm::gc_door::PayloadInFlight::new(),
    }
    .into_fiber(0, 0);
    assert!(fiber.ctx.start.is_some(), "the task waits in the context");
    let own = fiber.ctx.roots();
    let listed = vm.root_registry.roots();
    for h in [callee, arg, fl] {
        assert!(own.contains(&h), "the context roots {h:?}");
        assert!(listed.contains(&h), "the registry lists {h:?}");
    }
    drop(fiber);
    assert!(
        vm.root_registry.roots().is_empty(),
        "a dropped fiber leaves the registry"
    );
}
