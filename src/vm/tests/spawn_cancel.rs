//! VM tests: spawn-cancel on `parallel:` escape, cooperative fibers, runner permits.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- TASK B / §2c1: spawn-cancel on early `parallel:` escape -----
// Policy: a spawn task on a `parallel:` that escapes early (`?`/`return`/`break`) before its join
// is CANCELLED. The escape propagates unchanged and the nursery stack stays leak-free (depth 0).
//
// §2c1 CHANGED WHAT IS ASSERTABLE HERE, twice.
//
// 1. **No report line.** These tests used to pin `"{n} pending task(s) cancelled on early exit from
//    parallel:"` on stdout. That line is deleted: a task now starts at its `spawn`, so there are no
//    unstarted tasks to count, and a residual count would be racy.
// 2. **"the task did NOT run" is no longer a fact on the M:N engine.** The task is a live fiber by
//    the time the body escapes, so whether its straight-line prologue got a slice is a RACE — the
//    same property trio and `asyncio.TaskGroup` have, and the same one
//    `golden_tests::exit_in_spawned_child_aborts_siblings` already documents ("a spawned task always
//    runs its straight-line prologue before it can observe that cancel"). So the spawned bodies
//    below are SILENT, and what is asserted is what is actually invariant: the escape's own value
//    wins, nothing extra reaches stdout, and the nursery is reclaimed.
//
// That the cancel still BITES — that an escape does not leave a started task running — is pinned
// deterministically by `an_escaped_nursery_cancels_a_started_task` below, which parks the task
// forever and requires the program to terminate anyway.

/// `?` escape: the escape faults the program, nothing extra reaches stdout, and the nursery depth
/// returns to 0 (no leak). White-box depth check.
#[test]
fn parallel_try_escape_cancels_pending_silently() {
    // `main()?`, not a bare `main()`: a bare top-level drop no longer aborts, so the `?` is
    // what keeps the program faulting.
    let src = "fn side():\n    0\n\
                   fn boom() -> int!:\n    return !\"x\"\n\
                   fn main() -> int!:\n    parallel:\n        spawn side()\n        y := boom()?\n        print(y)\n    return 0\nmain()?\n";
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert!(vm_out.is_err(), "the uncaught ? faults the program");
    assert_eq!(
        depth, 0,
        "the ?-escaped nursery must be reclaimed, not leaked"
    );
    // Stdout captured up to the fault: empty — the cancel is silent.
    let (vm_so_far, vm_res) = run_program(src);
    assert!(vm_res.is_err());
    assert_eq!(vm_so_far, "", "nothing on stdout before the fault");
    let (mn_so_far, mn_res) = run_program(src);
    assert!(mn_res.is_err());
    assert_eq!(mn_so_far, vm_so_far, "stdout-so-far divergence");
    assert!(run_capture(src).is_err(), "M:N also faults");
}

/// §2c1 — the cancel still BITES on a task that HAS started. `side()` parks forever on a channel
/// nobody feeds; the body then `return`s out of the `parallel:` before its join. If the escape did
/// not cancel the live fiber, the reclaim would wait for it and the program would never terminate.
/// The `5` proves the escape's own value still wins,.
#[test]
fn an_escaped_nursery_cancels_a_started_task() {
    let src = "fn side(ch: Channel[int]):\n    _ := ch.recv()\n\
                   fn worker() -> int:\n    ch := Channel[int]()\n    parallel:\n        spawn side(ch)\n        return 5\n    99\n\
                   fn main():\n    print(worker())\nmain()\n";
    assert_eq!(run(src), "5\n", "the escape value wins");
    assert_eq!(
        run_capture(src).expect("M:N run"),
        "5\n",
        "M:N: the started task is cancelled, so the escape still completes"
    );
}

/// `return` escape: the early `return` value still wins and the cancel is silent.
#[test]
fn parallel_return_escape_cancels_pending_silently() {
    let src = "fn side():\n    0\n\
                   fn worker() -> int:\n    parallel:\n        spawn side()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\nmain()\n";
    let expected = "5\n".to_string();
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert_eq!(
        vm_out.as_deref().map(str::to_string),
        Ok(expected.clone()),
        "VM"
    );
    assert_eq!(
        depth, 0,
        "the return-escaped nursery must be reclaimed, not leaked"
    );
    assert_eq!(
        run_capture(src).expect("repeat run"),
        expected,
        "repeat run"
    );
    assert_eq!(
        run_capture(src).expect("parallel run"),
        expected,
        "parallel run"
    );
}

/// `break`-in-loop escape: the NET-NEW VM site (a `break` that leaves a `parallel:` scope via the
/// in-frame loop-exit Jump, NOT via `do_return`). The loop exits, the function continues, the
/// cancel is silent. Byte-identical, depth 0.
#[test]
fn parallel_break_escape_cancels_pending_silently() {
    let src = "fn side():\n    0\n\
                   fn main():\n    for i in 0..3:\n        parallel:\n            spawn side()\n            if i == 0:\n                break\n            print(\"unreached\")\n    print(\"done\")\nmain()\n";
    // i==0: spawn side(), then break out of the `parallel:` scope before the join → cancel, exit
    // the loop. "unreached" never prints.
    let expected = "done\n".to_string();
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert_eq!(
        vm_out.as_deref().map(str::to_string),
        Ok(expected.clone()),
        "VM (net-new break site)"
    );
    assert_eq!(
        depth, 0,
        "the break-escaped nursery must be reclaimed, not leaked"
    );
    assert_eq!(
        run_capture(src).expect("repeat run"),
        expected,
        "repeat run"
    );
    assert_eq!(
        run_capture(src).expect("parallel run"),
        expected,
        "parallel run"
    );
}

/// B3.4: a `recv`-blocked sibling must ABORT when a sibling faults before sending, instead of
/// hanging the join forever. `boom` is spawned first so it runs inline on the joining thread —
/// it faults immediately and trips the nursery cancel flag without depending on pool scheduling
/// (avoids the G3 pool-starvation hazard on low-core CI). `consumer` runs on the pool, blocks on
/// the empty channel, and its re-checking `recv` wait observes the cancel and unwinds — so the
/// join completes and reports the producer's fault rather than deadlocking.
#[test]
fn parallel_recv_blocked_sibling_aborts_on_sibling_fault() {
    let src = "fn boom(ch: Channel[int]):\n    xs := [1]\n    print(xs[9])\n\
                   fn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn boom(ch)\n        spawn consumer(ch)\nmain()\n";
    let err = run_capture(src).expect_err("expected the producer fault to propagate, not hang");
    assert!(
        err.message.contains("out of bounds"),
        "got: {}",
        err.message
    );
}

/// B3.4: a CPU-bound sibling aborts mid-flight when a sibling faults, observing the cancel flag
/// at a dispatch back-edge. `looper` runs inline (task[0]); it writes `1`, hands `trigger` a
/// channel token (so the fault happens-after `looper` has started — no timing race), then spins
/// and would write `99` only after the (huge) loop. `trigger` (pool) waits for the token, then
/// faults → trips cancel → `looper` aborts mid-loop. Asserting `1` proves `looper` started AND
/// was cancelled before completing; without the back-edge cancel check it would print `99`.
#[test]
fn parallel_cpu_sibling_aborts_on_sibling_fault() {
    let src = "import Shared from std.concurrency\nfn looper(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn looper(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture(src).expect("the fault is recovered, so the program completes");
    assert_eq!(
        out, "1\n",
        "looper started (1) but was cancelled before completing (never wrote 99)"
    );
}

