//! VM tests: work stealing, wake keys, D5 blocking calls, owe-3 demotion, owner-fault verdicts,
//! `wait` timers in callbacks, extern nil return.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// D4c: `try_steal` grabs ceil-half of the first non-empty victim's ring (from the back), leaving
/// the rest, and is net-zero on `runnable` (the fibers stay runnable, just change owner).
#[test]
fn schedule_steals_half_from_victim() {
    let sched = mk_sched(4);
    {
        let mut vq = sched.lock_local(1);
        for i in 0..4 {
            vq.ring.push_back(mk_fiber(i));
        }
    }
    sched.runnable.fetch_add(4, Ordering::Relaxed); // keep the counter consistent with the queues
    let stolen = sched.try_steal(0); // worker 0 steals from worker 1
    assert_eq!(stolen.len(), 2, "ceil(4/2) stolen");
    assert_eq!(
        sched.lock_local(1).ring.len(),
        2,
        "half left with the victim"
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        4,
        "stealing is net-zero on runnable"
    );
}

/// D4d: on a `GLOBAL_CHECK_INTERVAL`th schedule a worker pulls from the global queue before its
/// own local (anti-starvation); on any other tick it drains its own local first.
#[test]
fn schedule_pulls_global_every_61st_tick() {
    // tick=61 (a multiple) → the global fiber wins over the local one.
    let periodic = mk_sched(4);
    periodic.lock_local(0).ring.push_back(mk_fiber(0)); // local
    periodic.seed(vec![mk_fiber(1)]); // global (bumps runnable)
    periodic.runnable.fetch_add(1, Ordering::Relaxed); // for the local fiber
    let got = match periodic.take_runnable(0, GLOBAL_CHECK_INTERVAL, 0) {
        Take::Run(f, _) => f.task_index,
        Take::Stop => panic!("expected a runnable fiber"),
    };
    assert_eq!(got, 1, "periodic tick drains the global queue first");

    // tick=1 (not a multiple) → the local fiber wins (normal order).
    let normal = mk_sched(4);
    normal.lock_local(0).ring.push_back(mk_fiber(0));
    normal.seed(vec![mk_fiber(1)]);
    normal.runnable.fetch_add(1, Ordering::Relaxed);
    let got = match normal.take_runnable(0, 1, 0) {
        Take::Run(f, _) => f.task_index,
        Take::Stop => panic!("expected a runnable fiber"),
    };
    assert_eq!(got, 0, "non-periodic tick drains the own local first");
}

/// TICKET-128 (W13-25) — the periodic global pull (step 0 of `take_runnable`) can run a global
/// fiber AHEAD of this worker's own `runnext`. That global fiber may block its thread in a
/// `Kind::Inline` native, so step 0 must recruit an idle worker to steal the skipped `runnext` —
/// otherwise nobody is left to run it (the W13-25 send-side stall).
#[test]
fn a_periodic_global_pull_recruits_a_worker_for_the_skipped_runnext() {
    let sched = Arc::new(mk_sched(2));
    {
        let mut lq = sched.lock_local(0);
        lq.runnext = Some(mk_fiber(0));
        lq.runnext_at = Some(std::time::Instant::now());
    }
    sched.runnable.fetch_add(1, Ordering::Relaxed);
    sched.seed(vec![mk_fiber(1)]);

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (woken_tx, woken_rx) = std::sync::mpsc::channel();
    let s = Arc::clone(&sched);
    std::thread::spawn(move || {
        let mut c = s.lock();
        s.idle_sleepers.fetch_add(1, Ordering::Relaxed);
        ready_tx.send(()).unwrap();
        c = s.idle_cv.wait(c).unwrap_or_else(|e| e.into_inner());
        s.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
        drop(c);
        woken_tx.send(()).unwrap();
    });
    ready_rx.recv().unwrap();

    let got = match sched.take_runnable(0, GLOBAL_CHECK_INTERVAL, SENTINEL_SCOPE) {
        Take::Run(f, _) => f.task_index,
        Take::Stop => panic!("expected a runnable fiber"),
    };
    assert_eq!(got, 1, "step 0 must run the global fiber ahead of runnext");
    assert!(
        sched.lock_local(0).runnext.is_some(),
        "the runnext must still be there, unstolen"
    );
    assert!(
        woken_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_ok(),
        "step 0 ran a global fiber ahead of a runnext and recruited nobody (W13-25 send-side stall)"
    );
}

/// D4c: a thief never steals from itself and skips empty victims (returns nothing when only its
/// own local has work).
#[test]
fn steal_skips_self_and_empty_victims() {
    let sched = mk_sched(4);
    {
        let mut own = sched.lock_local(0);
        own.ring.push_back(mk_fiber(0));
        own.ring.push_back(mk_fiber(1));
    }
    assert!(
        sched.try_steal(0).is_empty(),
        "no sibling has work; must not steal from self"
    );
}

/// TICKET-128 (W13-25) — a `runnext` younger than `HANDOFF_GRACE` is not stealable, so an idle
/// worker cannot snatch a rendezvous partner before the waker parks; once the grace elapses it
/// IS stealable, so a fiber stranded there is never permanently unreachable.
#[test]
fn try_steal_leaves_a_fresh_runnext_until_the_handoff_grace_elapses() {
    let sched = mk_sched(0);
    sched.lock_local(1).runnext = Some(mk_fiber(0));
    sched.lock_local(1).runnext_at =
        Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    assert!(
        sched.try_steal(0).is_empty(),
        "a fresh runnext must not be stolen"
    );
    sched.lock_local(1).runnext_at = Some(std::time::Instant::now() - HANDOFF_GRACE * 2);
    assert_eq!(
        sched.try_steal(0).len(),
        1,
        "a runnext older than HANDOFF_GRACE must become stealable"
    );
}

/// D2b/U2: parking the running fiber on an EMPTY channel frees the worker (`running--`,
/// `parked++`); a `send_wake` on that channel enqueues the message and moves the fiber back onto
/// the run queue as `Ready`.
#[test]
fn mnsched_park_then_wake_requeues_fiber() {
    let sched = mk_sched(1);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    sched.park(key, Arc::clone(&core), f);
    {
        let c = sched.lock();
        assert_eq!(c.running, 0);
        assert_eq!(c.parked_n, 1);
        assert!(c.global.is_empty());
    }
    sched.send_wake(key, &core, WireValue::Int(7));
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0);
        assert_eq!(c.global.len(), 1);
    }
    let g = take_run(&sched);
    assert_eq!(g.task_index, 0);
    assert!(matches!(g.state, FiberState::Ready));
}

/// TICKET-128 (W13-25) — a rendezvous wake that wakes exactly ONE fiber files it in the WAKER's
/// own `LocalQ.runnext` (Go's `runnext` handoff) instead of the global queue, so the pair stays
/// on one worker and no broadcast is needed.
#[test]
fn handoff_wake_files_one_woken_fiber_in_the_wakers_runnext() {
    let sched = mk_sched(2);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched);
    let _f1 = take_run(&sched);
    sched.park(key, Arc::clone(&core), f0);
    sched.handoff_wake(key, &core, WakeKind::All, 1, false);
    assert!(
        sched.lock_local(1).runnext.is_some(),
        "the woken fiber must land in worker 1's runnext"
    );
    assert!(
        sched.lock().global.is_empty(),
        "the woken fiber must not also sit in the global queue"
    );
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 1);
}

