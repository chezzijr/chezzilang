//! VM tests: panic, C4 concurrency, channel close, `wait` and its edge cases.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ---- user-callable panic(msg) builtin ----

/// `panic(msg)` beneath `recover:` materializes as `Err(e)` whose `.message()` == msg on the VM
/// too — byte-identical against a literal golden.
#[test]
fn vm_panic_under_recover_yields_err_with_message() {
    let src = "fn main():\n    r: int! = recover:\n        panic(\"boom\")\n    match r:\n        ?v: print(\"ok: {v}\")\n        !e: print(\"recovered: {e.message()}\")\nmain()\n";
    assert_eq!(run(src), "recovered: boom\n");
}

/// An uncaught `panic(msg)` aborts with that message (run_capture returns Err), same as overflow.
#[test]
fn vm_panic_uncaught_returns_runtime_error() {
    let src = "fn main():\n    panic(\"kaboom\")\nmain()\n";
    let err = run_capture(src).expect_err("uncaught panic should fault");
    assert_eq!(err.message, "kaboom");
}

/// `defer`s run during the VM panic unwind, identically to the interp.
#[test]
fn vm_panic_runs_defers_during_unwind() {
    let src = "fn log(m: str):\n    print(m)\nfn risky():\n    defer log(\"cleanup ran\")\n    panic(\"kaboom\")\nfn main():\n    r := recover:\n        risky()\n    match r:\n        ?v: print(\"ok\")\n        !e: print(\"recovered: {e.message()}\")\nmain()\n";
    assert_eq!(run(src), "cleanup ran\nrecovered: kaboom\n");
}

/// `panic` golden: a `recover:` catching a bare `panic(msg)` as `Err`, a `defer` running during
/// the panic unwind, and the bottom-typed `panic` in if-expression value position. Byte-identical
/// on the VM against its `.expected`.
#[test]
fn golden_panic_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/panic.chz");
    let expected = include_str!("../../../examples/panic.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `defer` golden: LIFO order, method + free-fn calls, the `?` short-circuit path, args evaluated
/// at the defer statement (per-iteration snapshot), the `defer:` block form (in-block order,
/// LIFO-as-a-unit, by-value snapshot at the defer point, `?`-path), defers running before a
/// `recover:` catch, and a fault inside a deferred call. Byte-identical on the VM against
/// its `.expected`.
#[test]
fn golden_defer_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/defer.chz");
    let expected = include_str!("../../../examples/defer.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

// ----- concurrency C4 (spawn / parallel: / Channel / Shared) -----

/// C1 golden: `parallel:` nursery + both `spawn` forms run to completion at the dedent (FIFO),
/// the parent resuming only after the join. Byte-identical on the VM against the
/// `.expected` file.
#[test]
fn golden_parallel_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/parallel.chz");
    let expected = include_str!("../../../examples/parallel.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// B3.3-threads sub-step 1: the `--parallel` engine is selectable (`run_capture` sets
/// `Vm::parallel`). A `parallel:` with task-order output (no cross-task blocking) yields the
/// expected result — proving the flag plumbs through without changing
/// well-ordered output.
#[test]
fn parallel_engine_runs_simple_program() {
    let src = include_str!("../../../examples/parallel.chz");
    let expected = include_str!("../../../examples/parallel.expected");
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

/// M-C golden: implicit nurseries — bare `spawn` at function scope joins at the body's
/// `return`/end; an inner `parallel:` joins earlier at its dedent. Byte-identical against the golden.
#[test]
fn golden_implicit_nursery_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/implicit_nursery.chz");
    let expected = include_str!("../../../examples/implicit_nursery.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

/// B3.3-threads golden: N real OS-thread tasks `update` one `Shared[int]` concurrently; the box
/// serialises every write, so the count is exactly the spawn count (lost-update race fixed by the
/// `update_lock`). Deterministic-by-construction (order-independent) — proves the bounded pool +
/// `Shared` cross-thread atomicity.
#[test]
fn golden_parallel_shared_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_shared.chz");
    let expected = include_str!("../../../examples/parallel_shared.expected");
    assert_eq!(run_capture(src).expect("parallel run"), expected);
    // Same program without the flag is identical (decision A oracle).
    assert_eq!(run_capture(src).expect("vm run"), expected);
}

/// `RwShared[T]` golden: N tasks each `write` a distinct key into one shared `RwShared[map]`
/// (exclusive write lock; the whole RMW is serialised under `--parallel` so no update is lost),
/// the nursery joins, then the parent `read`s the whole map back. Order-independent → identical
/// with and without the M:N `--parallel` flag.
#[test]
fn golden_rwshared_concurrent_matches_expected() {
    let src = include_str!("../../../examples/rwshared.chz");
    let expected = include_str!("../../../examples/rwshared.expected");
    assert_eq!(run_capture(src).expect("vm run"), expected);
    assert_eq!(run_capture(src).expect("parallel run"), expected);
    assert_eq!(run_capture(src).expect("repeat run"), expected);
}

/// Airlock identity: a `RwShared` mutated from a spawned task is observed by the parent — the
/// handle crosses as a SHARED `Arc` core (not a deep-copy), so both reach the one box.
#[test]
fn rwshared_mutation_from_spawn_is_observed_by_parent() {
    let src = "import RwShared from std.concurrency\nfn bump(r: RwShared[int]):\n    r.write(fn(x): x + 1)\nfn main():\n    r := RwShared(0)\n    parallel:\n        spawn bump(r)\n    print(r.get())\nmain()\n";
    assert_eq!(run_capture(src).expect("vm run"), "1\n");
    assert_eq!(run_capture(src).expect("parallel run"), "1\n");
    assert_eq!(run_capture(src).expect("interp"), "1\n");
}

/// B3.3-threads golden: the collector task `recv`s before any producer runs, so on the real-thread
/// engine it BLOCKS on the channel condvar and is woken by producer `send`s from pool threads.
/// It sorts what it gathers → the printed order is fixed however threads interleave
/// (deterministic-by-construction). Exercises condvar `recv` + flush-on-join.
#[test]
fn golden_parallel_channel_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_channel.chz");
    let expected = include_str!("../../../examples/parallel_channel.expected");
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

/// The "correct pattern" companion to the cross-nursery deadlock: two mutually-dependent
/// blocking tasks as SIBLINGS in one nursery interleave fine (nesting them would deadlock — see
/// `docs/history/designs/cross-nursery-flat-scheduler.md`). VM-only blocking, so checked on the VM + `--parallel`
/// engines (the frozen interp cannot suspend a cross-fiber `recv`).
#[test]
fn golden_parallel_cross_nursery_ok_chz_matches_expected() {
    let src = include_str!("../../../examples/parallel_cross_nursery_ok.chz");
    let expected = include_str!("../../../examples/parallel_cross_nursery_ok.expected");
    assert_eq!(run_capture(src).expect("vm run"), expected);
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

// ---- Channel.close() + closed semantics ----

#[test]
fn vm_channel_send_after_close_faults() {
    let err =
        run_err("fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.send(1)\nmain()\n");
    assert!(err.contains("send on a closed channel"), "{err}");
}

// ----- §6d: `wait` (select) — VM-only blocking -----

#[test]
fn vm_wait_picks_first_ready_arm_in_source_order() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.send(1)\n    b.send(2)\n    wait:\n        v := a.recv(): print(10 + v)\n        w := b.recv(): print(20 + w)\nmain()\n";
    assert_eq!(run(src), "11\n");
}

#[test]
fn vm_wait_skips_closed_empty_arm() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.close()\n    b.send(9)\n    wait:\n        v := a.recv(): print(100)\n        w := b.recv(): print(w)\nmain()\n";
    assert_eq!(run(src), "9\n");
}

#[test]
fn vm_wait_runs_else_when_nothing_ready() {
    let src = "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\n        else: print(0)\nmain()\n";
    assert_eq!(run(src), "0\n");
}

#[test]
fn vm_wait_assign_arm_mutates_outer_lvalue() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(5)\n    n := 0\n    wait:\n        n = ch.recv(): print(n)\n    print(n)\nmain()\n";
    assert_eq!(run(src), "5\n5\n");
}

#[test]
fn vm_wait_timer_arm_fires() {
    let src = "import timer from std.time\nfn main():\n    t := timer(1)\n    wait:\n        _ := t.recv(): print(\"tick\")\nmain()\n";
    assert_eq!(run(src), "tick\n");
}

#[test]
fn vm_wait_all_closed_no_else_faults() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n",
    );
    assert!(err.contains("all channels closed"), "{err}");
}