/// B3.4: `defer` still composes with cancellation. The blocked consumer is aborted when the
/// producer faults; its `defer cleanup(s)` must still run on the cancel unwind (writing the
/// shared sentinel), proving deferred calls fire even on a cancelled task.
///
/// Synchronized like [`parallel_cpu_sibling_aborts_on_sibling_fault`]: `boom` waits for a token
/// `consumer` sends only AFTER it has registered its `defer` and is about to block, so the fault
/// happens-after the defer is registered. (Under the M:N engine task start order is not
/// deterministic, so a fault that races a not-yet-started sibling would legitimately skip its
/// not-yet-registered defer, per Go semantics; this token makes the intended "blocked consumer is
/// aborted" scenario the one actually exercised.)
///
/// The token closes ONLY the defer-REGISTRATION race — it does NOT make this test race-free (an
/// earlier version of this comment wrongly claimed "no timing race"). The surviving race was
/// scheduler-side and AFTER the fault: the cancel trip and its `cancel_drain` sit two core-lock
/// acquisitions apart, and an idle worker's deadlock check landing in that gap used to reap the
/// still-parked consumer as `Deadlocked`, dropping it WITHOUT `unwind_deferred` — so this printed
/// `0` roughly 35/200 runs under CPU contention. That is closed by the cancel-teardown veto in
/// `is_deadlocked` (see `SchedCore::any_incomplete_scope_cancelled`, src/vm/mod.rs). This test is
/// the REGRESSION guard for it: a failure here means the veto is gone, not that the token is flaky.
#[test]
fn parallel_defer_runs_on_cancelled_sibling() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
                   fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture(src).expect("the producer fault is recovered, so the program completes");
    assert_eq!(
        out, "42\n",
        "the cancelled consumer's defer ran on the unwind"
    );
}

/// Cancellation points: a task spinning in an UNBOUNDED `while true:` loop must still be cancelled
/// promptly when a sibling faults — that is exactly what the loop BACK-EDGE checkpoint is for
/// (`Vm::jump_checked`). A regression (e.g. only one of the two `Op::Jump` dispatch sites wired)
/// HANGS the nursery here instead of failing an assert.
#[test]
fn parallel_spinning_sibling_does_not_hang_the_nursery_under_cancel() {
    let src = "import Shared from std.concurrency\nfn spinner(go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    i := 0\n    while true:\n        i = i + 1\n\
                   fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn spinner(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the spinning sibling must be cancelled at a loop back-edge, not hang the nursery")
        .expect("the trigger fault is recovered, so the program completes");
    assert_eq!(out, "42\n", "the spinner was cancelled and ran its defer");
}

/// N4 (second seam — `abort_enlisted_scope`): a cancelled task's `defer` must run when the cancel
/// comes from an EARLY-ENLISTED outer nursery whose body ESCAPES past its join (here: a nested
/// `parallel:` inside the outer body → `early_enlist_outer` seeds the outer scope's tasks as live
/// fibers and marks it `awaiting_builder`; the `return` then escapes → `drain_escaped_nursery` →
/// `abort_enlisted_scope`). That seam used to CLEAR the `awaiting_builder` deadlock veto before it
/// tripped the scope cancel (which arms the cancel-teardown veto), leaving a window with NEITHER: an
/// idle worker's `take_runnable` in that window saw the quiesce (the inline builder is not counted in
/// `running`), declared a spurious DEADLOCK, and `flag_deadlock` DROPPED the parked `task` fiber
/// without `unwind_deferred` — its `defer` never ran, invisibly (`abort_enlisted_scope` discards the
/// reduce, so not even the bogus `Deadlocked` surfaced). Fixed by tripping the cancel FIRST (a gapless
/// veto handoff — `MnSched::trip_scope_cancel`, under the core lock).
///
/// `gate` (the nested nursery's task) consumes the token `task` sends only AFTER registering its
/// `defer`, so the inner join — and therefore the escape — happens-after the defer is registered.
/// Natural window: `cleanup=0` ~1/100 under CPU contention; with a 30ms sleep probing the gap it was
/// 20/20 pre-fix and 0/20 after. M:N only (`run_capture`): the since-removed serial engine never
/// started a
/// lazy nursery's pending task before the escape, so no `defer` was registered there at all and it
/// printed `0` — a pre-existing engine divergence in what a not-yet-started task does (gaps.md N6), not
/// this bug.
#[test]
fn parallel_defer_runs_when_enlisted_nursery_escapes() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn task(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
                   fn gate(go: Channel[int]):\n    go.recv()\n\
                   fn outer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    parallel:\n        spawn task(ch, go, s)\n        parallel:\n            spawn gate(go)\n        return\n\
                   fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    s := Shared(0)\n    outer(ch, go, s)\n    print(s.get())\nmain()\n";
    let out = run_capture(src).expect("the escape is not a fault — the program completes");
    assert_eq!(
        out, "42\n",
        "the enlisted scope's cancelled task unwound through its defer (no spurious deadlock reaped it)"
    );
}

/// N4 (panic-fault seam): a worker-VM PANIC (a VM bug, a panicking native/FFI callback) becomes a task
/// `Fault` via `run_one_fiber`'s `catch_unwind` fallback. Without a trip of its scope cancel the scope
/// aborts with `cancel == false`: `cancel_drain` requeues the parked siblings, they re-run `recv`,
/// `park`'s gap re-check sees no cancel and PARKS THEM AGAIN, the scope quiesces uncancelled,
/// `is_deadlocked` fires (by its own rules — nothing is cancelled), and `flag_deadlock` drops them
/// without `unwind_deferred` — their `defer`s silently skipped, hidden behind the panic-fault
/// (`reduce_task_slots` ranks Fault > Deadlocked). `MnSched::finish_with` is the one trip of a task's
/// aborting end, so the grid holds every outcome kind: a `Fault` (a panic's included) and an `Exit`
/// trip the flag the fiber ran under, under the core lock with the slot; the others trip nothing.
/// Unit-level because there is no Chezzi-source way to panic a worker VM (every runtime error is an
/// `Err(RuntimeError)`; even integer overflow is a clean fault).
#[test]
fn an_aborting_outcome_trips_the_fibers_cancel_at_finish() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let panic = vm.panic_outcome(Box::new("worker boom"), Span::RUNTIME);
    assert!(
        matches!(panic, TaskOutcome::Fault { .. }),
        "a panicking task is a Fault"
    );
    let grid: Vec<(&str, TaskOutcome, bool)> = vec![
        ("panic fault", panic, true),
        (
            "fault",
            TaskOutcome::Fault {
                err: dl_err(),
                out: Vec::new(),
                stderr: Vec::new(),
                trace: Vec::new(),
            },
            true,
        ),
        (
            "exit",
            TaskOutcome::Exit {
                code: 3,
                out: Vec::new(),
                stderr: Vec::new(),
            },
            true,
        ),
        (
            "done",
            TaskOutcome::Done(WorkerResult {
                out: Vec::new(),
                stderr: Vec::new(),
            }),
            false,
        ),
        (
            "cancelled",
            TaskOutcome::Cancelled {
                out: Vec::new(),
                stderr: Vec::new(),
            },
            false,
        ),
        (
            "cancelled fault",
            TaskOutcome::CancelledFault {
                err: dl_err(),
                out: Vec::new(),
                stderr: Vec::new(),
            },
            false,
        ),
        (
            "deadlocked",
            TaskOutcome::Deadlocked {
                err: dl_err(),
                out: Vec::new(),
                stderr: Vec::new(),
            },
            false,
        ),
    ];
    for (what, outcome, trips) in grid {
        let scope = Arc::new(AtomicBool::new(false));
        let mine = AtomicBool::new(false);
        let sched = MnSched::new(1, 1, Arc::clone(&scope), dl_err(), 0);
        sched.seed(vec![mk_fiber(0)]);
        let f = take_run(&sched);
        let _ = sched.finish_with(f.task_index, 0, outcome, Some(&mine));
        assert_eq!(
            mine.load(Ordering::Relaxed),
            trips,
            "{what}: the fiber's own flag"
        );
        assert!(
            !scope.load(Ordering::Relaxed),
            "{what}: only the fiber's own flag trips"
        );
        assert!(
            sched.lock().slots[f.task_index].is_some(),
            "{what}: the slot is recorded in the same lock hold"
        );
    }
}