/// TICKET-185 — a cap-0 `give` fills exactly one parked receiver's slot, so that receiver goes to
/// the giver's `runnext` (DEC-128), not the global queue plus a broadcast.
#[test]
fn a_give_hands_the_slot_owner_to_the_givers_runnext() {
    let sched = mk_sched(2);
    let core = cap0_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched);
    let _f1 = take_run(&sched);
    sched.park(key, Arc::clone(&core), f0);
    let out = sched.send_commit(key, &core, WireValue::Int(5), None, 1);
    assert!(matches!(out, crate::vm::core::SendOutcome::Sent));
    assert!(
        sched.lock_local(1).runnext.is_some(),
        "the slot owner must land in the giver's runnext"
    );
    assert!(sched.lock().global.is_empty());
    assert_eq!(sched.lock().parked_n, 0);
}

/// TICKET-130 (W13-26) — `wake_key` on a sched whose bucket for `key` holds NO fiber changes no
/// counter that sched's workers read, so it must not wake them. `wake_run_wide` calls `wake_key` on
/// every peer sched once per wake; with an unconditional `notify_waiters` a four-deep nested
/// ping-pong woke every idle worker of four peer eager scheds on every message.
#[test]
fn wake_key_on_an_empty_bucket_does_not_wake_an_idle_sleeper() {
    let sched = Arc::new(mk_sched(2));
    let core = empty_core();
    let key = core_key(&core);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (woken_tx, woken_rx) = std::sync::mpsc::channel();
    let s = Arc::clone(&sched);
    std::thread::spawn(move || {
        let mut c = s.lock();
        s.idle_sleepers.fetch_add(1, Ordering::Relaxed);
        ready_tx.send(()).unwrap();
        c = s.idle_cv.wait(c).unwrap_or_else(|e| e.into_inner());
        s.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
        drop(c);
        woken_tx.send(()).unwrap();
    });
    ready_rx.recv().unwrap();
    sched.wake_key(key, WakeKind::All);
    assert!(
        woken_rx
            .recv_timeout(std::time::Duration::from_millis(200))
            .is_err(),
        "wake_key drained an empty bucket and still woke an idle worker (W13-26 peer-sched broadcast)"
    );
    sched.notify_waiters();
    assert!(
        woken_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_ok(),
        "the sleeper must still answer a real notify_waiters"
    );
}

/// TICKET-130 (W13-26) — the other half of `wake_key`'s contract: a drain that DID requeue a parked
/// fiber must still wake an idle worker to run it, or that fiber sits in `global` with nobody awake.
#[test]
fn wake_key_that_requeues_a_parked_fiber_still_wakes_an_idle_sleeper() {
    let sched = Arc::new(mk_sched(2));
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    sched.park(key, Arc::clone(&core), f0);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (woken_tx, woken_rx) = std::sync::mpsc::channel();
    let s = Arc::clone(&sched);
    std::thread::spawn(move || {
        let mut c = s.lock();
        s.idle_sleepers.fetch_add(1, Ordering::Relaxed);
        ready_tx.send(()).unwrap();
        c = s.idle_cv.wait(c).unwrap_or_else(|e| e.into_inner());
        s.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
        drop(c);
        woken_tx.send(()).unwrap();
    });
    ready_rx.recv().unwrap();
    sched.wake_key(key, WakeKind::All);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        1,
        "wake_key must requeue the parked fiber"
    );
    assert!(
        woken_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .is_ok(),
        "wake_key requeued a parked fiber and woke no idle worker to run it"
    );
}

/// D3/U: a fiber that exhausts its reduction budget `yield_fiber`s — the scheduler frees the
/// worker (`running--`) and requeues it at the **tail** of `runq` (round-robin), still `Ready`.
/// No park bucket is touched (a yield carries no channel handle). Mirrors the park/wake test.
#[test]
fn mnsched_yield_fiber_requeues_at_tail() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // pops task 0, running == 1
    assert_eq!(f0.task_index, 0);
    sched.yield_fiber(f0); // requeue task 0 behind task 1
    {
        let c = sched.lock();
        assert_eq!(c.running, 0);
        assert_eq!(c.parked_n, 0); // a yield never parks
        assert_eq!(c.global.len(), 2);
    }
    // Round-robin: task 1 (which was behind task 0) now runs before the requeued task 0.
    assert_eq!(take_run(&sched).task_index, 1);
    let back = take_run(&sched);
    assert_eq!(back.task_index, 0);
    assert!(matches!(back.state, FiberState::Ready));
}

/// W8-7 — the liveness property `yield_fiber`'s dropped `notify_all` rests on: a yielded fiber is
/// reachable by a **different** worker through the ordinary global-queue path, no wake required.
/// wid 0 yields task 0 back to the tail; a SEPARATE wid (1), passing the sentinel scope (a farmed
/// helper never self-stops), pulls it straight off the global queue via the normal `take_runnable`
/// batch-grab — proving delivery needs no `cv.notify`. Green both before and after the removal (it
/// pins ordering/reachability, not the notify itself); a future change that routes a yielded fiber
/// to the yielding worker's own local ring instead of the global queue would break it.
#[test]
fn mnsched_yield_fiber_reachable_by_other_worker_without_wake() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // wid 0 pops task 0
    assert_eq!(f0.task_index, 0);
    sched.yield_fiber(f0); // requeue task 0 at the global tail — no notify needed for this to work
    // task 1 is still ahead of the requeued task 0 in the global queue.
    match sched.take_runnable(1, 1, SENTINEL_SCOPE) {
        Take::Run(f, _) => assert_eq!(f.task_index, 1),
        Take::Stop => panic!("expected a runnable fiber, got Stop"),
    }
    match sched.take_runnable(1, 1, SENTINEL_SCOPE) {
        Take::Run(f, _) => assert_eq!(f.task_index, 0),
        Take::Stop => panic!("expected a runnable fiber, got Stop"),
    }
}

/// D4/stress: a combined-churn workload that exercises EVERY new D4 path together under a
/// watchdog — 500 consumers that block on `recv` (park + `send_wake`), 500 producers that do CPU
/// work (reduction `yield` → global, batch-grab, work-stealing between idle workers) then `send`
/// (waking a parked consumer), all `#fibers ≫ #workers`. The consumers accumulate into one
/// `Shared`; the join must complete with the exact arithmetic sum, with no lost/duplicated fiber,
/// no false deadlock, and no hang (the watchdog turns a regression — a lost wakeup, a steal/grab
/// accounting bug, a deadlock-predicate false positive — into a loud failure rather than a wedge).
#[test]
fn d4_worksteal_cpu_and_channel_stress() {
    let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..500:
            spawn consumer(ch, sink)
        for k in 0..500:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
    // sum_{k=0}^{499} sum_{i=10k}^{10k+9} i = sum_{k=0}^{499} (100k + 45) = 12_475_000 + 22_500.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("mixed work-steal nursery completed"), "12497500\n"),
        Err(_) => panic!(
            "hung — D4 work-stealing/grab/wait_timeout regressed (lost wakeup or accounting bug)"
        ),
    }
}