/// W7-2 contract fence — the observable an ALL-DEAD `park_wait` requeue lands on is this fault, and
/// it must be byte-identical for a MULTI-arm wait (the single-arm case above only
/// exercises the poll, never the park-gap re-check).
#[test]
fn vm_wait_all_closed_multi_arm_faults_both_engines() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.close()\n    b.close()\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nmain()\n";
    let serial = run_err(src);
    assert!(serial.contains("all channels closed"), "{serial}");
    let mn = match run_capture(src) {
        Ok(o) => panic!("expected a fault, got {o:?}"),
        Err(e) => e.message,
    };
    assert_eq!(serial, mn, "serial vs M:N wait all-closed fault divergence");
}

#[test]
fn vm_wait_live_empty_no_else_top_level_deadlocks() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n",
    );
    assert!(err.contains("deadlock"), "{err}");
}

/// W7-2 anti-detector-weakening fence: a GENUINE all-parked nursery (a `wait:` on live, empty
/// channels with no possible waker) must still be reported promptly as a deadlock on M:N. The W7-2
/// fix must deliver the missing wakeup, never blunt the detector.
#[test]
fn vm_wait_real_deadlock_still_reported_parallel() {
    let src = "fn w(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        u := b.recv(): print(u)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn w(a, b)\nmain()\n";
    let mn = match run_capture(src) {
        Ok(o) => panic!("expected a deadlock fault, got {o:?}"),
        Err(e) => e.message,
    };
    assert!(mn.contains("deadlock"), "{mn}");
}

/// VM-only (the interp would deadlock): a spawned consumer `wait`s on two empty channels and
/// blocks (cooperative multi-channel park); a sibling `send` to the SECOND channel wakes it, and
/// the re-poll takes that arm. Exercises the multi-key park + wake on a non-first channel.
#[test]
fn vm_wait_blocks_then_wakes_on_second_channel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"a {v}\")\n        w := b.recv(): print(\"b {w}\")\nfn producer(b: Channel[int]):\n    b.send(99)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(b)\nmain()\n";
    assert_eq!(run(src), "b 99\n");
}

/// The multi-channel park SWEEP: a consumer parks on {a, b}, a `send` to `a` wakes it and it
/// finishes — then a later `send` to `b` must NOT re-wake the now-done fiber (its stale `b`
/// registration was swept on resume). Without the sweep this re-schedules a `Done` fiber → panic.
/// `run` is `run_capture`, so this races the same three producers as the parallel twin below;
/// only membership in `{"1\n", "2\n"}` is guaranteed, not a specific arm.
#[test]
fn vm_wait_sweeps_other_buckets_after_waking() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn p_a(a: Channel[int]):\n    a.send(1)\nfn p_b(b: Channel[int]):\n    b.send(2)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn p_a(a)\n        spawn p_b(b)\nmain()\n";
    let last = run(src);
    assert!(
        last == "1\n" || last == "2\n",
        "unexpected sweep output: {last:?}"
    );
}