/// B3.4: `defer` runs even when a task is cancelled at the CPU **back-edge** (not only on the
/// recv path). `worker` (inline) registers `defer cleanup(s)`, signals `trigger` it has started,
/// then spins; cancelled mid-loop, the unwind must run the defer (writing 42). Regression guard:
/// a raw `return Err` from the loop top would bypass the defer machinery — this asserts the
/// cancel unwinds through `unwind_deferred`. (Without the fix this prints `0`.)
#[test]
fn parallel_defer_runs_on_back_edge_cancel() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn worker(go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn worker(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture(src).expect("the trigger fault is recovered, so the program completes");
    assert_eq!(
        out, "42\n",
        "the CPU-cancelled worker's defer ran on the back-edge unwind"
    );
}

/// B3.4: cancellation is NOT catchable by a `recover:` inside a worker — a cancelled task must
/// die, not resume. `victim` (inline) writes `1`, signals it has started, then wraps a long loop
/// in `recover:` and would write `99` after it. If the cancel sentinel were an ordinary catchable
/// fault, the inner `recover:` would swallow it and `victim` would reach `s.set(99)`; the bypass
/// unwinds past the recover instead, so the sentinel stays at the pre-loop `1`. (Buggy: `99`.)
#[test]
fn parallel_recover_inside_worker_does_not_catch_cancel() {
    let src = "import Shared from std.concurrency\nfn victim(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    r := recover:\n        i := 0\n        while i < 1000000000:\n            i = i + 1\n        0\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn victim(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture(src).expect("the trigger fault is recovered, so the program completes");
    assert_eq!(
        out, "1\n",
        "victim's inner recover must NOT catch the cancel; it never reaches s.set(99)"
    );
}

/// C2 golden: `Channel[T]` fan-out — workers `send` at the dedent, the parent `recv`s after the
/// join. The three workers send **concurrently**, so the queued strings arrive in either order (see
/// `channel.chz`'s own header comment) — the line SET is asserted against `expected`, not the exact
/// order. The second assertion compares two M:N runs **against each other**, a stability check, not
/// an expectation.
#[test]
fn golden_channel_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/channel.chz");
    let expected = include_str!("../../../examples/channel.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_same_lines(expected, &vm_out);
    assert_same_lines(&vm_out, &run_capture(src).expect("M:N run"));
}

/// `Atomic[int].add` cross-thread atomicity: N real-OS-thread fibers each `add(1)` one shared
/// box; the join sum must be exactly N (no lost read-modify-write — the whole point of `Atomic`).
#[test]
fn parallel_atomic_add_is_exact() {
    let n = 300;
    let src = format!(
        "import Atomic from std.concurrency\nfn work(a: Atomic[int]):\n    a.add(1)\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn work(a)\n    print(a.load())\nmain()\n"
    );
    assert_eq!(run_capture(&src).expect("parallel"), format!("{n}\n"));
}

/// `Atomic[int].cas` under contention: N fibers each increment via a load-then-CAS retry loop. A
/// lost CAS (the box changed under us) retries, so the serialised total is exactly N — proving the
/// compare-and-swap is atomic across threads.
#[test]
fn parallel_atomic_cas_increment_is_exact() {
    let n = 200;
    let src = format!(
        "import Atomic from std.concurrency\nfn bump(a: Atomic[int]):\n    while true:\n        cur := a.load()\n        if a.cas(cur, cur + 1):\n            break\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn bump(a)\n    print(a.load())\nmain()\n"
    );
    assert_eq!(run_capture(&src).expect("parallel"), format!("{n}\n"));
}

/// `Atomic[float]` add/sub/exchange/cas must behave identically (covers the
/// numeric-`T` arm for floats, not just ints).
#[test]
fn atomic_float_ops_two_engine_parity() {
    let src = "import Atomic from std.concurrency\nfn main():\n    a := Atomic(1.5)\n    print(a.add(2.0))\n    print(a.sub(0.5))\n    print(a.exchange(9.0))\n    print(a.cas(9.0, 4.0))\n    print(a.load())\nmain()\n";
    // Hand-derived from `add`/`sub` returning the NEW value and `exchange` returning the OLD value
    // (the same contract `examples/atomic.chz`/`.expected` already pins): 1.5+2.0=3.5; 3.5-0.5=3.0;
    // exchange(9.0) returns the pre-swap 3.0 and sets 9.0; cas(9.0, 4.0) matches (holds 9.0) -> true,
    // sets 4.0; load() -> 4.0. Floats print with a trailing `.0` (`run("print(float(3))")` ==
    // "3.0\n", tests.rs).
    assert_eq!(run_capture(src).expect("vm"), "3.5\n3.0\n3.0\ntrue\n4.0\n");
}

/// `cas` on a non-scalar `T` (a list) exercises the VM's lock-held `values_equal` path
/// — the most distinctive Atomic code path. Both engines must agree.
#[test]
fn atomic_cas_on_list_two_engine_parity() {
    let src = "import Atomic from std.concurrency\nfn main():\n    a := Atomic([1, 2])\n    print(a.cas([1, 2], [9]))\n    print(a.load())\n    print(a.cas([1, 2], [0]))\n    print(a.load())\nmain()\n";
    // Hand-derived: current [1, 2] == expected [1, 2] (values_equal) -> cas #1 succeeds, swaps to
    // [9]; cas #2 compares the NEW current [9] against expected [1, 2] -> no match -> false, no
    // swap, so `load()` still reads [9] both times.
    assert_eq!(run_capture(src).expect("vm"), "true\n[9]\nfalse\n[9]\n");
}

/// `timer(0)` (and any already-elapsed deadline) delivers `true` immediately, on every engine.
#[test]
fn timer_zero_delivers_immediately() {
    let src = "import timer from std.time\nfn main():\n    print(timer(0).recv())\nmain()\n";
    let out = run_capture(src).expect("vm");
    assert_eq!(out, "true\n");
    assert_eq!(run_capture(src).expect("parallel"), "true\n");
}

/// A1 golden: `Channel[T].try_recv()` — a non-blocking poll returning `T?`. Workers `send` at the
/// dedent; the parent drains with `try_recv` (`Some` per value, then `None`). Never blocks/faults.
///
/// **The three `got` lines are a SET, not a sequence, and this test used to assert the sequence.**
/// Three workers `send` into one channel concurrently, so which arrives first is scheduling, not
/// semantics. The exact `assert_eq!(vm_out, expected)` was sound only while the first `run_capture`
/// meant the cooperative single-thread engine, which ran the workers in spawn order; with that
/// engine deleted every run is M:N and the pin became a load-sensitive flake — reproduced at 1 in 4
/// full `cargo test --lib` runs on a clean tree, and never once when the test runs alone. It is the
/// same defect as the order assertion removed from `tests/executor_reentrant_shutdown.rs`: an
/// assertion on an order the language explicitly does not guarantee (`concurrency.md` §"Output
/// ordering"). What IS guaranteed is asserted below — the multiset of lines, and `empty` LAST, since
/// the drain runs after the nursery has joined so exactly three values are queued and the fourth
/// poll is `None`.
#[test]
fn golden_try_recv_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/try_recv.chz");
    let expected = include_str!("../../../examples/try_recv.expected");
    let vm_out = run_capture(src).expect("vm run");
    let stress_out = run_capture_stress(src);
    assert_same_lines(expected, &vm_out);
    assert_same_lines(&vm_out, &run_capture(src).expect("M:N run"));
    assert_same_lines(expected, &stress_out);
    for (label, out) in [
        ("run", &vm_out),
        ("expected", &expected.to_string()),
        ("stress", &stress_out),
    ] {
        assert_eq!(
            out.lines().last(),
            Some("empty"),
            "the 4th poll drains an already-empty channel, so `empty` must be last ({label}): {out}"
        );
    }
}