/// D4e/stress: the lost-wakeup regression guard for the runnable-gated park. D4e removed the 2 ms
/// `wait_timeout` backstop — an idle worker now sleeps on `cv` INDEFINITELY when `runnable == 0`,
/// woken ONLY by a real `notify` from a sibling's `send`/`yield`/`finish`/offload-complete. A
/// single missed wakeup is no longer a 2 ms stall but a PERMANENT hang. The race is probabilistic
/// (the batch-grab in-hand window, the park-vs-send gap), so we REPEAT a park-heavy
/// consumer-first workload many rounds, each under a watchdog: any lost wakeup in any round =>
/// the round never completes => `recv_timeout` fires => loud failure. 300 consumers are spawned
/// FIRST (they all `recv`-park, driving every worker to a true `cv.wait` sleep with `runnable`
/// near zero), then 300 producers wake them — the exact sleep→`send_wake`→wake path D4e changed.
#[test]
fn d4e_pingpong_no_lost_wakeup_stress() {
    let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..300:
            spawn consumer(ch, sink)
        for k in 0..300:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
    // sum_{k=0}^{299} sum_{i=10k}^{10k+9} i = sum_{k=0}^{299} (100k + 45) = 100*44850 + 300*45.
    let expected = format!("{}\n", 100 * (299 * 300 / 2) + 300 * 45);
    for round in 0..25 {
        let want = expected.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture(src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(r) => assert_eq!(
                r.expect("park-heavy nursery completed"),
                want,
                "round {round}"
            ),
            Err(_) => panic!(
                "hung on round {round} — D4e runnable-gated park lost a wakeup \
                     (an idle worker slept on cv with a runnable fiber pending)"
            ),
        }
    }
}

/// D4e/stress: wake-from-TRUE-sleep. Distinct from the churn test above, this isolates the exact
/// state D4e introduced — workers asleep on `cv` with `runnable == 0` — and proves a later `send`
/// wakes them with the poll gone. One `slow_producer` burns CPU on a single worker while `N`
/// consumers `recv`-park; with nothing queued (`runnable == 0`) every OTHER worker reaches the
/// runnable-gated branch and does a real `cv.wait` (no 2 ms timeout to fall back on). Only when
/// the producer finishes its spin and fires its burst of `send`s are the sleepers woken. The join
/// completing with `sink == N` proves no sleeper was stranded. Watchdog 30 s.
#[test]
fn d4e_wake_parked_workers_from_true_sleep() {
    let n = 200usize;
    let src = format!(
        "\
fn slow_producer(ch: Channel[int], n: int):
    acc := 0
    i := 0
    while i < 8000000:
        acc += i
        i += 1
    j := 0
    while j < n:
        ch.send(acc + j)
        j += 1

fn consumer(ch: Channel[int], sink: Shared[int]):
    ch.recv()
    sink.update(fn(x): x + 1)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn consumer(ch, sink)
        spawn slow_producer(ch, {n})
    print(sink.get())

main()
"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(&src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(
            r.expect("wake-from-sleep nursery completed"),
            format!("{n}\n")
        ),
        Err(_) => panic!(
            "hung — D4e: a `send` failed to wake workers parked in the runnable-gated `cv.wait` \
                 (lost wakeup from true sleep)"
        ),
    }
}