/// A `wait` `=` arm to a Field/Index lvalue (the custom `emit_assign_value_first` stash-and-reload path):
/// the received value must land in the struct field / list slot, identical to the interp.
#[test]
fn vm_wait_assign_to_field_and_index_matches_interp() {
    let src = "struct Box:\n    v: int\nfn main():\n    ch := Channel[int]()\n    ch.send(7)\n    b := Box(0)\n    wait:\n        b.v = ch.recv(): print(b.v)\n    xs := [0, 0]\n    ch.send(9)\n    wait:\n        xs[1] = ch.recv(): print(xs[1])\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "7\n9\n");
}

/// A single-arm `wait` reduces to a plain `recv`: ready value taken (the recv-park 1-key special
/// case stays correct — plain `recv` is covered by its own tests, unchanged).
#[test]
fn vm_wait_single_arm_takes_ready_value() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    wait:\n        v := ch.recv(): print(v)\nmain()\n";
    assert_eq!(run(src), "42\n");
}

/// A lone blocking `wait` under `--parallel` with NO possible sender is a genuine deadlock — it
/// must fault `deadlock` (NOT hang, NOT "not yet supported"). Proves the M:N wait-park accounts
/// the parked fiber via `parked_n` so the deadlock predicate fires.
#[test]
fn vm_wait_lone_blocked_parallel_deadlocks() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\nmain()\n";
    let err = run_capture(src).expect_err("lone blocked wait under --parallel must deadlock");
    assert!(err.message.contains("deadlock"), "{}", err.message);
}

/// M:N blocking wait-park (TDD step 4): a spawned consumer `wait`s on two empty channels and parks
/// under `--parallel`; a sibling `send` to the SECOND channel wakes it and the re-poll takes that
/// arm. Asserts the parallel output AND the plain (no-scheduler) output.
#[test]
fn vm_wait_blocks_then_wakes_on_second_channel_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"a {v}\")\n        w := b.recv(): print(\"b {w}\")\nfn producer(b: Channel[int]):\n    b.send(99)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(b)\nmain()\n";
    assert_eq!(run_capture(src).expect("parallel wait park"), "b 99\n");
    assert_eq!(run(src), "b 99\n");
}

/// M:N wait-park cross-bucket SWEEP (TDD step 5): a consumer parks on {a,b}; a `send` to one wakes
/// it and it finishes — a later `send` to the OTHER must NOT re-wake the now-Done fiber (its stale
/// token was swept under the sched lock). Without the sweep this re-schedules a Done fiber → panic.
/// Two real-thread producers race, so EITHER arm may win — the sweep's guarantee is structural (no
/// panic, no hang), NOT a specific value; assert the run completes cleanly with a valid value, and
/// loop to exercise both orderings + the post-done send. `run` is `run_capture` too, so it races
/// exactly like the loop above it; only membership is asserted, not a specific arm.
#[test]
fn vm_wait_sweeps_other_buckets_after_waking_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn p_a(a: Channel[int]):\n    a.send(1)\nfn p_b(b: Channel[int]):\n    b.send(2)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn p_a(a)\n        spawn p_b(b)\nmain()\n";
    for _ in 0..20 {
        // `.expect` catches a panic-to-fault ("scheduled a Done fiber") or a hang-then-deadlock; the
        // membership check tolerates the genuine producer race.
        let out = run_capture(src).expect("parallel wait sweep must not panic or hang");
        assert!(
            out == "1\n" || out == "2\n",
            "unexpected sweep output: {out:?}"
        );
    }
    let last = run(src);
    assert!(
        last == "1\n" || last == "2\n",
        "unexpected sweep output: {last:?}"
    );
}

/// M:N wait-park: a wait-parked fiber with a LIVE sibling that will send must take the arm and
/// print — NOT fault deadlock (the live sibling vetoes the predicate).
#[test]
fn vm_wait_sibling_send_vetoes_deadlock_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"got {v}\")\n        w := b.recv(): print(\"got {w}\")\nfn producer(a: Channel[int]):\n    a.send(5)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(a)\nmain()\n";
    assert_eq!(
        run_capture(src).expect("live sibling vetoes deadlock"),
        "got 5\n"
    );
    assert_eq!(run(src), "got 5\n");
}

/// WAIT-1 (HIGH) regression — a `wait` over a long timer arm + a data channel under `--parallel`
/// must NOT pin the worker on the inline-sleep and unconditionally take the timer. A sibling
/// `send` landing mid-window (after a tiny `sleep_ms`) must win the channel arm. Pre-fix the
/// 2000ms inline-sleep strands the send and the timer arm fires (prints "timeout"). Looped to
/// catch any straggler ordering. `--parallel`-only (timer+wait is observably nondeterministic, so
/// no golden).
#[test]
fn vm_wait_timer_loses_to_midwindow_send_parallel() {
    let src = "import timer from std.time\nfn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn producer(ch: Channel[int]):\n    d := timer(5)\n    _ := d.recv()\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    t := timer(2000)\n    parallel:\n        spawn consumer(ch, t)\n        spawn producer(ch)\nmain()\n";
    for _ in 0..20 {
        assert_eq!(
            run_capture(src).expect("mid-window send must win the channel arm"),
            "got 7\n",
            "timer arm took the wait instead of the mid-window send (WAIT-1)"
        );
    }
}

/// Companion to the WAIT-1 fix: a short timer with NO sender must still fire its arm and the run
/// must COMPLETE (no hang). Proves the timed-park files the timer channel in the park set and the
/// deadline `send_wake` claims the fiber. A fix that forgot the timer arm in the park set would
/// hang; the `inflight` veto keeps the deadlock predicate quiet while the lone fiber waits.
#[test]
fn vm_wait_short_timer_fires_when_no_send_parallel() {
    let src = "import timer from std.time\nfn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn main():\n    ch := Channel[int]()\n    t := timer(5)\n    parallel:\n        spawn consumer(ch, t)\nmain()\n";
    assert_eq!(
        run_capture(src).expect("short timer must fire its arm, not hang"),
        "timeout\n"
    );
}