/// B1+B2 golden: blocking `recv`. The consumer is scheduled first, parks on the empty channel, the
/// scheduler runs the producer, and the consumer resumes to receive. The LINE SET is fixed by the
/// program's logic; the exact print order is a genuine M:N race between the two fibers' stdout
/// (unlike the single-threaded cooperative engine this golden was originally pinned against), so
/// compare order-insensitively.
#[test]
fn golden_channel_block_chz_matches_expected() {
    let src = include_str!("../../../examples/channel_block.chz");
    let expected = include_str!("../../../examples/channel_block.expected");
    let vm_out = run_capture(src).expect("vm run");
    crate::vm::assert_same_lines(&vm_out, expected);
    // Same result under GC stress — parked fibers survive collection.
    crate::vm::assert_same_lines(&run_capture_stress(src), expected);
}

/// Cross-nursery circular wakeup — the case-A flat-scheduler fix (M:N / `--parallel` ONLY).
/// `inner` spawns a NESTED nursery; the OUTER `parallel:` spawns sibling O and then calls
/// `inner(a, b)`. O recvs a → sends b; inner's spawned fiber sends a → recvs b. Pre-fix this
/// faulted `deadlock` (the inner nursery's inline owner drained only its private queue and could
/// never RUN the outer sibling O). With the M:N flat scheduler (one global `MnSched`, scope-scoped
/// owner stop) the inline owner's OS thread drains the GLOBAL queue, so it runs O, which unblocks I.
/// M:N-only (no coop/interp leg — the cooperative flatten is a separate, later commit; case A still
/// faults deadlock under plain `run`), mirroring `golden_channel_block`. Wrapped in a 30s
/// `recv_timeout` watchdog so a regression fails LOUD instead of hanging CI.
#[test]
fn golden_parallel_cross_nursery_circular_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_circular.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_circular.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — cross-nursery flat scheduler regressed (lost wakeup or owner stopped too early)"
        ),
    }
}

/// Cross-nursery flat scheduler — a MULTI-TASK inner nursery (regression guard for the early-enlist
/// vs deadlock-predicate race): with >1 inner task, helper workers exist, so the outer sibling `O`
/// MUST be enlisted before any worker can run an inner fiber to a park. M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_fanout_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_fanout.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_fanout.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — multi-task inner cross-nursery regressed (early-enlist/deadlock-predicate race)"
        ),
    }
}

/// Cross-nursery flat scheduler — an INLINE-BODY `send` (issued on the builder VM, `self.mn ==
/// None`, sched held only in `mn_enlist_sched`) must wake an enlisted, parked outer sibling. Before
/// the routing fix this false-faulted `deadlock` under `--parallel` (value queued, receiver never
/// made runnable) while coop prints "O got 42". M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_inline_send_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_inline_send.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_inline_send.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => {
            panic!("hung — inline-body send to an enlisted sibling regressed (lost wakeup)")
        }
    }
}

/// Cross-nursery flat scheduler — an INLINE-BODY `close` (and the `send` before it) issued on the
/// builder VM must wake an enlisted sibling RANGING over the channel (`for v in t:`): it receives the
/// value, then observes the close and ends. Same routing fix as inline-send, exercising the `close`
/// arm. M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_inline_close_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_inline_close.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_inline_close.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — inline-body close to an enlisted ranging sibling regressed (lost wakeup)"
        ),
    }
}

/// Cross-nursery flat scheduler — independent multi-level nesting (no shared channel) golden: four
/// levels deep (`main`→`top`→`mid`→`inner`), each level a `parallel:` with a sibling `spawn`. The old
/// "2+ enlisting levels" gate wrongly faulted this; it now RUNS. Order is deterministic by
/// data-dependency (each nested call joins — flushing its print — before that level's own sibling
/// runs), so this golden exact-matches AND equals the coop run. Regression guard for the gate removal.
/// M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_multilevel_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_multilevel.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_multilevel.expected");
    // Independent nesting is three-engine-stable here, so coop must produce the SAME bytes.
    assert_eq!(
        run_capture(src).expect("coop run"),
        expected,
        "coop diverged from the golden"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!("hung — independent multi-level nesting golden regressed"),
    }
}

/// Cross-nursery flat scheduler — independent multi-level nesting (no shared channel): three nested
/// `parallel:` blocks, each just printing, with sibling `spawn`s at each level. This is ordinary
/// nesting that does NOT contend a channel, so it must RUN and match the cooperative engine — it
/// must NOT fault (the old "2+ enlisting levels" gate wrongly rejected it). Data-dependent so the
/// inner level prints before the enclosing level finishes; order may still interleave under threads,
/// so we assert the line MULTISET equals coop. M:N-only, 30s watchdog.
#[test]
fn parallel_cross_nursery_independent_3level_runs_all() {
    let src = "fn inner():\n    parallel:\n        spawn:\n            print(\"inner\")\n        spawn:\n            print(\"inner2\")\nfn mid():\n    parallel:\n        spawn:\n            print(\"mid\")\n        inner()\n        spawn:\n            print(\"mid2\")\nfn top():\n    parallel:\n        spawn:\n            print(\"top\")\n        mid()\n        spawn:\n            print(\"top2\")\nfn main():\n    parallel:\n        spawn:\n            print(\"main\")\n        top()\n        spawn:\n            print(\"main2\")\n    print(\"done\")\nmain()\n";
    let coop = run_capture(src).expect("coop run");
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let par =
                r.expect("independent multi-level nesting must run under --parallel, not fault");
            let mut a: Vec<&str> = par.lines().collect();
            let mut b: Vec<&str> = coop.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                a, b,
                "parallel multiset != coop\nparallel:\n{par}\ncoop:\n{coop}"
            );
        }
        Err(_) => panic!("hung — independent multi-level nesting must complete, not hang"),
    }
}

/// Cross-nursery flat scheduler — a late `spawn:` into a NON-OUTERMOST (middle) nursery must RUN, not
/// be dropped and not panic `index out of bounds` at `join_enlisted_scope` (the old gate faulted it;
/// removing the gate alone clobbered the held sched). The middle nursery is early-enlisted, then
/// refilled by a `spawn:` after `inner()`; that late task runs at the join on the HELD sched as a
/// fresh trailing scope. Must match coop's line multiset (incl. "M2"). M:N-only, 30s watchdog.
#[test]
fn parallel_cross_nursery_late_spawn_into_middle_runs() {
    let src = "fn inner():\n    spawn:\n        print(\"inner\")\nfn mid():\n    parallel:\n        spawn:\n            print(\"M1\")\n        inner()\n        spawn:\n            print(\"M2\")\nfn main():\n    parallel:\n        spawn:\n            print(\"O1\")\n        mid()\n    print(\"done\")\nmain()\n";
    let coop = run_capture(src).expect("coop run");
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let par = r.expect("late spawn into middle nursery must run (no panic, no drop)");
            assert!(par.contains("M2"), "late task M2 dropped: {par}");
            let mut a: Vec<&str> = par.lines().collect();
            let mut b: Vec<&str> = coop.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                a, b,
                "parallel multiset != coop\nparallel:\n{par}\ncoop:\n{coop}"
            );
        }
        Err(_) => panic!("hung — late-spawn-into-middle-nursery must complete, not hang/panic"),
    }
}