/// D5 — the discriminating offload proof. `N = workers * 4` fibers each `sleep_ms(150)`. Run
/// INLINE on the core pool, each of the `workers` threads must run 4 sleeps back-to-back → wall
/// clock ≥ `4 * 150 = 600 ms` regardless of core count. OFFLOADED to the dirty pool, all `N`
/// sleeps run concurrently → wall clock ≈ `150 ms`. Asserting `< 450 ms` (`3 * sleep`) fails on
/// the inline path and passes once offload is wired — and the `N ∝ workers` construction keeps
/// that gap on any machine (the inline path is always 4 batches). Watchdog 30 s.
#[test]
fn d5_blocking_sleeps_run_concurrently_not_serialized() {
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1)
        .max(1);
    let n = workers * 4;
    let src = format!(
        "\
import std.time

fn sleeper():
    time.sleep_ms(150)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
    );
    let entry = write_temp_chz("d5_sleeps", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let out = run_file_with(&run_entry, crate::native::HostConfig::default());
        let _ = tx.send((out, start.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok(((out, _err, res, _code), elapsed)) => {
            assert!(res.is_ok(), "sleeper nursery faulted: {res:?}");
            assert_eq!(out, "done\n");
            assert!(
                elapsed < std::time::Duration::from_millis(450),
                "{n} sleep_ms(150) fibers took {elapsed:?} — blocking calls serialized on the core pool \
                     instead of offloading to the dirty pool (G3 starvation)"
            );
        }
        Err(_) => {
            panic!("hung — D5 offload/complete regressed (lost wakeup or inflight accounting bug)")
        }
    }
}

/// D5 owe #3 Path C (#3 sleep-in-callback demote) — the discriminating proof, mirroring
/// `d5_blocking_sleeps_run_concurrently_not_serialized` but with the `sleep_ms` reached INSIDE a
/// native callback (`[1].map(nap)`, `native_reentry > 0`). The offload gate requires
/// `native_reentry == 0`, so without the demote this sleep runs INLINE and pins its worker:
/// `N = workers * 4` such tasks ⇒ 4 back-to-back batches ⇒ ≥ `4 * 150 = 600 ms` on any core count.
/// WITH the demote each sleeping callback frees its worker (spawns a replacement) so all `N` run
/// concurrently ⇒ ≈ `150 ms`. Asserting `< 450 ms` fails on the inline path and passes once the
/// in-callback demote is wired. Watchdog 30 s.
#[test]
fn d5_owe3_path_c_sleep_in_callback_demotes_frees_worker() {
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1)
        .max(1);
    let n = workers * 4;
    let src = format!(
        "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(150)
    return x

fn sleeper():
    [1].map(nap)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
    );
    let entry = write_temp_chz("d5_owe3_sleep_cb", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let out = run_file_with(&run_entry, crate::native::HostConfig::default());
        let _ = tx.send((out, start.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok(((out, _err, res, _code), elapsed)) => {
            assert!(res.is_ok(), "in-callback sleeper nursery faulted: {res:?}");
            assert_eq!(out, "done\n");
            assert!(
                elapsed < std::time::Duration::from_millis(450),
                "{n} sleep_ms(150)-in-callback fibers took {elapsed:?} — the in-callback sleep pinned \
                     its worker (ran inline) instead of demoting (#3)"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed"),
    }
}

/// D5 owe #3 Path C (#3) — correctness of the in-callback sleep demote: a `sleep_ms` inside a
/// native `xs.map` still produces the right result after demoting (the worker is freed + resumed in
/// place; output unchanged). The sum proves all three callbacks ran past their sleep. Watchdog 30 s.
#[test]
fn d5_owe3_path_c_sleep_in_callback_correct() {
    let src = "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(20)
    return x * 2

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(nap)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
        spawn work(sink)
    print(sink.get())

main()
";
    let entry = write_temp_chz("d5_owe3_sleep_correct", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback sleep demote faulted: {res:?}");
            // each work(): 2*(1+2+3) = 12; two tasks update the same sink → 24.
            assert_eq!(out, "24\n");
        }
        Err(_) => {
            panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed (correctness)")
        }
    }
}

/// D5 owe #3 Path C (#3 socket half) — correctness of the in-callback socket DEMOTE: a `Socket::read`
/// reached INSIDE a native `xs.map` callback (`native_reentry > 0`) that `WouldBlock`s must demote
/// the worker (spin a replacement, backoff-poll the non-blocking read in place) and resume with the
/// real bytes — NOT surface the `--parallel`-engine error. `park_on_fd` only parks on the netpoller
/// when `native_reentry == 0`; inside a callback the Rust-stack `map` loop can't snapshot-park, so
/// without the demote the read returns `Result::Err("read would block: ... require the --parallel
/// engine")`, which `?` propagates → the client prints `ERR:…` instead of the echoed line. The
/// server `sleep_ms(50)`s after `accept` before writing, so the client's in-callback read is
/// *guaranteed* empty (forces the demote path deterministically). Parallel-only, 30 s watchdog.
#[test]
fn d5_owe3_path_c_socket_read_in_callback_demotes() {
    let src = "\
import std.net
import std.time

fn read_reply(s: Socket) -> str!:
    line := s.read(64)?
    return ?line

fn do_client(addr: str) -> str!:
    sock := net.connect(addr)?
    socks := [sock]
    replies := socks.map(read_reply)
    line := replies[0]?
    sock.close()
    return ?line

fn client(addr: str):
    match do_client(addr):
        ?line: print(line)
        !e: print(\"ERR:\" + e.message())

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(50)
    conn.write(\"hello\")?
    conn.close()
    listener.close()
    return ?0

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
    let entry = write_temp_chz("d5_owe3_sock_read_cb", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "in-callback socket read demote faulted: {res:?}"
            );
            // client prints the echoed line; main prints a trailing blank line.
            assert_eq!(
                out, "hello\n\n",
                "in-callback read did not demote (got: {out:?})"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C socket-read-in-callback demote regressed"),
    }
}

/// D5 owe #3 Path C (#3 socket half) — the listener path: an `accept` reached INSIDE a native `map`
/// callback that `WouldBlock`s (no client yet) demotes + resumes once a sibling client connects,
/// instead of erroring. Proves the `Listener::accept` gate, not just `Socket::read`. The client
/// `sleep_ms(50)`s before connecting so the in-callback `accept` is guaranteed to block first.
/// Parallel-only, 30 s watchdog.
#[test]
fn d5_owe3_path_c_accept_in_callback_demotes() {
    let src = "\
import std.net
import std.time

fn accept_one(l: Listener) -> int!:
    conn := l.accept()?
    conn.read(64)?
    conn.close()
    return ?1

fn do_server(listener: Listener) -> int!:
    ls := [listener]
    got := ls.map(accept_one)
    n := got[0]?
    listener.close()
    return ?n

fn server(listener: Listener):
    match do_server(listener):
        ?n: print(n)
        !e: print(\"ERR:\" + e.message())

fn client(addr: str) -> int!:
    time.sleep_ms(50)
    sock := net.connect(addr)?
    sock.write(\"ping\")?
    sock.close()
    return ?0

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
    let entry = write_temp_chz("d5_owe3_sock_accept_cb", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback accept demote faulted: {res:?}");
            assert_eq!(
                out, "1\n\n",
                "in-callback accept did not demote (got: {out:?})"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C accept-in-callback demote regressed"),
    }
}

/// D5 — a blocking *filesystem* native (`fs.exists`, returns a `bool`) is offloaded, runs off the
/// core worker, and its result is lowered + pushed on resume so execution continues correctly
/// past the call. `N` fibers each check a real temp file and bump a `Shared` — the join sum must
/// be exactly `N` (every offloaded call returned `true` and resumed into the `if`). Guards the
/// resume-continues-past-the-call + bool-lowering path. Watchdog 30 s.
#[test]
fn d5_blocking_fs_calls_offload_and_resume_correctly() {
    let path = std::env::temp_dir().join(format!("chezzi_d5_exists_{}.txt", std::process::id()));
    std::fs::write(&path, b"x").expect("write temp file");
    let path_str = path.to_str().expect("utf8 temp path").to_string();
    let n = 64usize;
    let src = format!(
        "\
import std.fs

fn checker(sink: Shared[int], path: str):
    if fs.exists(path):
        sink.update(fn(x): x + 1)

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink, \"{path_str}\")
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_fs", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "fs.exists nursery faulted: {res:?}");
            assert_eq!(out, format!("{n}\n"));
        }
        Err(_) => panic!("hung — D5 fs offload/resume regressed"),
    }
}

/// D5 — a blocking native reached *inside a native callback* (here `fs.exists` inside the
/// per-element fn of `list.map`, which runs under the `native_reentry` guard) must NOT be
/// offloaded to the dirty pool: the callback's loop state lives on the Rust host stack and cannot be
/// parked into a fiber. The offload is gated on `native_reentry == 0`, so it falls back to inline
/// execution and the map completes correctly (no fault, no corruption). Guards the gate for a
/// NON-sleep blocking native (`sleep_ms` specifically now DEMOTES the worker inside a callback —
/// see `d5_owe3_path_c_sleep_in_callback_*`). Watchdog 30 s.
#[test]
fn d5_blocking_native_in_callback_runs_inline() {
    let path = std::env::temp_dir().join(format!("chezzi_d5_cb_exists_{}.txt", std::process::id()));
    std::fs::write(&path, b"x").expect("write temp file");
    let path_str = path.to_str().expect("utf8 temp path").to_string();
    let src = format!(
        "\
import std.fs

fn dbl(x: int) -> int:
    if fs.exists(\"{path_str}\"):
        return x * 2
    return 0

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(dbl)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_callback", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let _ = std::fs::remove_file(&path);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback nursery faulted: {res:?}");
            assert_eq!(out, "12\n");
        }
        Err(_) => panic!(
            "hung — D5 native_reentry gate regressed (offloaded an in-callback blocking native)"
        ),
    }
}

/// D5 owe #3 (Path A) — a blocking `recv` reached **through a chezzi-source HOF** (`iter.map`,
/// `std/iter.chz`) parks (snapshot-suspends) instead of demoting, unlike the native `.map`
/// (`fibers_recv_inside_map_callback_demotes_on_mn`, whose Rust loop frame breaks the snapshot
/// chain and must demote a worker instead). Every frame from the fiber's entry to the `recv` is a
/// VM frame (`map`'s `for`-loop + the closure), so the park-in-place path is sound and no
/// replacement worker is needed. Both mechanisms now converge on the same result (M:N is the only
/// engine), so the thing worth pinning is that Path A really did *park*, not just get lucky with an
/// already-filled buffer: `produce` delays its first `send` (`time.sleep_ms`, same technique as
/// `d5_owe3_path_c_recv_in_native_map_callback_demotes`) so `consume`'s first `recv` is guaranteed
/// empty, forcing a real park deterministically. Sum `66` proves all three recvs threaded through
/// the closure. 30 s watchdog so a park/wake regression hangs loud instead of hanging the suite.
#[test]
fn d5_owe3_recv_in_iter_map_callback_parks() {
    let src = "\
import std.iter
import std.time

fn produce(ch: Channel[int]):
    time.sleep_ms(50)
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn consume(ch: Channel[int], out: Shared[int]):
    ys := iter.map([1, 2, 3], fn(x: int) -> int: x + ch.recv())
    out.update(fn(a): a + ys[0] + ys[1] + ys[2])

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.get())

main()
";
    let entry = write_temp_chz("d5_owe3_iter_map", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "iter.map recv-in-callback nursery faulted: {res:?}"
            );
            assert_eq!(out, "66\n", "iter.map recv-in-callback wrong sum");
        }
        Err(_) => {
            panic!("hung — D5 owe #3 Path A regressed (recv inside iter.map did not park)")
        }
    }
}

/// D5 owe #3 (Path A) — the new `std/iter.chz` HOFs (`map`/`filter`/`fold`/`reduce`) are correct
/// and byte-identical against the golden. `map` to a different return type (`int -> str`)
/// exercises generic-return inference (`U` is bound solely from the closure, not from `xs`), the
/// primary risk flagged in the plan — it works without explicit type args. Cooperative (no
/// `--parallel`): this guards the functional surface; the park behaviour is guarded separately by
/// `d5_owe3_recv_in_iter_map_callback_parks`.
#[test]
fn d5_owe3_iter_hofs_correct_on_both_engines() {
    let src = "\
import std.iter

fn main():
    xs := [1, 2, 3, 4, 5]
    print(iter.map(xs, fn(x: int) -> int: x * x))
    print(iter.filter(xs, fn(x: int) -> bool: x % 2 == 0))
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a + x))
    print(iter.reduce(xs, fn(a: int, b: int) -> int: a * b))
    print(iter.map([1, 2], fn(x: int) -> str: \"n{x}\"))
    # subtraction is non-commutative — locks the left-to-right fold order (0-1-2-3-4-5 = -15)
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a - x))

main()
";
    let entry = write_temp_chz("d5_owe3_hofs", src);
    let cfg = crate::native::HostConfig::default;
    let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
    let (io, _ie, ir, _ic) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(vr.is_ok(), "vm run faulted: {vr:?}");
    assert!(ir.is_ok(), "interp run faulted: {ir:?}");
    assert_eq!(vo, io, "vm/interp stdout divergence");
    assert_eq!(
        vo,
        "[1, 4, 9, 16, 25]\n[2, 4]\n15\n120\n['n1', 'n2']\n-15\n"
    );
}

/// D5 owe #3 (Path C) — a blocking `recv` reached inside a **native** callback (`xs.map`, whose
/// per-element loop frame lives on the Rust host stack and CANNOT be snapshot-parked) no longer
/// faults `deadlock` under `--parallel`: the worker thread is **demoted** (blocks in place on the
/// channel condvar, a fresh replacement worker covers its `wid`), and **resumes in place** when a
/// sibling `send`s — Go's `handoffp`. The contrast to Path A (`d5_owe3_recv_in_iter_map_callback_parks`,
/// where `iter.map` is chezzi source → pure VM frames → snapshot-parks, no demotion needed) and to
/// `fibers_recv_inside_map_callback_demotes_on_mn` (the same native-callback shape, pinned in
/// isolation). The result is written with `Shared.set` (NOT `update`) so the recv site is the `xs.map` callback only,
/// avoiding the `update_lock`-held-while-blocked hazard. Sum `66` = (1+10)+(2+20)+(3+30): all three
/// recvs threaded through the native map callback. Parallel-only, under a 30 s watchdog so a
/// demote/resume hang fails loud instead of hanging the suite. The producer `sleep_ms`s before its
/// first `send` so the consumer's first map-callback `recv` is **guaranteed empty** — forcing the
/// demote path deterministically (without the delay the producer races ahead and pre-fills the FIFO,
/// so the `recv` never blocks and the test would flake-pass even with Path C broken).
#[test]
fn d5_owe3_path_c_recv_in_native_map_callback_demotes() {
    let src = "\
import std.time

fn use_map(ch: Channel[int], out: Shared[int]):
    xs := [1, 2, 3]
    ys := xs.map(fn(x): x + ch.recv())
    out.set(ys[0] + ys[1] + ys[2])

fn fill(ch: Channel[int]):
    time.sleep_ms(50)
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(ch, out)
        spawn fill(ch)
    print(out.get())

main()
";
    let entry = write_temp_chz("d5_owe3_path_c", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "Path C: recv inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(out, "66\n");
        }
        Err(_) => panic!(
            "hung — D5 owe #3 Path C regressed (recv inside native xs.map did not demote-and-resume)"
        ),
    }
}