/// Re-park with a live timer arm: a sibling `close`s the channel arm mid-window, which wakes the
/// waiting fiber WITHOUT a consumable value, so it re-runs WaitPoll and re-enters the snapshot-park
/// block while the timer is still pending. The timer must still fire correctly ("timeout") and the
/// run must COMPLETE — no hang. This path (a timer-armed wait that re-parks) was previously
/// untested; it also guards the arm-once `timer_armed` CAS latch against ever wedging the wake.
#[test]
fn vm_wait_timer_arm_survives_channel_close_repark_parallel() {
    let src = "import timer from std.time\nfn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn closer(ch: Channel[int]):\n    d := timer(5)\n    _ := d.recv()\n    ch.close()\nfn main():\n    ch := Channel[int]()\n    t := timer(40)\n    parallel:\n        spawn consumer(ch, t)\n        spawn closer(ch)\nmain()\n";
    for _ in 0..20 {
        assert_eq!(
            run_capture(src).expect("timer arm must fire after a mid-window close; no hang"),
            "timeout\n",
            "a channel close that re-parks the wait stranded the timer arm"
        );
    }
}

// ----- §6d edge cases: control flow + nesting + spawn inside `wait` arm bodies (VM == interp) -----

/// A `wait` arm body that `break`s out of an enclosing loop (compiled via `compile_defer_scoped_arm`,
/// the same path as a `match` arm). byte-identical against the golden.
#[test]
fn vm_wait_arm_break_in_loop() {
    let src = "fn main():\n    found := -1\n    i := 0\n    while i < 3:\n        a := Channel[int]()\n        a.send(i * 10)\n        wait:\n            v := a.recv():\n                if v == 10:\n                    found = v\n                    break\n        i += 1\n    print(found)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "10\n");
}

/// A `wait` arm body that `continue`s the enclosing loop.
#[test]
fn vm_wait_arm_continue_in_loop() {
    let src = "fn main():\n    total := 0\n    i := 0\n    while i < 3:\n        a := Channel[int]()\n        a.send(i)\n        i += 1\n        wait:\n            v := a.recv():\n                if v == 1:\n                    continue\n                total += v\n    print(total)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "2\n");
}

/// A `wait` arm body that `return`s a value from the enclosing function.
#[test]
fn vm_wait_arm_return() {
    let src = "fn pick(a: Channel[int]) -> int:\n    wait:\n        v := a.recv():\n            return v * 100\nfn main():\n    a := Channel[int]()\n    a.send(7)\n    print(pick(a))\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "700\n");
}

/// A `wait` arm body containing ANOTHER `wait` (nested select).
#[test]
fn vm_wait_nested() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.send(1)\n    b.send(2)\n    wait:\n        v := a.recv():\n            wait:\n                w := b.recv(): print(v + w)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "3\n");
}

/// A `wait` inside a loop that blocks, wakes, re-iterates, and blocks again under `--parallel`: the
/// consumer parks on each iteration until the producer feeds it. VM == interp == --parallel.
#[test]
fn vm_wait_in_loop_reparks_parallel() {
    let src = "fn consumer(ch: Channel[int], done: Channel[int]):\n    sum := 0\n    i := 0\n    while i < 3:\n        wait:\n            v := ch.recv(): sum += v\n        i += 1\n    done.send(sum)\nfn producer(ch: Channel[int]):\n    ch.send(10)\n    ch.send(20)\n    ch.send(30)\nfn main():\n    ch := Channel[int]()\n    done := Channel[int]()\n    parallel:\n        spawn consumer(ch, done)\n        spawn producer(ch)\n    print(done.recv())\nmain()\n";
    assert_eq!(run(src), "60\n");
    assert_eq!(run_capture(src).expect("parallel repark loop"), "60\n");
}

/// A `wait` arm body containing a bare `spawn` (exercises `block_has_bare_spawn`'s recursion into
/// wait arms — the body opens an implicit nursery that joins at function return). byte-identical against the golden.
#[test]
fn vm_wait_arm_bare_spawn() {
    let src = "fn worker():\n    print(\"worker ran\")\nfn main():\n    a := Channel[int]()\n    a.send(1)\n    wait:\n        v := a.recv():\n            spawn worker()\n    print(\"after wait\")\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "after wait\nworker ran\n");
}

/// §6d regression — a multi-arm `wait` whose arm body references an OUTER local inside a fused
/// binop (`x + w`): the peephole fuses `GetLocal+GetLocal+Add` → `BinLocalLocal`, shifting the
/// arm-body indices, so `WaitPoll.arm_targets` must be relocated (else the wake jumps PAST the
/// bind prologue → VM 65, interp 66). Pins VM == interp on every arm.
#[test]
fn vm_wait_arm_body_outer_local_in_binop_matches_interp() {
    let src = "fn pick(a: Channel[int], b: Channel[int], x: int) -> int:\n    wait:\n        v := a.recv(): return x + v\n        w := b.recv(): return x + w\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    b.send(65)\n    print(pick(a, b, 1))\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "66\n");
}