/// Cross-nursery flat scheduler — the genuinely-CONTENDED case: 2+ live receivers racing ONE channel
/// across nested nurseries is a racy program. Under `--parallel` it may diverge in delivery order or
/// even deadlock; that is ALLOWED (suspendable concurrency is VM-only / divergent by design — see
/// PROGRESS.md). It must only NEVER PANIC and NEVER HANG: the outcome is EITHER Ok OR a clean
/// `deadlock` fault. M:N-only, 30s watchdog. (Replaces the old 2+-enlisting-levels gate assertion.)
#[test]
fn parallel_cross_nursery_contended_never_panics() {
    let src = "fn inner():\n    spawn:\n        print(\"i\")\nfn mid(cm: Channel[int]):\n    parallel:\n        spawn:\n            x := cm.recv()\n            print(\"M {x}\")\n        inner()\n        cm.send(2)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn:\n            y := c.recv()\n            print(\"O {y}\")\n        mid(c)\n        c.send(1)\nmain()\n";
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        // Delivery order is concurrent-divergent by design: accept Ok OR a clean deadlock fault.
        Ok(Ok(_)) => {}
        Ok(Err(e)) => assert!(
            e.message.contains("deadlock"),
            "contended case must succeed or deadlock-fault, got: {}",
            e.message
        ),
        Err(_) => panic!(
            "hung — contended cross-nursery must complete or deadlock-fault, never hang/panic"
        ),
    }
}

/// Cross-nursery flat scheduler — late-spawn-into-middle with a PARKED outer receiver. Guards the
/// `register_scope_seeded` atomicity: a non-atomic register→seed of the late trailing scope opened a
/// window where a SENTINEL helper could read (scope incomplete, awaiting_builder=false, runnable==0,
/// outer recv parked) as a false quiesce and spuriously fault the parked receiver. Single sender /
/// single receiver → no contention → deterministic, so VM (`--parallel`) MUST equal the plain
/// run. Run many times to shake the race. (Hardening for the auto-task panel's blocker.)
#[test]
fn parallel_cross_nursery_late_spawn_parked_matches_coop() {
    let src = include_str!("../../../examples/parallel_cross_nursery_late_spawn_parked.chz");
    let expected =
        include_str!("../../../examples/parallel_cross_nursery_late_spawn_parked.expected");
    let coop = run(src);
    assert_eq!(coop, expected, "coop must match expected");
    for _ in 0..12 {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = src.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture(&s));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(
                r.expect("parallel run (no spurious deadlock on the parked outer receiver)"),
                expected
            ),
            Err(_) => panic!("hung — late-spawn-into-middle with a parked receiver regressed"),
        }
    }
}

/// Cross-nursery flat scheduler — the `awaiting_builder` deadlock veto must be SURGICAL: a genuine
/// deadlock inside a NESTED nursery (`inner` spawns a no-sender `recv`) must STILL fault even while an
/// outer sibling is early-enlisted (`awaiting_builder`). The nested scope stays incomplete and is NOT
/// `awaiting_builder`, so `all_incomplete_awaiting_builder` is false → the predicate fires. Guards
/// against the veto hanging a real deadlock. M:N-only, 30s watchdog. (charges #1/#2 risk.)
#[test]
fn parallel_cross_nursery_genuine_nested_deadlock_still_faults() {
    let src = "fn inner(dead: Channel[int]):\n    spawn:\n        v := dead.recv()\n        print(\"never {v}\")\nfn main():\n    dead := Channel[int]()\n    parallel:\n        spawn:\n            print(\"O ran\")\n        inner(dead)\n    print(\"done\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("a genuine nested no-sender recv must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected deadlock, got: {}",
                err.message
            );
        }
        Err(_) => {
            panic!("hung — awaiting_builder veto wrongly suppressed a genuine nested deadlock")
        }
    }
}

/// gaps.md B5 — a `send` from inside a NESTED (child, eager) `parallel:` nursery must wake a receiver
/// parked on that channel in the OUTER nursery. Uncontended 1-send / 1-recv → deterministic, so serial
/// (the oracle) == M:N == the golden. Before the child→parent wake-routing fix (`MnSched::parent_wake`)
/// the DEFAULT M:N engine spuriously faulted `deadlock`: the value landed in the shared `ChannelCore`
/// but the eager child sched's `send_wake` only scanned its OWN park set, so the outer parked receiver
/// was never made runnable and the parent quiesced to a false deadlock — while serial printed "receiver
/// got 1". 10 rounds under a 30s watchdog so a lost-wakeup regression fails loud instead of hanging.
#[test]
fn golden_parallel_cross_nursery_nested_send_to_outer_recv_matches_expected() {
    let src =
        include_str!("../../../examples/parallel_cross_nursery_nested_send_to_outer_recv.chz");
    let expected =
        include_str!("../../../examples/parallel_cross_nursery_nested_send_to_outer_recv.expected");
    // Uncontended single-sender/single-receiver → the serial oracle must produce the SAME bytes.
    assert_eq!(
        run_capture(src).expect("coop run"),
        expected,
        "coop diverged from the golden"
    );
    for _ in 0..10 {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = src.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture(&s));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(
                r.expect("parallel run (no spurious cross-nursery deadlock)"),
                expected
            ),
            Err(_) => panic!("hung — cross-nursery child→parent wake regressed (lost wakeup)"),
        }
    }
}

/// gaps.md B5 detector-accuracy guard — the child→parent wake routing must NOT weaken the deadlock
/// detector. The B5 shape with NO send anywhere (the nested grandchild does other work, never sends)
/// is a GENUINE deadlock: the outer receiver parks forever. It must STILL fault `deadlock` on M:N
/// (`parent_wake` exists but is never triggered — no send). Proves the fix didn't turn the detector
/// off to silence the false positive. 30s watchdog.
#[test]
fn parallel_cross_nursery_nested_no_send_still_deadlocks() {
    let src = "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    print(\"grandchild ran\")\n        spawn:\n            v := ready.recv()\n            print(\"receiver got {v}\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err =
                r.expect_err("a genuine no-sender cross-nursery recv must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected deadlock, got: {}",
                err.message
            );
        }
        Err(_) => {
            panic!("hung — genuine cross-nursery deadlock no longer fires (detector weakened)")
        }
    }
}

/// gaps.md B5 — a nested (eager) nursery whose grandchild raises a REAL fault (index-out-of-bounds)
/// while an outer sibling parks on the channel must surface the REAL fault, NOT a spurious `deadlock`
/// (`reduce_task_slots` ranks Fault > Deadlocked). Guards that the wake-routing patch keeps the real
/// error winning. Serial oracle reports the same real fault. 30s watchdog.
#[test]
fn parallel_cross_nursery_nested_real_fault_reports_real_error() {
    let src = "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    xs := [1]\n                    ready.send(xs[9])\n        spawn:\n            v := ready.recv()\n            print(\"receiver got {v}\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("the nested grandchild's real fault must surface");
            assert!(
                err.message.contains("out of bounds"),
                "expected the real fault, got: {}",
                err.message
            );
            assert!(
                !err.message.contains("deadlock"),
                "real fault masked by a spurious deadlock: {}",
                err.message
            );
        }
        Err(_) => panic!("hung — nested real-fault path regressed"),
    }
}

/// TICKET-099 — the reverse direction (receiver parked INSIDE an eager body, sender in an ANCESTOR
/// nursery) is now routed run-wide too, same as child→parent. Before this fix the direction was
/// unrouted and whether this program completed or deadlock-faulted depended on scheduling order (a
/// bare accept-either-outcome test); now it must deterministically succeed. 30s watchdog.
#[test]
fn parallel_cross_nursery_parent_to_child_send_wakes_the_deeper_receiver() {
    let src = "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    v := ready.recv()\n                    print(\"got {v}\")\n        spawn:\n            ready.send(1)\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(out)) => assert!(out.contains("got 1"), "expected 'got 1' in output: {out}"),
        Ok(Err(e)) => panic!(
            "parent→child send must wake the deeper receiver, not deadlock-fault: {}",
            e.message
        ),
        Err(_) => {
            panic!("hung — parent→child send must wake the deeper receiver, never hang/panic")
        }
    }
}