/// D5 owe #3 (Path C) — a `recv` inside a native callback with **no possible sender** must still
/// **fault `deadlock`**, not hang. This is the load-bearing half of the pragmatic deadlock scope:
/// the demoted thread is registered as a fiber waiter (a 5th fiber state), which feeds
/// [`MnSched::is_deadlocked`] (`parked_n>0 || any_fiber_waiter()`). The demote's registration
/// notifies `cv` so the idle replacement worker re-evaluates the predicate; on fire, `flag_deadlock`
/// sets `terminate`, the demoted thread observes it within `DEMOTE_POLL_BACKOFF` and faults in place,
/// and `wait_for_completion` lets the join reduce the deadlock outcome. Watchdog 30 s: a regressed
/// predicate (or a missing notify) would HANG here instead of faulting.
#[test]
fn d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks() {
    let src = "\
fn use_map(ch: Channel[int]):
    xs := [1]
    ys := xs.map(fn(x): x + ch.recv())
    print(ys)

fn main():
    ch := Channel[int]()
    parallel:
        spawn use_map(ch)

main()
";
    let entry = write_temp_chz("d5_owe3_path_c_dl", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => {
                let s = format!("{e:?}");
                assert!(
                    s.contains("deadlock"),
                    "Path C: no-sender recv-in-callback should fault deadlock, got: {s}"
                );
            }
            Ok(()) => panic!("Path C: no-sender recv-in-callback unexpectedly succeeded"),
        },
        Err(_) => panic!(
            "hung — D5 owe #3 Path C deadlock detection regressed (fiber-waiter predicate / notify)"
        ),
    }
}

/// gaps.md W10-16 — a child task's fault must outrank a synthesized `Deadlocked` verdict for the
/// nursery OWNER parked on a channel `recv`, exactly as `reduce_task_slots` already ranks
/// `Exit` > `Fault` > `Deadlocked` for SIBLING slots (W7-47). Today the owner-parked path bypasses
/// that ranking: the owner's parked `recv` synthesizes `deadlock` and the child's real fault
/// (`task died`) never surfaces. Go (`panic: task died`) is the oracle.
#[test]
fn w10_16_owner_parked_recv_reports_child_fault_not_deadlock() {
    let src = "\
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            panic(\"task died\")
        print(ch.recv())

main()
";
    let entry = write_temp_chz("w10_16_owner_parked", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => {
                let s = format!("{e:?}");
                assert!(
                    s.contains("task died"),
                    "owner-parked recv should report the child's fault \"task died\", got: {s}"
                );
                assert!(
                    !s.contains("deadlock"),
                    "owner-parked recv should not synthesize deadlock when a sibling faulted, got: {s}"
                );
            }
            Ok(()) => panic!("w10_16: expected the child's fault to propagate, program succeeded"),
        },
        Err(_) => panic!("hung — w10_16 owner-parked-recv-vs-child-fault regressed"),
    }
}

/// TICKET-134 — `Vm::block_halt_check` reads the owner's nursery fault, then reads the deadlock
/// verdict, as two separate observations of shared state. A child that faults between them makes the
/// verdict see a complete nursery and report a synthesized `deadlock` instead of the child's real
/// fault. The `owner_fault_window_hook` seam (`src/vm/netio.rs`) widens that window so the race is
/// deterministically reachable: it blocks between the two reads until the child's fault lands.
#[test]
fn owner_fault_recorded_after_the_fault_rung_still_outranks_the_deadlock_verdict() {
    let src = "\
import std.time

fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            time.sleep_ms(300)
            panic(\"task died\")
        print(ch.recv())

main()
";
    let entry = write_temp_chz("t134_owner_fault_window", src);
    let cfg = crate::native::HostConfig::default();
    // The literal equals `netio::OWNER_FAULT_WINDOW_ENV`. It is spelled out, not imported, so this
    // test still compiles on a base without the seam (the pipeline's red-on-base check copies only
    // this file there). The hook `remove`s the key when it arms, so a key still present after the
    // run means THIS run never reached the window.
    const KEY: &str = "CHEZZI_TEST_OWNER_FAULT_WINDOW";
    cfg.env
        .lock()
        .unwrap()
        .insert(KEY.to_string(), "1".to_string());
    let env = std::sync::Arc::clone(&cfg.env);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&run_entry, cfg));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    assert!(
        !env.lock().unwrap().contains_key(KEY),
        "window not exercised: the hook never armed in this run"
    );
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => {
                let s = format!("{e:?}");
                assert!(s.contains("task died"), "got: {s}");
                assert!(!s.contains("deadlock"), "got: {s}");
            }
            Ok(()) => panic!("t134: expected the child's fault to propagate, program succeeded"),
        },
        Err(_) => panic!("hung — t134 owner-fault-window regressed"),
    }
}