/// 1-key regression PIN (TDD step 1): an ordinary blocking `recv` (NOT a `wait`) under
/// `--parallel`, woken by a sibling `send`, still works byte-identically — this guards the
/// `SchedCore.parked` refactor (`Vec<Fiber>` → `Vec<ParkedEntry>`) from regressing the recv park.
/// Asserts both the parallel output AND the plain output.
#[test]
fn vm_wait_single_arm_recv_park_unchanged_under_parallel() {
    let src = "fn consumer(a: Channel[int]):\n    v := a.recv()\n    print(\"got {v}\")\nfn producer(a: Channel[int]):\n    a.send(7)\nfn main():\n    a := Channel[int]()\n    parallel:\n        spawn consumer(a)\n        spawn producer(a)\nmain()\n";
    assert_eq!(run_capture(src).expect("parallel recv park"), "got 7\n");
    assert_eq!(run(src), "got 7\n");
}

#[test]
fn vm_channel_recv_on_closed_empty_faults() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.recv())\nmain()\n",
    );
    assert!(err.contains("receive on a closed channel"), "{err}");
}

#[test]
fn vm_channel_drains_buffered_after_close() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.close()\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "1\n2\n");
}

#[test]
fn vm_channel_try_send_false_when_closed() {
    let src = "fn main():\n    ch := Channel[int]()\n    print(ch.try_send(1))\n    ch.close()\n    print(ch.try_send(2))\nmain()\n";
    assert_eq!(run(src), "true\nfalse\n");
}

#[test]
fn vm_channel_double_close_ok() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.close()\n    print(1)\nmain()\n";
    assert_eq!(run(src), "1\n");
}

#[test]
fn vm_channel_close_then_len_zero() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.len())\nmain()\n";
    assert_eq!(run(src), "0\n");
}

#[test]
fn vm_channel_try_recv_closed_empty_is_none() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    match ch.try_recv():\n        ?v: print(v)\n        None: print(\"none\")\nmain()\n";
    assert_eq!(run(src), "none\n");
}

#[test]
fn vm_for_over_channel_drains_then_exits() {
    // Producer-first (no concurrency needed): the channel is closed+full before the `for` runs.
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.send(3)\n    ch.close()\n    total := 0\n    for v in ch:\n        total = total + v\n    print(total)\nmain()\n";
    assert_eq!(run(src), "6\n");
}

/// `--parallel`: a `for v in ch:` consumer that runs ahead of the producer PARKS on the empty
/// channel; a sibling `close()` (no value sent) wakes it and the loop ends cleanly (0 iterations).
#[test]
fn parallel_close_wakes_parked_receiver() {
    let src = "\
fn produce(ch: Channel[int]):
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    n := 0
    for v in ch:
        n = n + 1
    out.send(n)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.recv())
main()
";
    assert_eq!(run_capture(src).expect("parallel run"), "0\n");
}

/// `--parallel`: a single `close()` must wake EVERY receiver parked on the channel (not just one,
/// as a `send` would). Three consumers each loop-then-report; all three exit and report.
#[test]
fn parallel_close_wakes_multiple_receivers() {
    let src = "\
fn consume(ch: Channel[int], done: Channel[int]):
    for v in ch:
        n := v
    done.send(1)
fn main():
    ch := Channel[int]()
    done := Channel[int]()
    parallel:
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn:
            ch.close()
    total := 0
    for i in 0..3:
        total = total + done.recv()
    print(total)
main()
";
    assert_eq!(run_capture(src).expect("parallel run"), "3\n");
}

/// `--parallel`: a consumer that loops `recv` past the producer's last value used to deadlock-fault;
/// with `for v in ch:` + a producer `close()` it drains the buffered values and exits cleanly.
#[test]
fn parallel_consumer_loop_terminates_on_close() {
    let src = "\
fn produce(ch: Channel[int]):
    for i in 1..6:
        ch.send(i)
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    total := 0
    for v in ch:
        total = total + v
    out.send(total)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn produce(ch)
        spawn consume(ch, out)
    print(out.recv())
main()
";
    // 1+2+3+4+5 = 15, however the producer/consumer interleave.
    assert_eq!(run_capture(src).expect("parallel run"), "15\n");
}