/// Cross-nursery flat scheduler — a `spawn:` issued AFTER a nursery was early-enlisted (which drained
/// its task vec) must NOT be silently dropped: the late task runs at the join. Before the fix the
/// "already enlisted" join branch discarded the refilled vec. M:N-only, 30s watchdog. (charge #3.)
#[test]
fn golden_parallel_cross_nursery_late_spawn_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_late_spawn.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_late_spawn.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!("hung — late-spawn-into-enlisted-nursery regressed"),
    }
}

/// Cross-nursery flat scheduler — on an EARLY EXIT (`return`) past a join whose nursery holds a
/// late `spawn:`, the escape must reclaim that task and let the caller continue. Guards the
/// `drain_escaped_nursery` half of charge #3. M:N-only.
///
/// §2c1 — this used to assert the `"pending task(s) cancelled"` report line, which is deleted (and
/// under eager start the late task is a live fiber, not an unstarted `PendingCall`, so there was
/// nothing left to count). What it guards now is the part that was always the point: the escape
/// path does not LEAK or HANG on that task — `after` still prints, inside the 30 s watchdog.
#[test]
fn parallel_cross_nursery_late_spawn_escape_reclaims_the_late_task() {
    let src = "fn inner():\n    spawn:\n        print(\"inner ran\")\nfn run():\n    parallel:\n        spawn:\n            print(\"O1 ran\")\n        inner()\n        spawn:\n            print(\"O2 ran\")\n        return\nfn main():\n    run()\n    print(\"after\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let out = r.expect("parallel run");
            assert!(
                out.ends_with("after\n"),
                "post-parallel statement must run: {out}"
            );
        }
        Err(_) => panic!("hung — late-spawn escape path regressed"),
    }
}

/// Cross-nursery flat scheduler regression guard: a GENUINE no-sender deadlock must STILL fault
/// `deadlock` under `--parallel` (not hang, not succeed) — the global predicate must fire even
/// though park/wake/scopes are now VM-global. 30s watchdog so a regression fails loud.
#[test]
fn golden_parallel_deadlock_still_faults() {
    let src = include_str!("../../../examples/parallel_deadlock.chz");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("a genuine no-sender nursery must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected a deadlock fault, got: {}",
                err.message
            );
        }
        Err(_) => panic!(
            "hung — the global deadlock predicate failed to fire (cross-nursery flat scheduler regressed)"
        ),
    }
}

// ----- B1 + B2: cooperative fibers + blocking recv (VM engine) -----

/// Ping-pong across two channels exercises many suspend↔resume cycles: each fiber repeatedly
/// parks on an empty `recv`, the scheduler runs the sibling whose `send` wakes it, and the parked
/// fiber resumes mid-`while`-loop with its locals intact.
///
/// The program's own print order IS deterministic, not a race: every print is causally chained
/// through a channel handoff (`pong 0` → `a.send(100)` → `a.recv()` → `ping 100` → `b.send(1)` →
/// `pong 1` → …), and the real CLI's stdout sink is one locked write per `print` — line-atomic, no
/// per-thread buffering (`docs/reference/concurrency/`) — so the happens-before chain pins the
/// interleaving exactly there (verified against the release binary: 20/20 runs identical,
/// `pong 0\nping 100\n…`).
///
/// This Rust unit test, though, does NOT go through that sink — `run`/`run_capture` use the
/// in-process BUFFERED `Vm::out`, and under the M:N engine each spawned task's buffer is `mem::take`n
/// per-fiber (`Vm::swap_ctx`) and only spliced back into the parent at the join, in SPAWN-SLOT
/// order, not completion or causal order (`reduce_task_slots`, `src/vm/sched.rs:2118`). So what this
/// harness actually observes is each task's ENTIRE output as one block, concatenated ping-then-pong
/// (spawn order) — an artifact of the capture path, not of the program. Compare order-insensitively
/// for that reason; the load-bearing assertion is that every VALUE threaded correctly across the
/// suspend/resume cycles, which order-insensitive comparison still catches.
#[test]
fn fibers_ping_pong_interleaves() {
    let src = "fn ping(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        b.send(i)\n        x := a.recv() else: panic(\"a closed\")\n        print(\"ping {x}\")\n        i = i + 1\nfn pong(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        y := b.recv() else: panic(\"b closed\")\n        print(\"pong {y}\")\n        a.send(y + 100)\n        i = i + 1\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn ping(a, b)\n        spawn pong(a, b)\nmain()\n";
    let expected = "pong 0\nping 100\npong 1\nping 101\npong 2\nping 102\n";
    crate::vm::assert_same_lines(&run(src), expected);
    // Same result under GC stress — parked fibers' frames/locals are rooted while they wait.
    crate::vm::assert_same_lines(&run_capture_stress(src), expected);
}

/// All siblings parked on empty channels that no one will fill ⇒ a real deadlock (detected by the
/// scheduler when no fiber is runnable yet not all are done).
#[test]
fn fibers_all_blocked_is_deadlock() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
    assert!(run_err(src).contains("deadlock"), "expected deadlock");
}

/// Native-reentry guard, M:N: a blocking `recv` reached inside a list-HOF callback (the HOF's loop
/// state is on the Rust stack, so it cannot PARK in place) now completes via the M:N engine's
/// DEMOTION path instead — same C5/N6g mechanism as a `defer`red `recv` (`docs/contributing/gaps.md` N6g): a
/// replacement worker takes over scheduling while this thread blocks synchronously waiting for the
/// sibling's send. On the (removed) cooperative engine this had no replacement worker to demote to
/// and faulted `deadlock` instead — a documented, accepted engine-capability difference, not a bug.
#[test]
fn fibers_recv_inside_map_callback_demotes_on_mn() {
    let src = "fn use_map(ch: Channel[int]):\n    xs := [0]\n    ys := xs.map(fn(x): ch.recv())\n    print(ys)\nfn fill(ch: Channel[int]):\n    ch.send(1)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn use_map(ch)\n        spawn fill(ch)\nmain()\n";
    assert_eq!(run(src), "[1]\n");
}

/// Native-reentry guard, M:N: a blocking `recv` inside a struct `index`/`slice`/`set_index`
/// operator overload (run from the native indexing opcodes, host-stack state) now completes via
/// M:N's demotion path — same mechanism as `fibers_recv_inside_map_callback_demotes_on_mn`.
#[test]
fn fibers_recv_inside_index_overload_demotes_on_mn() {
    let src = "struct Src:\n    ch: Channel[int]\n    fn index(self, k: int) -> int:\n        return self.ch.recv()\nfn use_index(s: Src):\n    print(s[0])\nfn fill(ch: Channel[int]):\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    s := Src(ch)\n    parallel:\n        spawn use_index(s)\n        spawn fill(ch)\nmain()\n";
    assert_eq!(run(src), "7\n");
}