/// TICKET-134 — the same race as
/// [`owner_fault_recorded_after_the_fault_rung_still_outranks_the_deadlock_verdict`], through the
/// bounded-`send` funnel instead of `recv`: the owner blocks in `chan_send_step`'s block-in-place
/// loop with the cap-1 buffer already full, and `owner_fault_window_hook` holds the same window open
/// until the child's fault lands.
#[test]
fn owner_fault_recorded_after_the_fault_rung_still_outranks_the_full_send_deadlock_verdict() {
    let src = "\
import std.time

fn main():
    ch := Channel[int](1)
    parallel:
        spawn:
            time.sleep_ms(300)
            panic(\"task died\")
        ch.send(1)
        ch.send(2)

main()
";
    let entry = write_temp_chz("t134_owner_fault_window_send", src);
    let cfg = crate::native::HostConfig::default();
    // The literal equals `netio::OWNER_FAULT_WINDOW_ENV`. It is spelled out, not imported, so this
    // test still compiles on a base without the seam (the pipeline's red-on-base check copies only
    // this file there). The hook `remove`s the key when it arms, so a key still present after the
    // run means THIS run never reached the window.
    const KEY: &str = "CHEZZI_TEST_OWNER_FAULT_WINDOW";
    cfg.env
        .lock()
        .unwrap()
        .insert(KEY.to_string(), "1".to_string());
    let env = std::sync::Arc::clone(&cfg.env);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&run_entry, cfg));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    assert!(
        !env.lock().unwrap().contains_key(KEY),
        "window not exercised: the hook never armed in this run"
    );
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => {
                let s = format!("{e:?}");
                assert!(s.contains("task died"), "got: {s}");
                assert!(!s.contains("deadlock"), "got: {s}");
            }
            Ok(()) => panic!("t134: expected the child's fault to propagate, program succeeded"),
        },
        Err(_) => panic!("hung — t134 owner-fault-window-send regressed"),
    }
}

/// gaps.md W10-1 — a main-task `recv` inside a native re-entry (here: a generator resume) must not
/// fault `deadlock` while a live sibling can still send. Before TICKET-062 the block-in-place gate
/// required a judged party (`native_reentry == 0`), so main-in-callback fell to the "fault, as
/// before" arm even though the sender was alive; `block::mode` now blocks it in place. Go (main goroutine pulling through a
/// closure) prints `0 1 2`. The no-sender case (`d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks`)
/// must stay green — a genuinely unsendable recv must still fault deadlock.
#[test]
fn w10_1_main_recv_in_generator_resume_with_live_sender_does_not_deadlock() {
    let src = "\
fn g(ch: Channel[int]) -> Iterator[int]:
    while true:
        v := ch.recv()
        if v < 0:
            return
        yield v

fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            for i in 0..3:
                ch.send(i)
            ch.send(-1)
        for x in g(ch):
            print(x)

main()
";
    let entry = write_temp_chz("w10_1_main_recv_in_gen", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "main recv inside generator resume faulted with a live sender: {res:?}"
            );
            assert_eq!(out, "0\n1\n2\n");
        }
        Err(_) => panic!("hung — w10_1 main-recv-in-native-reentry regressed"),
    }
}

/// gaps.md W8-7 hang regression — DEMOTE x YIELD. A `recv` inside a native `xs.map` callback demotes
/// the worker (`demote_recv_block` blocks it in place, spinning a REPLACEMENT thread that covers its
/// `wid`); at demote time `runnable == 0`, so that replacement typically parks straight into
/// `take_runnable`'s untimed `cv.wait` (mod.rs D4e). If the demoted fiber then runs long enough after
/// `recv` returns to exhaust `CONTEXT_REDS` (4000 dispatched ops — the tail loop here does ~800k), it
/// preempts: `Disp::Yield` -> `yield_fiber` requeues it with **no notify** (W8-7), and the demoted
/// worker's own `mn_worker_loop` returns immediately after on `self.demoted` (`sched.rs`) without
/// re-entering `take_runnable`. Nobody is left awake to consume the requeued fiber, and the joiner's
/// `wait_for_completion` (also an untimed `cv.wait`) never wakes either: a hang. None of the other
/// demote tests in this file (`d5_owe3_path_c_*`, the `wait`/`sleep_ms`/socket demote sites) catch
/// this class because every one of them finishes its fiber in well under `CONTEXT_REDS` ops after the
/// demote — no `Disp::Yield` ever fires on a demoted worker in that corpus, so a narrowed wake is
/// invisible to it. CLAUDE.md's "a widening is untested by its own suite", inverted: a *narrowing* of
/// when a wake fires is structurally untested by a corpus written before the narrowing landed.
///
/// Forces `worker_count(2)` for a deterministic repro (measured 2/2 hangs at `--threads=1` and
/// `--threads=2` pre-fix; racy at `--threads=4`, where a spuriously-rechecking idle sibling can
/// happen to grab the fiber anyway). 15 s watchdog: a regression HANGS here instead of the whole
/// suite wedging — it must fail loud via the watchdog, not via a stuck test binary.
#[test]
fn w8_7_demoted_fiber_yield_after_demote_does_not_strand_replacement() {
    #[rustfmt::skip]
    let in_parent = crate::vm::rerun_in_child(module_path!(), "w8_7_demoted_fiber_yield_after_demote_does_not_strand_replacement");
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
    crate::vm::set_worker_count(2);

    let src = "\
import std.time
import std.concurrency

fn use_map(ch: Channel[int], out: Shared[int]):
    xs := [0]
    ys := xs.map(fn(x): ch.recv())
    s := 0
    i := 0
    while i < 200000:
        s = s + i
        i = i + 1
    out.set(s + ys[0])

fn fill(ch: Channel[int]):
    time.sleep_ms(50)
    ch.send(1)

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(ch, out)
        spawn fill(ch)
    print(out.get())

main()
";
    let entry = write_temp_chz("w8_7_demote_yield", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(15));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "faulted instead of completing: {res:?}");
            assert_eq!(out, "19999900001\n");
        }
        Err(_) => panic!(
            "hung — a demoted worker's `Disp::Yield` (post-recv, CONTEXT_REDS-exhausted) left its \
             REPLACEMENT worker stranded in an untimed `cv.wait` with no notify (gaps.md W8-7 hang \
             regression: the departure-notify fix in `mn_worker_loop`'s `self.demoted` exit is \
             missing or broken)"
        ),
    }
}

/// §6d M:N wait-in-callback DEMOTE (TDD step 8) — a blocking `wait` reached inside a native `xs.map`
/// callback (`native_reentry > 0`) cannot snapshot-park (the HOF's loop state is on the Rust host
/// stack), so it DEMOTES: blocks the worker in place, polling all N arm queues on a bounded backoff
/// until a sibling `send` to either arm delivers (mirrors `demote_recv_block`, the documented
/// lower-throughput v1 path). The producer `sleep_ms`s before its `send` so the callback's first
/// `wait` poll is GUARANTEED empty — forcing the demote path deterministically. The send lands on
/// the SECOND arm, so the demote loop's source-order N-arm scan is exercised. Watchdog 30 s so a
/// demote/resume hang fails loud. `66` = 1 + 65 (the second-arm value).
#[test]
fn vm_wait_in_native_callback_demotes_under_parallel() {
    let src = "\
import std.time

fn pick(a: Channel[int], b: Channel[int], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        w := b.recv(): return x + w

fn use_map(a: Channel[int], b: Channel[int], out: Shared[int]):
    xs := [1]
    ys := xs.map(fn(x): pick(a, b, x))
    out.set(ys[0])

fn fill(b: Channel[int]):
    time.sleep_ms(50)
    b.send(65)

fn main():
    a := Channel[int]()
    b := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(a, b, out)
        spawn fill(b)
    print(out.get())

main()
";
    let entry = write_temp_chz("vm_wait_callback_demote", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "wait inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(out, "66\n");
        }
        Err(_) => panic!("hung — M:N wait-in-callback demote-and-resume regressed"),
    }
}