/// Channel.close() golden: producer sends a run + `close()`s; consumer `for v in ch:` drains then
/// ends cleanly. VM output == expected (decision A oracle), and the `--parallel`
/// engine (consumer parks on the empty channel, woken by the producer's send/close) prints the
/// same total. Pins the headline `for`-over-channel + `try_send`-after-close surface.
#[test]
fn golden_parallel_channel_close_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/parallel_channel_close.chz");
    let expected = include_str!("../../../examples/parallel_channel_close.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

#[test]
fn golden_parallel_channel_close_chz_parallel_engine() {
    let src = include_str!("../../../examples/parallel_channel_close.chz");
    let expected = include_str!("../../../examples/parallel_channel_close.expected");
    assert_eq!(run_capture(src).expect("parallel run"), expected);
}

#[test]
fn parallel_send_after_close_faults() {
    let src = "\
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            ch.close()
            ch.send(1)
main()
";
    let err = run_capture(src).expect_err("send after close should fault");
    assert!(
        err.message.contains("send on a closed channel"),
        "{}",
        err.message
    );
}

/// B3.6 golden: `Executor` tasks run on the bounded pool. Three submitted closures capture the
/// result `Channel` (sendable → crosses as a shared `Arc`) and `send` from pool threads; `shutdown`
/// drains them onto the pool and joins, then main sorts what it gathered → fixed printed order
/// however threads interleave. Running it without `--parallel` too (decision A oracle: same
/// output, inline drain) proves the `submit`-by-value / pool-drain change is observationally inert.
#[test]
fn golden_executor_pool_chz_matches_expected() {
    let src = include_str!("../../../examples/executor_pool.chz");
    let expected = include_str!("../../../examples/executor_pool.expected");
    assert_eq!(run_capture(src).expect("parallel run"), expected);
    assert_eq!(run_capture(src).expect("vm run"), expected);
}

/// D1 (lazy module snapshot): a `--parallel` task that calls a sibling free function which in
/// turn reads a module-level global must resolve both against the worker's *own* home module —
/// exercising lazy fault-in of that module into the worker heap on first global access. The
/// plain (no-`--parallel`) run is the equivalence oracle (same output). Characterization test:
/// green before D1 (eager `build_worker_modules`) and after (lazy snapshot).
#[test]
fn parallel_task_resolves_sibling_fn_and_global() {
    let src = "\
G := 100
fn helper(x: int) -> int:
    return x + G
fn send_one(ch: Channel[int], x: int):
    ch.send(helper(x))
fn main():
    ch := Channel[int]()
    parallel:
        spawn send_one(ch, 1)
        spawn send_one(ch, 2)
    a := ch.recv()
    b := ch.recv()
    print(a + b)
main()
";
    assert_eq!(run_capture(src).expect("parallel run"), "203\n");
    assert_eq!(run_capture(src).expect("vm run"), "203\n");
}

/// D1 (lazy module snapshot): many trivial `--parallel` spawns no longer pay a full
/// per-task module-graph rebuild. Correctness gate — every one of N tasks reaches the same
/// `Shared` box, so the serialised count is exactly N. The cost is a COUNT, not a clock
/// (TICKET-233): a task that reads no module global replays no module, where the eager per-task
/// rebuild replayed every module for every task.
#[test]
fn parallel_many_spawns_cheap_and_correct() {
    const N: usize = 2000;
    let program = |n: usize, step: &str| {
        // TICKET-246 (plan step 9): the global read sits in the TASK body, outside the box
        // closure, so the replay count stays a property of `spawn`. A box closure reads a scalar
        // global through the box's slot door, which replays no module.
        let mut src = format!(
            "import Shared from std.concurrency\nstep: int = 1\nfn bump(s: Shared[int]):\n    k := {step}\n    s.update(fn(x): x + k)\nfn main():\n    s := Shared(0)\n    parallel:\n"
        );
        for _ in 0..n {
            src.push_str("        spawn bump(s)\n");
        }
        src.push_str("    print(s.get())\nmain()\n");
        src
    };
    let (out, replays) = run_capture_counting(&program(N, "1"), &MODULE_REPLAYS);
    assert_eq!(out.expect("parallel run"), format!("{N}\n"));
    assert_eq!(
        replays, 0,
        "{N} spawns that read no module global replayed {replays} modules (want 0) -- a task is \
         rebuilding a module it never reads"
    );
    // The control: a task that reads one module global replays that one module, so the count is live.
    let (out, replays) = run_capture_counting(&program(50, "step"), &MODULE_REPLAYS);
    assert_eq!(out.expect("parallel run"), "50\n");
    assert_eq!(
        replays, 50,
        "50 spawns that each read one module global replayed {replays} modules (want 50)"
    );
}

/// W8-34: `List.unique()` is O(N^2) (`seq_slot` linear-scans `out` per element) — 80k ints /
/// 40k distinct took 8.47s wall on the release binary vs CPython's `dict.fromkeys` at 2.1ms
/// on the same box. Pinned with a COUNT of element comparisons (TICKET-233): the hash index
/// compares each element with its own bucket only, the scan with every kept element.
#[test]
fn unique_is_not_quadratic() {
    let src = "fn main():\n    xs := List[int]()\n    i := 0\n    while i < 4000:\n        xs.push(i % 2000)\n        i = i + 1\n    ys := xs.unique()\n    print(ys.len())\nmain()\n";
    let (out, compares) = run_capture_counting(src, &ELEM_COMPARES);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "2000\n");
    assert!(
        (2000..=8000).contains(&compares),
        "unique() on 4000 ints (2000 distinct) made {compares} element comparisons (want 2000..=8000) -- O(N^2) regression"
    );
}

/// TICKET-072: `s[i]` collects a fresh `Vec<char>` of the WHOLE string per subscript
/// (`src/vm/stmt.rs:665`), so an index loop over a string is O(n^2) instead of O(n). Measured on
/// the release binary at 86eb23dd: n=10000 took 0.142s vs CPython's 0.0019s, n=40000 took 1.678s
/// (220x CPython) -- a clean 4x-per-doubling signature, not CPython's 2x. Pinned with a COUNT of
/// the characters each subscript read (TICKET-233): one on the ASCII arm, `len(s)` on the scan.
#[test]
fn string_index_loop_is_not_quadratic() {
    let src = "fn main():\n    n := 8000\n    s := \"a\".repeat(n)\n    c := 0\n    i := 0\n    while i < n:\n        if s[i] == \"a\": c = c + 1\n        i = i + 1\n    print(c)\nmain()\n";
    let (out, chars) = run_capture_counting(src, &STR_INDEX_CHARS);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "8000\n");
    assert_eq!(
        chars, 8000,
        "indexing an 8000-char string 8000 times read {chars} characters (want 8000) -- O(n^2) regression"
    );
}

/// TICKET-072: `core_method`'s `Obj::Str` arm used to `to_string()` (clone) the whole receiver
/// before dispatching ANY method, so a borrow-only method like `starts_with` cost O(len(s)) per
/// call. Pinned with a COUNT of the receiver bytes that clone copied (TICKET-233): zero for each
/// of the five borrow-only methods, the whole receiver for `upper`, which proves the count is live.
#[test]
fn str_method_dispatch_does_not_clone_the_receiver() {
    let src = "fn main():\n    s := \"a\".repeat(1000000)\n    c := 0\n    i := 0\n    while i < 200:\n        if s.starts_with(\"a\"): c = c + 1\n        if s.ends_with(\"a\"): c = c + 1\n        if s.contains(\"a\"): c = c + 1\n        c = c + s.index_of(\"a\") + s.len()\n        i = i + 1\n    print(c)\n    print(s.upper().len())\nmain()\n";
    let (out, cloned) = run_capture_counting(src, &STR_RECV_CLONED_BYTES);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "200000600\n1000000\n");
    assert_eq!(
        cloned, 1_000_001,
        "1000 borrow-only method calls on a 1MB string cloned {cloned} receiver bytes (want 1000001: one for `\"a\".repeat`, 1000000 for the one `upper`) -- the receiver is still being cloned per call"
    );
}