/// A blocking `recv` inside a `defer`red call (run during frame teardown) parks the fiber like any
/// other wait (plan 2026-11-07-defer-drain-frames): the run completes and no worker demotes. The
/// `recv` is in the deferred function's body — only the receiver handle is captured at the `defer`
/// statement — so it runs at teardown. It re-runs alone in a child test binary, so the process-wide
/// demote count is this test's own.
#[test]
fn fibers_recv_inside_defer_parks_on_mn() {
    #[rustfmt::skip]
    let in_parent = crate::vm::rerun_in_child(module_path!(), "fibers_recv_inside_defer_parks_on_mn");
    if in_parent {
        return;
    }
    let src = "fn consume(ch: Channel[int]):\n    ch.recv()\nfn worker(ch: Channel[int]):\n    defer consume(ch)\n    print(\"body\")\nfn sender(ch: Channel[int]):\n    ch.send(5)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\nmain()\n";
    let before = crate::vm::demote_count();
    assert_eq!(run(src), "body\n");
    assert_eq!(
        crate::vm::demote_count() - before,
        0,
        "the wait in the defer body must park, not demote"
    );
}

/// A nested `parallel:` inside a child fiber runs its own scheduler level (recursively); the
/// child resumes after its grandchildren join, and the outer sibling runs afterward.
#[test]
fn fibers_nested_parallel() {
    let src = "fn child():\n    parallel:\n        spawn:\n            print(\"grandchild\")\n    print(\"child after nested\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling\")\nmain()\n";
    assert_eq!(run(src), "grandchild\nchild after nested\nsibling\n");
}

/// D0 — the cooperative scheduler must run a large nursery in ~O(N·logN), not O(N²). 50k trivial
/// fibers pin this with a COUNT, not a clock: the scheduler must not scan (or requeue) the whole run
/// queue per pick. TICKET-164: the old wall-clock ceiling was vacuous for its own purpose (the
/// quadratic engine it names took ~2.3 s at 50k, under its 5 s ceiling, and that engine is gone) and
/// load-sensitive (measured 6.66-52.25 s under concurrent `cargo test` on this box, same test). The
/// fibers deliberately touch no `Shared` -- that guard's contention under load is a real, separate
/// finding, covered by its own ticket, not by this one.
#[test]
fn fibers_scale_ready_queue_not_quadratic() {
    let n = 50_000;
    let src = format!(
        "fn work(ch: Channel[int], k: int):\n    ch.send(k + 1)\n\
             fn main():\n    ch := Channel[int]()\n    parallel:\n        for k in 0..{n}:\n            spawn work(ch, k)\n    ch.close()\n    total := 0\n    for v in ch:\n        total += v\n    print(total)\nmain()\n"
    );
    let (out, picks) = run_capture_counting_picks(&src);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(
        out,
        format!("{}\n", n * (n + 1) / 2),
        "every fiber must run exactly once"
    );
    assert!(
        (n..=2 * n).contains(&picks),
        "scheduler picks per fiber out of bounds: {picks} picks for {n} fibers (want {n}..={})",
        2 * n
    );
}

/// TICKET-230 — a preempted fiber with nothing else runnable keeps its worker. Queueing it in
/// `global` let an idle worker's free-permit look (or its 5 ms timed wake) take it in the
/// push-to-re-pop window and run it on a CPU that was idle: `primes_parallel` T=0 measured per-slice
/// time 1.24x and wall 1.36x, with no spinning and no ready-queue wait. Pinned with a COUNT: every
/// slice end that goes back through `global` re-enters `take_runnable` and adds one pick (about 200
/// slices here).
#[test]
fn a_preempted_fiber_with_nothing_else_runnable_keeps_its_worker() {
    let src = [
        "fn burn():",
        "    i := 0",
        "    while i < 200000:",
        "        i += 1",
        "fn main():",
        "    parallel:",
        "        spawn burn()",
        "    print(\"done\")",
        "main()",
    ]
    .join("\n");
    let (out, picks) = run_capture_counting_picks(&src);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "done\n");
    assert!(
        picks <= 20,
        "a fiber alone in its sched must keep its worker across slices: {picks} picks"
    );
}

/// TICKET-230 — a preempted permit holder hands over once per `width::HANDOVER_QUANTUM`, not per
/// slice. With more runner threads than permits, a hand-over per slice makes every thread run part
/// of the time on its own core, and `schedutil` clocks those cores down: two Executors at
/// `--threads=2` used 16.9 s of CPU for 7.2 s of work. Pinned with a COUNT of preemption
/// hand-overs (measured on the debug CLI: about 2000 per-slice, 16-19 with the quantum).
#[test]
fn oversubscribed_runners_keep_their_permit_across_slices() {
    #[rustfmt::skip]
    let in_parent = crate::vm::rerun_in_child(module_path!(), "oversubscribed_runners_keep_their_permit_across_slices");
    if in_parent {
        return;
    }
    let _lock = crate::vm::TEST_WORKER_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::vm::set_worker_count(2);
    let src = "
import Executor from std.concurrency
fn burn():
    i := 0
    while i < 200000:
        i += 1
a := Executor()
b := Executor()
for _ in 0..4:
    a.submit(fn(): burn())
    b.submit(fn(): burn())
a.shutdown()
b.shutdown()
print(\"done\")
";
    let before = super::width::PREEMPT_HANDOVERS.load(std::sync::atomic::Ordering::Relaxed);
    let out = run_capture(src);
    let handovers =
        super::width::PREEMPT_HANDOVERS.load(std::sync::atomic::Ordering::Relaxed) - before;
    crate::vm::set_worker_count(crate::vm::test_baseline_worker_count());
    assert_eq!(
        out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}")),
        "done\n"
    );
    assert!(
        handovers <= 500,
        "four runner threads on two permits must hand over per quantum, not per slice: {handovers} preemption hand-overs"
    );
}

/// D0 — the `blocked_on` wake path: one consumer parks on a shared channel and is re-woken by each
/// of many producers' `send`s. Sibling fibers hold DISTINCT `GcRef`s aliasing the same
/// `Arc<ChannelCore>` (cooperative `spawn` deep-clones the channel), so the wake map must key on
/// the core pointer, not the handle — a `GcRef` key would lose every wakeup and fault `deadlock`.
#[test]
fn fibers_many_producers_one_consumer() {
    let n = 200;
    let src = format!(
        "import Shared from std.concurrency\nfn produce(ch: Channel[int]):\n    ch.send(1)\n\
             fn consume(ch: Channel[int], k: int, s: Shared[int]):\n    total := 0\n    for _ in 0..k:\n        v := ch.recv() else: panic(\"ch closed\")\n        total += v\n    s.set(total)\n\
             fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consume(ch, {n}, s)\n        for _ in 0..{n}:\n            spawn produce(ch)\n    print(s.get())\nmain()\n"
    );
    assert_eq!(
        run(&src),
        format!("{n}\n"),
        "consumer must receive every produced value"
    );
}

/// D0 — cross-level wakeup: a fiber nested in an INNER `parallel:` `send`s to a channel an
/// OUTER-level sibling is parked on. The `send` arm must drain the blocked set of EVERY scheduler
/// level (not just the innermost), or the outer consumer never wakes and the nursery faults
/// `deadlock` after the inner level joins. (The old `pick_runnable` re-scanned all levels each
/// turn, so this worked; the ready-set must preserve it.)
#[test]
fn fibers_cross_level_wakeup() {
    let src = "import Shared from std.concurrency\nfn consumer(ch: Channel[int], s: Shared[int]):\n    s.set(ch.recv())\n\
                   fn inner_sender(ch: Channel[int]):\n    ch.send(42)\n\
                   fn middle(ch: Channel[int]):\n    parallel:\n        spawn inner_sender(ch)\n\
                   fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consumer(ch, s)\n        spawn middle(ch)\n    print(s.get())\nmain()\n";
    assert_eq!(
        run(src),
        "42\n",
        "outer consumer must wake from an inner-level send"
    );
}

/// A `recover:` inside a child fiber catches a fault in that fiber's own context (its handlers /
/// frames are per-fiber); the sibling is unaffected and runs normally.
#[test]
fn fibers_recover_inside_child_is_isolated() {
    let src = "fn boom():\n    xs := [1]\n    print(xs[9])\nfn child():\n    r := recover:\n        boom()\n        0\n    print(\"caught\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling ok\")\nmain()\n";
    assert_eq!(run(src), "caught\nsibling ok\n");
}