/// WAIT-1 (HIGH) on the DEMOTE path — a `wait` over a long timer + data channel reached INSIDE a
/// native `xs.map` callback (`native_reentry > 0`, cannot snapshot-park) must let a mid-window
/// sibling `send` win the data arm, NOT pin the worker on the 2000ms timer. The demote poll loop's
/// source-order channel scan must beat the timer deadline. Pre-fix the demote loop had no deadline
/// handling and behaved inconsistently / could mishandle the timer arm. 30s watchdog.
#[test]
fn vm_wait_timer_loses_to_send_in_native_callback_parallel() {
    let src = "\
import std.time

fn pick(a: Channel[int], t: Channel[bool], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        _ := t.recv(): return -1

fn use_map(a: Channel[int], t: Channel[bool], out: Shared[int]):
    xs := [0]
    ys := xs.map(fn(x): pick(a, t, x))
    out.set(ys[0])

fn fill(a: Channel[int]):
    time.sleep_ms(5)
    a.send(7)

fn main():
    a := Channel[int]()
    t := timer(2000)
    out := Shared(0)
    parallel:
        spawn use_map(a, t, out)
        spawn fill(a)
    print(out.get())

main()
";
    let entry = write_temp_chz("vm_wait_callback_timer", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "wait+timer inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(
                out, "7\n",
                "the timer arm took the wait instead of the mid-window send (WAIT-1, demote path)"
            );
        }
        Err(_) => panic!("hung — M:N wait-in-callback timer demote regressed"),
    }
}

/// §6d M:N wait-in-callback DEMOTE with no possible sender must still fault `deadlock`, not hang —
/// the demote loop's self-detected-deadlock path (`is_deadlocked` over the registered arm channels).
#[test]
fn vm_wait_in_native_callback_no_sender_deadlocks() {
    let src = "\
fn pick(a: Channel[int], b: Channel[int], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        w := b.recv(): return x + w

fn use_map(a: Channel[int], b: Channel[int]):
    xs := [1]
    ys := xs.map(fn(x): pick(a, b, x))
    print(ys)

fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:
        spawn use_map(a, b)

main()
";
    let entry = write_temp_chz("vm_wait_callback_dl", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => assert!(
                format!("{e:?}").contains("deadlock"),
                "no-sender wait-in-callback should deadlock: {e:?}"
            ),
            Ok(()) => panic!("no-sender wait-in-callback unexpectedly succeeded"),
        },
        Err(_) => panic!("hung — M:N wait-in-callback deadlock detection regressed"),
    }
}

/// D5 owe #3 Path C (#1 false-positive) — the deadlock checker must NOT fault an innocent parked
/// sibling when a demoted fiber has a value racing into its queue. F1 demotes on `a` inside a native
/// `xs.map`, then wakes F2 via `c.send(7)`; F2 snapshot-parks on `c`; F3 feeds `a` then finishes.
/// The bad interleaving (microseconds): F3's `running→0` quiesce can fire the predicate before F1
/// pops its queued `10`, wrongly killing the parked F2. With the #1 fix (`is_deadlocked` peeks the
/// demoted channel `a`, which holds `10`) the fire is vetoed. Run many times to expose the race; the
/// output must ALWAYS be `7` and NEVER a spurious `deadlock`. Watchdog per iteration.
#[test]
fn d5_owe3_path_c_no_false_deadlock_when_demoted_fiber_has_queued_value() {
    let src = "\
fn main():
    a := Channel[int]()
    c := Channel[int]()
    xs := [1]
    parallel:
        spawn:
            xs.map(fn(x): x + a.recv())
            c.send(7)
        spawn: print(c.recv())
        spawn: a.send(10)

main()
";
    let entry = write_temp_chz("d5_owe3_path_c_fp", src);
    for i in 0..200 {
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_with(
                &run_entry,
                crate::native::HostConfig::default(),
            ));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        // Remove the temp file BEFORE asserting so an assert panic doesn't leak it (the other paths
        // out of this test all unwind, and the post-loop cleanup never runs on panic).
        if i == 199 {
            let _ = std::fs::remove_file(&entry);
        }
        match result {
            Ok((out, _err, res, _code)) => {
                if res.is_err() || out != "7\n" {
                    let _ = std::fs::remove_file(&entry);
                }
                assert!(
                    res.is_ok(),
                    "iter {i}: spurious fault (the #1 false-positive killing the parked sibling?): {res:?}"
                );
                assert_eq!(out, "7\n", "iter {i}: wrong output");
            }
            Err(_) => {
                let _ = std::fs::remove_file(&entry);
                panic!("iter {i}: hung — D5 owe #3 Path C regressed");
            }
        }
    }
}

/// D5 owe #1 — a blocking *subprocess* native (`process.cmd`, returns `Result[str]`) is offloaded,
/// runs off the core worker, and its `Ok`/`Err` result is lowered + pushed on resume so the `match`
/// continues correctly past the call. `N` fibers (≫ the core pool) each run a trivial command and
/// bump a `Shared` on `Ok` — the join sum must be exactly `N` (every offloaded `cmd` returned `Ok`
/// and resumed into the arm). Guards the request/process classification + the Result-lowering
/// resume path for a non-`io`/`fs` blocking native. Watchdog 30 s.
#[test]
fn d5_owe1_blocking_process_cmd_offloads_and_resumes_correctly() {
    let n = 64usize;
    let src = format!(
        "\
import std.process

fn checker(sink: Shared[int]):
    match process.cmd(\"true\"):
        ?out: sink.update(fn(x): x + 1)
        !e: print(e.message())

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink)
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_owe1_cmd", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "process.cmd nursery faulted: {res:?}");
            assert_eq!(out, format!("{n}\n"));
        }
        Err(_) => panic!("hung — D5 owe #1 process.cmd offload/resume regressed"),
    }
}

/// C-ABI FFI under the M:N engine: an extern fn called INSIDE a `spawn:` block exercises the
/// `SnapValue::Cffi` snapshot path (the worker re-allocs `Obj::Cffi` from the shared `Arc` — no
/// re-dlopen, same address space). Must produce deterministic output.
/// Linux-only (needs libm.so.6). This is the hard proof from the FFI plan.
#[test]
#[cfg(target_os = "linux")]
fn extern_in_spawn_parallel_snapshot() {
    let src = "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\n\n\
                   ch := Channel[float]()\nparallel:\n    spawn:\n        ch.send(sqrt(9.0))\n\
                   r := ch.recv()\nprint(r)\n";
    let entry = write_temp_chz("ffi_spawn", src);
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(vm_res.is_ok(), "VM faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "parallel engine faulted: {par_res:?}");
    assert_eq!(vm_out, "3.0\n");
}

/// Regression (blocker): an extern fn with an explicit `-> nil` (void) return must RUN, not panic.
/// The compiler's `ctype_of("nil")` is `None` meaning *void*, so the return slot must be built with
/// `and_then` (None ⇒ void), never `.expect`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_explicit_nil_return_runs() {
    let src = "extern \"libc.so.6\":\n    fn srand(seed: int) -> None\n\nsrand(1)\nprint(42)\n";
    let entry = write_temp_chz("ffi_nilret", src);
    let (out, _e, res, _) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "VM faulted on `-> None` extern: {res:?}");
    assert_eq!(out, "42\n");
}