/// B3.6: a submitted closure capturing a plain value (`int`) observes it **by value** across the
/// airlock — exercises the `WireValue::Closure` capture round-trip (not just the shared-`Arc` handle
/// path the golden's `Channel` capture takes). Auto-drained at program exit; the
/// `--parallel` drain reconstructs and runs the closure on the pool.
#[test]
fn executor_submitted_closure_captures_by_value() {
    let src = "import Executor from std.concurrency\nfn main():\n    n := 7\n    ex := Executor()\n    ex.submit(fn(): print(n))\nmain()\n";
    assert_eq!(run_capture(src).expect("vm run"), "7\n");
    assert_eq!(run_capture(src).expect("parallel run"), "7\n");
}

/// A submitted closure crosses the airlock **by value**: it isolates its captures at
/// `submit` time (`wire_callable` → `to_wire` deep-copies), so a mutation of a captured collection
/// after the `submit` is NOT observed by the job. Eager execution shrank that window to nothing on the
/// default (no-`--parallel`) run (the job is already running), but the isolation is what makes the
/// plain run and M:N agree here regardless — it has always been keyed to the `submit`, never to when
/// the job runs. The since-removed cooperative engine
/// used to share captures by reference (queuing the closure's own `Handle`) to mirror the tree-walk
/// `interp` oracle; that oracle has been removed and byte-identical output is now the sole invariant,
/// so a plain run isolates identically to M:N — both print `[1]` here (not `[1, 2]`).
#[test]
fn executor_cooperative_submit_isolates_captures_by_value() {
    let src = "import Executor from std.concurrency\nfn main():\n    xs := [1]\n    ex := Executor()\n    ex.submit(fn(): print(xs))\n    xs.push(2)\nmain()\n";
    assert_eq!(
        run_capture(src).expect("vm run"),
        "[1]\n",
        "plain submit isolates the captured list by value at submit time"
    );
    assert_eq!(
        run_capture(src).expect("parallel run"),
        "[1]\n",
        "M:N submit isolates the captured list by value too"
    );
}

/// B3.3-threads: a nested `parallel:` runs on the same bounded pool without exploding the thread
/// count (each join level adds only its own participating thread). Two outer tasks each spawn two
/// inner tasks, all bumping one `Shared` → `4`, deterministic.
#[test]
fn parallel_nested_nursery_on_pool() {
    let src = "import Shared from std.concurrency\nfn inner(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn outer(s: Shared[int]):\n    parallel:\n        spawn inner(s)\n        spawn inner(s)\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn outer(s)\n        spawn outer(s)\n    print(s.get())\nmain()\n";
    assert_eq!(run_capture(src).expect("parallel run"), "4\n");
}

/// Eager `Executor` — the successor to B3.3-threads' `done_signal_bumps_counter_on_panic`. That test
/// constructed the retired `DoneSignal` guard directly and asserted its `Drop` bumped the batch join's
/// counter on unwind. Eager execution replaced the batch farm/join (`run_workers_on_pool`) with a
/// per-job dispatch, so the invariant is now `the Executor's sched::outstanding` always reaching 0 — and it is
/// testable on the REAL path instead of on a guard in isolation: a job that ends abnormally must still
/// record its outcome, or `shutdown`'s condvar wait never wakes and the program hangs forever.
///
/// The watchdog is the point: without it a regression here HANGS the test binary instead of failing it
/// (the failure mode of the whole eager milestone), so assert on a bounded `recv_timeout`.
#[test]
fn executor_faulting_job_does_not_hang_shutdown() {
    let src = "import Executor from std.concurrency\n\
               ex := Executor()\n\
               t := ex.submit(fn(): panic(\"boom\"))\n\
               r: None!Error = recover: ex.shutdown()\n\
               match r:\n    \
                   ?_: print(\"no fault\")\n    \
                   !e: print(\"caught: {e.message()}\")\n\
               match t.get():\n    \
                   ?_: print(\"value\")\n    \
                   !e: print(\"task: {e.message()}\")\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("shutdown() must not hang when a job ends abnormally");
    // TICKET-289 — a job's fault is its Task's outcome: `shutdown()` raises nothing, the run goes
    // on, and `t.get()` gives the fault message.
    let out = got.expect("the job's fault does not end the run");
    assert_eq!(out, "no fault\ntask: boom\n");
}

/// B3.3-threads (decision F, review coverage gap): each worker buffers its own stdout and the join
/// flushes them **in task order** — so three concurrently-run tasks that each `print` produce a
/// deterministic, task-ordered transcript regardless of thread interleaving.
#[test]
fn parallel_output_flushes_in_task_order() {
    let src = "fn emit(s: str):\n    print(s)\n\
                   fn main():\n    parallel:\n        spawn emit(\"alpha\")\n        spawn emit(\"beta\")\n        spawn emit(\"gamma\")\nmain()\n";
    assert_eq!(
        run_capture(src).expect("parallel run"),
        "alpha\nbeta\ngamma\n"
    );
}

/// B3.3-threads: a fault in a **pool** task (not the inline task[0]) propagates out of the join as
/// the nursery's error after all siblings finish (sibling-abort is B3.4; here we join-then-report
/// the first fault). The ok task and the faulting task are independent (no channel) so there is no
/// deadlock without cancellation.
#[test]
fn parallel_pool_task_fault_propagates() {
    let src = "import Shared from std.concurrency\nfn ok_task(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn boom():\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn ok_task(s)\n        spawn boom()\n    print(\"unreached\")\nmain()\n";
    let err = run_capture(src).expect_err("expected the pool task fault to propagate");
    assert!(
        err.message.contains("out of bounds"),
        "got: {}",
        err.message
    );
}