/// C3 golden: `Shared[T]` cross-task box — three tasks bump one serialised counter. Byte-identical
/// on the VM, the interpreter, and the `.expected` file.
#[test]
fn golden_shared_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/shared.chz");
    let expected = include_str!("../../../examples/shared.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// C5 golden: the `Executor` escape hatch — submit/shutdown (FIFO drain), `defer ex.shutdown()`,
/// cancel (discard). Byte-identical on the VM, the interpreter, and the `.expected` file.
#[test]
fn golden_executor_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/executor.chz");
    let expected = include_str!("../../../examples/executor.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

// Micro-tests mirroring the interpreter's C2/C3 unit tests (src/interp/mod.rs), to pin the VM's
// channel/shared/spawn semantics directly (not just via the example goldens).

#[test]
fn channel_send_recv_fifo() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "1\n2\n");
}

#[test]
fn channel_send_moves_the_handle() {
    // M3.2: a send moves the list itself, so a push after the send is what the receiver gets. Go
    // (`rewrites.go`, M3.2 report): a map sent, then written, is received with the write (`3`).
    let src = "fn main():\n    ch := Channel[List[int]]()\n    xs := [1, 2]\n    ch.send(xs)\n    xs.push(3)\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "[1, 2, 3]\n");
}

#[test]
fn channel_recv_on_empty_is_deadlock_error() {
    let err = run_err("fn main():\n    ch := Channel[int]()\n    print(ch.recv())\nmain()\n");
    assert!(err.contains("deadlock"), "got: {err}");
}

/// A1: `try_recv` on an empty channel returns `None` (never the `recv` deadlock fault).
#[test]
fn channel_try_recv_on_empty_returns_none() {
    let src = "fn main():\n    ch := Channel[int]()\n    match ch.try_recv():\n        ?v: print(\"got {v}\")\n        None: print(\"empty\")\nmain()\n";
    assert_eq!(run(src), "empty\n");
}

/// A1: `try_recv` on a non-empty channel returns `Some(v)` (FIFO).
#[test]
fn channel_try_recv_with_value_returns_some() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    match ch.try_recv():\n        ?v: print(v)\n        None: print(\"empty\")\nmain()\n";
    assert_eq!(run(src), "42\n");
}

/// A1 × B1/B2 (VM-only): `try_recv` must drain the residue left after a *blocking* `recv` resumed.
/// The consumer parks on an empty `recv`; the producer sends two values; the consumer resumes,
/// `recv`s the first, then polls the rest with `try_recv` (the second value, then `None`). Pins
/// that the resume path leaves `suspend`/`ip` clean so the following non-blocking polls behave.
/// TICKET-164: the producer signals `ready` only after both sends, and the consumer polls only
/// after `ready.recv()`, so `try 2` is forced by happens-before. Without the handshake `try empty`
/// is a legal race outcome (Go agrees, 1 in 20000); never relax this to an order-insensitive check
/// -- that stops testing the residue drain.
#[test]
fn try_recv_drains_residue_after_blocking_recv_resumes() {
    let src = "fn producer(ch: Channel[int], ready: Channel[int]):\n    ch.send(1)\n    ch.send(2)\n    ready.send(0)\nfn consumer(ch: Channel[int], ready: Channel[int]):\n    a := ch.recv()\n    print(\"recv {a}\")\n    ready.recv()\n    match ch.try_recv():\n        ?v: print(\"try {v}\")\n        None: print(\"try empty\")\n    match ch.try_recv():\n        ?v: print(\"try {v}\")\n        None: print(\"try empty\")\nfn main():\n    ch := Channel[int]()\n    ready := Channel[int]()\n    parallel:\n        spawn consumer(ch, ready)\n        spawn producer(ch, ready)\nmain()\n";
    let expected = "recv 1\ntry 2\ntry empty\n";
    assert_eq!(run(src), expected);
    assert_eq!(run_capture_stress(src), expected);
}

/// A1 regression guard: `try_recv` on an empty channel INSIDE an active `parallel:` scheduler must
/// return `None` — it must NOT route through the `recv` park path (which would suspend the lone
/// child and then deadlock, since no sibling can ever send). Pins try_recv as truly non-blocking.
#[test]
fn channel_try_recv_in_parallel_does_not_suspend() {
    let src = "fn probe(ch: Channel[int]):\n    match ch.try_recv():\n        ?v: print(v)\n        None: print(\"empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn probe(ch)\nmain()\n";
    assert_eq!(run(src), "empty\n");
    assert_eq!(run_capture_stress(src), "empty\n");
}

#[test]
fn shared_get_set_round_trip() {
    let src = "import Shared from std.concurrency\nfn main():\n    s := Shared(1)\n    print(s.get())\n    s.set(42)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "1\n42\n");
}

#[test]
fn shared_update_read_modify_write() {
    let src = "import Shared from std.concurrency\nfn main():\n    s := Shared(10)\n    s.update(fn(x): x * 2)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "21\n");
}

#[test]
fn shared_get_returns_the_live_value() {
    // M3.3: `get` returns the value the box holds (Go `handle/main.go`: `shared_get_write 2`).
    let src = "import Shared from std.concurrency\nfn main():\n    s := Shared([1, 2])\n    xs := s.get()\n    xs.push(3)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "[1, 2, 3]\n");
}

#[test]
fn vm_rwshared_get_set_read_write_roundtrip() {
    // `get`/`set` (write-lock replace) + `read` (R-poly snapshot) + `write` (write-locked RMW).
    let src = "import RwShared from std.concurrency\nfn main():\n    r := RwShared(10)\n    print(r.get())\n    r.set(20)\n    print(r.read(fn(x): x + 1))\n    r.write(fn(x): x * 2)\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "10\n21\n40\n");
    // Byte-identical on the interpreter (the frozen oracle).
}

#[test]
fn vm_rwshared_read_returns_closure_result_no_writeback() {
    // `read` returns the closure's value and does NOT write back (the box is unchanged).
    let src = "import RwShared from std.concurrency\nfn main():\n    r := RwShared(5)\n    print(r.read(fn(x): \"{x * 3}\"))\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "15\n5\n");
}

#[test]
fn vm_rwshared_get_returns_the_live_value() {
    // M3.3: `get` returns the value the box holds (Go `handle/main.go`: `rwshared_get_write 2`).
    let src = "import RwShared from std.concurrency\nfn main():\n    r := RwShared([1, 2])\n    xs := r.get()\n    xs.push(3)\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "[1, 2, 3]\n");
}

#[test]
fn executor_submit_runs_fifo_at_shutdown() {
    let src = "import Executor from std.concurrency\nfn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\n    ex.shutdown()\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    // M:N drains the two Executor tasks on the pool, so `1`/`2` can print in either order (`0` is
    // the parent's, always first). Same multiset — cooperative FIFO order pinned above.
    assert_same_lines(&run(src), &run_capture(src).expect("M:N run"));
}

#[test]
fn executor_submit_after_shutdown_settles_the_task_with_the_refusal() {
    // TICKET-289: the refused submit returns an already-settled Task; the run does not fault.
    let src = "import Executor from std.concurrency\nfn main():\n    ex := Executor()\n    ex.shutdown()\n    t := ex.submit(fn(): print(1))\n    match t.get():\n        ?_: print(\"ran\")\n        !e: print(e.message())\nmain()\n";
    let out = run_capture(src).expect("a refused submit must not fault the run");
    assert_eq!(
        out,
        "submit on a shut-down Executor (it no longer accepts work)\n"
    );
}