/// Regression (blocker): a type alias defined in an IMPORTED module, `from`-imported and used at
/// an extern signature, must lower to its underlying C type. The checker resolves it (module-scoped
/// types: a cross-module alias is reachable via `import Size from sizes`, not bare), and the FFI
/// backend consumes that checker-resolved C type — else `ctype_of` returns `None` and the call site
/// silently drops a scalar return. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_cross_module_alias_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_xmod_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sizes.chz"), "type Size = int\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import Size from sizes\n\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Size\n\nprint(strlen(\"hello\"))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on cross-module alias extern: {vm_res:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on cross-module alias extern: {par_res:?}"
    );
    assert_eq!(vm_out, "5\n");
}

/// TICKET-061 W10-14: an extern fn is a module-global callable and exports like any other top-level
/// `fn` — both `c.strlen(...)` and `import strlen from c` must resolve and run. `run_file` alone
/// skips the checker (a compiler/runtime-only helper), so this asserts `check_graph` succeeds FIRST
/// (a checker-superset guard) — otherwise the runtime member binding could mask a checker still
/// rejecting the program (which is exactly what `chezzi run` would see). Linux-only (needs
/// libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_fn_is_an_importable_module_member_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_member_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("c.chz"),
        "extern \"libc.so.6\":\n    fn strlen(s: str) -> int\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import c\nimport strlen from c\nprint(c.strlen(\"abc\"))\nprint(strlen(\"abcd\"))\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (out, _e, res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(res.is_ok(), "faulted: {res:?}");
    assert_eq!(out, "3\n4\n");
}

/// Module-qualified enum-variant patterns (`geo.Color.Red`) in match arms, symmetric with
/// construction. The module binder is validated by the checker then dropped — the runtime matches
/// purely by the bare enum/variant identity, byte-for-byte against the golden. Covers:
/// whole-module `import geo` qualified arms, an `import geo as g` aliased binder, and a
/// payload-binding `geo.Shape.Circle(r)` arm.
#[test]
fn match_module_qualified_variant_three_engine_parity() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_mqvp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("geo.chz"),
        "enum Color:\n    Red\n    Green\n\nenum Shape:\n    Circle(int)\n    Square(int)\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import geo\nimport geo as g\n\nfn name(c: geo.Color) -> str:\n    match c:\n        geo.Color.Red: return \"red\"\n        geo.Color.Green: return \"green\"\n\nfn area(s: geo.Shape) -> int:\n    match s:\n        geo.Shape.Circle(r): return r * r\n        geo.Shape.Square(w): return w * w\n\nfn aliased(c: geo.Color) -> str:\n    match c:\n        g.Color.Red: return \"R\"\n        g.Color.Green: return \"G\"\n\nfn main():\n    print(name(geo.Color.Red))\n    print(name(geo.Color.Green))\n    print(area(geo.Shape.Circle(4)))\n    print(area(geo.Shape.Square(3)))\n    print(aliased(geo.Color.Green))\nmain()\n",
        )
        .unwrap();
    let cfg = crate::native::HostConfig::default;
    let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
    let (_po, _pe, pr, _pc) = run_file_with(&entry, cfg());
    let (io, _ie, ir, _ic) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(pr.is_ok(), "--parallel faulted: {pr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    let expected = "red\ngreen\n16\n9\nG\n";
    assert_eq!(vo, expected, "VM output");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// Bug #1 (L2) — a struct reached via a WHOLE-module import (`import geo`) is destructured with a
/// QUALIFIED struct pattern `geo.Point(x, y)` (the bare name is not in scope). Must RUN byte-identically
/// (a checker-superset guard: check_graph is asserted first).
#[test]
fn struct_match_qualified_whole_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_sqwm_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("geo.chz"),
        "struct Point:\n    x: int\n    y: int\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import geo\nfn f(p: geo.Point) -> int:\n    match p:\n        geo.Point(x, y): return x + y\nfn main():\n    print(f(geo.Point(3, 4)))\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (_io, _ie, ir, _ic) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(vo, "7\n", "VM output");
}

/// M24 Task 3 — a static-witness call ACROSS a module boundary must RUN,. Type-check
/// alone proves nothing here: the hidden argument is the concrete type's runtime IDENTITY KEY, which
/// is the OWNING module's (`<module-key>::Name`) — a wrong key surfaces only at run time, as
/// `type 'X' has no static method 'default'`. Every direction is exercised: the qualified callee
/// (`lib.reset`), the `from`-imported one (bare + aliased), a LOCAL type witnessing the IMPORTED
/// generic, an IMPORTED type witnessing a LOCAL generic, and a local generic FORWARDING its own
/// still-abstract witness into the imported one.
#[test]
fn witness_cross_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_xmod_witness_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\n\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\n\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n",
    )
    .unwrap();
    // A THIRD module in the middle: its `twice` forwards its own still-abstract witness into `lib`'s
    // `reset` — so a type declared in the ENTRY module reaches its `default()` across TWO boundaries.
    std::fs::write(
        dir.join("mid.chz"),
        "import lib\nimport reset, Default from lib\n\nfn twice[U: Default](x: U) -> U:\n    return reset(lib.reset(x))\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nimport mid\nimport reset, Default from lib\nimport reset as again from lib\n\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\n\nfn mine[T: Default](x: T) -> T:\n    return T.default()\n\nfn fwd[U: Default](x: U) -> U:\n    return lib.reset(x)\n\nfn main():\n    print(lib.reset(lib.Counter(1)).n)\n    print(reset(lib.Counter(1)).n)\n    print(again(lib.Counter(1)).n)\n    print(lib.reset[lib.Counter](lib.Counter(1)).n)\n    print(reset(Local(\"x\")).k)\n    print(mine(lib.Counter(1)).n)\n    print(fwd(lib.Counter(1)).n)\n    print(fwd(Local(\"y\")).k)\n    print(mid.twice(Local(\"z\")).k)\n    print(mid.twice(lib.Counter(1)).n)\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (_io, _ie, ir, _ic) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "7\n7\n7\n7\nloc\n7\n7\nloc\nloc\n7\n",
        "VM output (a wrong identity key faults, a wrong witness prints another type)"
    );
}

/// M24 Task 5 — the same, for a witness declared BY A MEMBER: an instance method and a static method
/// on an IMPORTED type, witnessed by both an imported and a LOCALLY-declared type. The member's proto
/// is compiled in the DECLARING module (which is where its hidden param is added) while the witness
/// constant is pushed by the CALLING one, so the two modules' identity keys must agree — a mismatch
/// only shows up at run time.
#[test]
fn witness_member_cross_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_xmod_member_w_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\n\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\n\nstruct Holder:\n    v: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nimport Holder, Counter from lib\n\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\n\nfn main():\n    print(lib.Holder(1).make(lib.Counter(9)).n)\n    print(lib.Holder(1).make(Local(\"x\")).k)\n    print(lib.Holder.build(lib.Counter(9)).n)\n    print(lib.Holder.build(Local(\"x\")).k)\n    print(Holder(2).make(Counter(1)).n)\n    print(Holder.build(Local(\"y\")).k)\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (_io, _ie, ir, _ic) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "7\nloc\n7\nloc\n7\nloc\n",
        "VM output (a wrong identity key faults, a wrong witness builds the other type)"
    );
}