/// gap #2: a plain `return` inside a `parallel:` body jumps past `JoinNursery`, so the nursery is
/// never popped at the dedent. It must truncate `self.nurseries` back to the frame's entry
/// depth (via `do_return`/`drain_escaped_nursery`), or the stale nursery leaks. TASK B: the escape
/// now also CANCELS-AND-REPORTS the unstarted
/// `noop()` — one report line precedes the early-return value. White-box residual-depth check.
/// A subsequent `parallel:` runs on a clean stack (its empty join is silent).
#[test]
fn parallel_return_escape_leaves_clean_nursery_stack() {
    let src = "fn noop():\n    0\n\
                   fn worker() -> int:\n    parallel:\n        spawn noop()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\n    parallel:\n        spawn noop()\nmain()\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("vm run");
    // `worker`'s parallel: escapes via `return` with one pending task → cancel+report, then `5`.
    // `main`'s trailing parallel: dedents normally to its join (NOT an escape), so it runs `noop()`
    // silently — no report, proving a later parallel: still works on the reclaimed stack.
    assert_eq!(
        vm_out, "5\n",
        "early return wins; the escaped nursery is reclaimed silently"
    );
    assert_eq!(
        nursery_depth, 0,
        "the return-escaped nursery must be reclaimed, not leaked"
    );
}

/// gap #2, second escape form: an uncaught `?` that propagates out of the frame (no `recover:`
/// between the `parallel:` and the program top) must also reclaim the skipped nursery via
/// `do_return`. The whole program faults, but the run-so-far must leave no leaked nursery. TASK B:
/// the unstarted `noop()` is cancelled-and-reported — one report line is on stdout before the fault
/// (the since-removed interp engine already dropped it on `?`; the report now emits identically).
#[test]
fn parallel_try_escape_leaves_clean_nursery_stack() {
    // `main()?`, not a bare `main()`: a bare top-level drop no longer aborts, so the `?` is
    // what keeps the program faulting.
    let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return !\"x\"\n\
                   fn main() -> int!:\n    parallel:\n        spawn noop()\n        y := boom()?\n        print(y)\n    return 0\nmain()?\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    assert!(vm_out.is_err(), "the uncaught ? faults the program");
    assert_eq!(
        nursery_depth, 0,
        "the ?-escaped nursery must be reclaimed, not leaked"
    );
    // §2c1 — the cancel is SILENT: stdout-so-far is empty, identical across engines.
    let (vm_so_far, vm_res) = run_program(src);
    assert!(vm_res.is_err());
    assert_eq!(vm_so_far, "", "VM: nothing on stdout before the fault");
    let (interp_so_far, interp_res) = run_program(src);
    assert!(interp_res.is_err());
    assert_eq!(interp_so_far, vm_so_far, "stdout-so-far divergence");
}

/// gap #2, boundary: a `?` inside a `parallel:` that IS caught by a same-frame `recover:` must
/// stay on the **existing** handler-catch reclaim (`Handler::nursery_len`), NOT the new
/// `do_return` truncate — the two paths are mutually exclusive in `do_try` (recover-scoped `?`
/// jumps to the handler and never calls `do_return`). TASK B: that recover-catch reclaim site now
/// routes through `drain_escaped_nursery`, so a recover-caught `?` cancels the pending
/// `noop()` IDENTICALLY to an uncaught `?`, silently (§2c1). Asserts the two
/// reclaim paths don't fight: the recovered program continues, a later `parallel:` runs, stack clean.
#[test]
fn parallel_try_caught_by_recover_leaves_clean_nursery_stack() {
    let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return !\"x\"\n\
                   fn main():\n    r := recover:\n        parallel:\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\n    parallel:\n        spawn noop()\nmain()\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
    // The recover-caught `?` cancels its one pending task and reports, THEN the recover continues.
    // `main`'s trailing parallel: joins normally (not an escape) → silent.
    assert_eq!(
        vm_out, "recovered\n",
        "recover swallows the fault; the cancel is silent"
    );
    assert_eq!(
        nursery_depth, 0,
        "the recover-caught nursery is reclaimed via the handler path"
    );
}

/// gap #2, ordering boundary: a recover-scoped `?` escaping a `parallel:` whose BODY has a
/// `defer` must run that defer BEFORE the recover
/// continues — `exec_parallel` reports only after the body's
/// `exec_scoped_block` has drained its defers. Regression for the do_try report-before-body-defer
/// divergence (the report previously trailed the parallel-body defer on the VM). Body-defer →
/// report → recovered, byte-identical on a plain run and under `--parallel`.
#[test]
fn parallel_recover_scoped_try_orders_report_after_body_defer() {
    let src = "fn noop():\n    0\n\
                   fn pdefer():\n    print(\"PDEFER\")\n\
                   fn boom() -> int!:\n    return !\"x\"\n\
                   fn main():\n    r := recover:\n        parallel:\n            defer pdefer()\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\nmain()\n";
    let expected = "PDEFER\nrecovered\n";
    let interp_out = run_capture(src).expect("repeat run");
    assert_eq!(
        interp_out, expected,
        "body-defer precedes report precedes recover"
    );
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
    assert_eq!(
        vm_out, expected,
        "the parallel-body defer runs before the recover continues"
    );
    assert_eq!(
        nursery_depth, 0,
        "the recover-caught nursery is reclaimed, not leaked"
    );
    assert_eq!(
        run_capture(src).expect("--parallel run"),
        expected,
        "--parallel run"
    );
}
