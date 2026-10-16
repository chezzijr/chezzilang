//! VM tests: Executor owner cut, held bytes, cut seal epoch, box context, method-miss messages.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// TICKET-195 C1: an owner cut by a nursery child's panic while it waits in `ex.shutdown()` of an
/// Executor created OUTSIDE the nursery must not cancel that Executor's running job — CPython lets
/// it finish (docs/concurrency.md: such an Executor is not cancelled).
#[test]
fn outside_executor_job_survives_owner_cut_in_shutdown() {
    let src = r#"import std.time
import Executor from std.concurrency
done := Channel[int](1)
fn job():
    time.sleep_ms(300)
    print("job finished its work")
    done.send(1)
ex := Executor()
ex.submit(job)
fn owner():
    parallel:
        spawn:
            time.sleep_ms(50)
            panic("boom")
        ex.shutdown()
r := recover: owner()
print(r)
time.sleep_ms(500)
print("job done? {done.try_recv()}")
"#;
    let entry = write_temp_chz("t195_c1", src);
    let (out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(
        res.is_ok(),
        "expected Ok, got: {:?}; out={out:?}",
        res.err().map(|e| e.message)
    );
    assert!(
        out.contains("job finished its work"),
        "job was stopped: out={out:?}"
    );
    assert!(out.contains("job done? 1"), "job was stopped: out={out:?}");
}

/// TICKET-195 C3: an Executor job's fault outranks the deadlock verdict of a main that waits on a
/// channel the job would have sent. Go prints the job's index panic.
#[test]
fn unjoined_job_fault_outranks_a_main_deadlock() {
    let src = r#"import Executor from std.concurrency
ch := Channel[int](0)
fn work(xs: List[int]):
    ch.send(xs[5])
ex := Executor()
ex.submit(fn(): work([1, 2]))
print(ch.recv())
ex.shutdown()
"#;
    let entry = write_temp_chz("t195_c3", src);
    let (_out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("expected a fault").message;
    assert!(
        msg.contains("index 5 out of bounds"),
        "job fault lost, got: {msg}"
    );
    assert!(
        !msg.contains("deadlock"),
        "verdict outranked the job fault: {msg}"
    );
}

/// TICKET-195 C4: a party cut by a child's fault reports that fault; its own `defer` fault ranks
/// below the root cause.
#[test]
fn cut_owner_defer_fault_ranks_below_the_child_fault() {
    let src = r#"import std.time
r := recover:
    parallel:
        spawn:
            time.sleep_ms(20)
            panic("boom")
        defer:
            panic("cleanup failed")
        time.sleep_ms(1000)
print(r)
"#;
    let entry = write_temp_chz("t195_c4", src);
    let (out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(
        res.is_ok(),
        "expected Ok, got: {:?}",
        res.err().map(|e| e.message)
    );
    assert!(
        out.contains("!boom"),
        "cut owner's defer fault won: out={out:?}"
    );
}

/// TICKET-195 C5: a delivered child fault prints the faulting party's frames, never the receiver's.
#[test]
fn cut_owner_trace_is_the_childs_trace() {
    let src = r#"import std.time
fn b():
    panic("boom")
fn a():
    time.sleep_ms(50)
    b()
fn waiter():
    time.sleep_ms(1000)
fn owner():
    parallel:
        spawn a()
        waiter()
owner()
"#;
    let entry = write_temp_chz("t195_c5", src);
    let (_out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    let e = res.expect_err("expected a fault");
    assert!(e.message.contains("boom"), "got: {}", e.message);
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert!(names.contains(&"b"), "child's frames missing: {names:?}");
    assert!(
        !names.contains(&"waiter") && !names.contains(&"owner"),
        "receiver's frames: {names:?}"
    );
}

/// TICKET-195 step 10: a run that ends in a deadlock verdict still drains its executors, so a job's
/// buffered output reaches the in-process sink — for a finished job and for one parked forever.
#[test]
fn a_verdict_run_still_drains_executor_output() {
    let finished = r#"import Executor from std.concurrency
import std.time
ch := Channel[int](0)
fn job():
    print("job printed")
ex := Executor()
ex.submit(job)
time.sleep_ms(200)
print(ch.recv())
"#;
    let parked = r#"import Executor from std.concurrency
import std.time
ch := Channel[int](0)
jstuck := Channel[int](0)
fn job():
    print("job started")
    print(jstuck.recv())
ex := Executor()
ex.submit(job)
time.sleep_ms(200)
print(ch.recv())
"#;
    for (src, line) in [(finished, "job printed"), (parked, "job started")] {
        let entry = write_temp_chz("t195_drain", src);
        let (out, _err, res, _code) = run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        let msg = res.expect_err("expected a verdict").message;
        assert!(msg.contains("deadlock"), "got: {msg}");
        assert!(out.contains(line), "job output lost: out={out:?}");
    }
}

#[test]
fn executor_job_sees_globals_as_of_its_own_submit() {
    let src = r#"
import Executor from std.concurrency
xs := [1]
fn show() -> str:
    return "{xs}"
fn main():
    out := Channel[str](4)
    ex := Executor()
    ex.submit(fn(): out.send("job1 " + show()))
    print(out.recv())
    xs.push(2)
    ex.submit(fn(): out.send("job2 " + show()))
    print(out.recv())
    ex.shutdown()
main()
"#;
    let out = run_capture(src).expect("run");
    assert_eq!(out, "job1 [1]\njob2 [1, 2]\n");
}

/// A sched under a live cap whose first `n` fibers are seeded and unstarted.
fn capped_sched(n: usize) -> Arc<MnSched> {
    let sched = Arc::new(MnSched::new(
        n,
        1,
        Arc::new(AtomicBool::new(false)),
        dl_err(),
        usize::MAX,
    ));
    sched.seed((0..n).map(mk_fiber).collect());
    sched
}

/// Run the next fiber of `sched` and finish it with a 4000-byte buffered `out`.
fn finish_with_output(sched: &MnSched) {
    let f = take_run(sched);
    sched.finish(
        f.task_index,
        0,
        TaskOutcome::Cancelled {
            out: vec![0u8; 4000],
            stderr: Vec::new(),
        },
    );
}

/// W7-26, the sampling half (TICKET-208: the sched owns it) — `take_charge` reports GROWTH, never
/// the running total: charging the total per submit would sweep on every submit, and charging
/// nothing leaves the cap unsampled. Fails if `take_charge` returns the total twice, or if
/// `take_finished_streams` leaves the retained bytes charged.
#[test]
fn mnsched_take_charge_reports_growth_only() {
    let sched = capped_sched(2);
    assert_eq!(sched.take_charge(), 0, "nothing finished yet");
    finish_with_output(&sched);
    let first = sched.take_charge();
    assert!(first >= 4000, "one finished task's output: {first}");
    assert_eq!(sched.take_charge(), 0, "no growth since the last call");
    let (out, _) = sched.take_finished_streams();
    assert_eq!(out.len(), 4000);
    assert_eq!(
        sched.held_bytes(),
        0,
        "the streams left, the charge with them"
    );
    finish_with_output(&sched);
    assert_eq!(
        sched.take_charge(),
        first,
        "the second task grows it by the same"
    );
}

/// W7-26 / DEC-205 (TICKET-208: one facade) — `Heap::live_bytes` counts what an Executor holds
/// outside every heap: its finished tasks' retained output and its unfinished tasks' submit-time
/// bytes. Fails if `held_bytes` omits either half, or if a reduced Executor stays charged.
#[test]
fn live_bytes_counts_an_executors_held_bytes() {
    use crate::vm::heap::{Heap, Obj};
    let mut h = Heap::new();
    let core = Arc::new(crate::vm::core::ExecutorCore::default());
    let r = h.alloc(Obj::Executor(Arc::clone(&core)));
    let base = h.live_bytes();

    let sched = capped_sched(10);
    core.attach_test_sched(Arc::clone(&sched));
    for _ in 0..10 {
        finish_with_output(&sched);
    }
    let with_results = h.live_bytes();
    assert!(
        with_results >= base + 40_000,
        "ten finished tasks' output must register: {base} -> {with_results}"
    );
    {
        let mut f = mk_pending_fiber(10);
        let mut c = sched.lock();
        sched.reserve_slot(&mut c, &mut f, 0, 5000);
    }
    assert!(
        h.live_bytes() >= with_results + 5000,
        "an unfinished task's submit-time bytes must register"
    );
    *core.scope.lock().unwrap() = None;
    assert_eq!(h.live_bytes(), base, "a reduced Executor holds nothing");
    let _ = r;
}

/// W7-26 (the second arm) — the two `--max-heap` walks share one de-dup set, so whichever meets
/// the Executor first must charge its held bytes. Both visit orders. Fails if
/// `drain_pending_core_bytes` and `Heap::live_bytes` read different numbers.
#[test]
fn nested_executor_charges_its_held_bytes_in_either_visit_order() {
    use crate::vm::core::AtomicCore;
    use crate::vm::heap::{Heap, Obj};
    let ex = Arc::new(crate::vm::core::ExecutorCore::default());
    let sched = capped_sched(10);
    ex.attach_test_sched(Arc::clone(&sched));
    for _ in 0..10 {
        finish_with_output(&sched);
    }
    let eager_bytes = ex.held_bytes();
    assert!(eager_bytes >= 40_000);

    // (a) reachable ONLY through a `Shared`'s payload — no `Obj::Executor` slot at all.
    let mut h = Heap::new();
    h.set_mem_cap(1);
    let outer = Arc::new(AtomicCore::default());
    outer.store(crate::vm::wire::WireValue::Executor(Arc::clone(&ex)));
    let base = h.live_bytes();
    let sr = h.alloc(Obj::Atomic(Arc::clone(&outer)));
    assert!(
        h.live_bytes() >= base + eager_bytes,
        "a nested executor's held bytes must be charged"
    );
    let _ = sr;

    // (b) the executor DOES have its own slot, but the enclosing `Shared` is met first.
    let mut h = Heap::new();
    h.set_mem_cap(1);
    let base = h.live_bytes();
    let sr = h.alloc(Obj::Atomic(Arc::clone(&outer)));
    let er = h.alloc(Obj::Executor(Arc::clone(&ex)));
    assert!(
        h.live_bytes() >= base + eager_bytes,
        "visit order must not decide whether the held bytes are counted"
    );
    let (_, _) = (sr, er);
}

/// TICKET-208, the lift direction of W7-56 — an Executor job that has FINISHED is no live sender,
/// so a nursery task parked on a channel nobody feeds must still fault as a deadlock. Fails (the
/// watchdog fires) if a finished job keeps vetoing the verdict, or if the nursery prints `after`.
#[test]
fn a_finished_executor_job_no_longer_vetoes_a_parked_nursery_task() {
    let src = "import Executor from std.concurrency\n\
               c := Channel[int](0)\n\
               fn noop():\n    pass\n\
               fn waiter():\n    _v := c.recv()\n\
               ex := Executor()\n\
               ex.submit(noop)\n\
               parallel:\n    spawn waiter()\n\
               print(\"after\")\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_program(src));
    });
    let (out, res) = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("a finished job must not veto the deadlock verdict forever");
    let msg = res.expect_err("expected a deadlock fault").message;
    assert!(msg.contains("deadlock"), "got: {msg}");
    assert!(!out.contains("after"), "the nursery returned: {out:?}");
}

/// TICKET-213 — an Executor's runner thread waits briefly for the next job before it leaves, so a
/// loop of sequential submit-then-wait round trips reuses its runner. TICKET-208 let the runner
/// leave the moment no job was unfinished: 2000 round trips started 2000 threads (measured
/// `clone` count 2007-2014 per run, base `fcbe2596` 8-11) and ran 2.5x slower. The bound is loose
/// on purpose: a starter descheduled longer than the wait costs one extra start, never a failure
/// of the rule.
#[test]
fn sequential_executor_round_trips_reuse_their_runner() {
    let vm = ran_standalone(
        "ex := Executor()\nch := Channel[int](0)\nn := 0\nfor i in 0..200:\n    ex.submit(fn(): ch.send(1))\n    n = n + ch.recv()\n",
    );
    let Obj::Module(m) = vm.heap.get(vm.module_objs[0]) else {
        panic!("module_objs[0] is the entry module");
    };
    let core = m
        .slots
        .iter()
        .filter_map(|v| v.as_obj())
        .find_map(|h| match vm.heap.get(h) {
            Obj::Executor(core) => Some(Arc::clone(core)),
            _ => None,
        })
        .expect("the `ex` global");
    let starts = core.runner_starts.load(Ordering::Relaxed);
    assert!(
        starts < 100,
        "200 sequential round trips started {starts} runner threads: the runner is not reused"
    );
}

/// TICKET-232 — a cut job's handle is sealed inside `job_event`, under the sched lock. The join
/// returns on the slot, so a seal written after the unlock leaves a reader on an empty channel
/// with no counted party: a false deadlock. Fails if the seal moves back out of the transition.
#[test]
fn a_cut_job_handle_is_sealed_in_the_lock_hold_that_ends_the_job() {
    let sched = mk_sched(1);
    let ch = empty_core();
    let mut c = sched.lock();
    c.leaf_site = true;
    c.exec_active = 1;
    c.job_settle.insert(
        0,
        JobSettle {
            ch: Arc::clone(&ch),
            cancel: WireValue::Int(7),
        },
    );
    let _step = c.job_event(
        JobEvent::Ended {
            task_index: 0,
            scope_id: 0,
            done: false,
        },
        RunHalt::Running,
    );
    assert!(
        ch.q.lock().unwrap().is_sealed(),
        "the handle is unsealed when job_event returns under the sched lock"
    );
    drop(c);
}

/// TICKET-236 — the bump-site grid: every way a waker leaves the counts a verdict reads moves the
/// run's epoch, so a judge whose walk overlapped the leave declines. One row per call. A
/// `SchedCore` row also fails if the core's epoch handle is not its sched's.
#[test]
fn every_way_out_of_the_counts_moves_the_epoch() {
    use crate::vm::core::Pending;
    type Row = (&'static str, fn(&MnSched, Fiber));
    let rows: [Row; 13] = [
        ("park", |s, f| {
            let c = empty_core();
            s.park(core_key(&c), c, f);
        }),
        ("park_send", |s, f| s.park_send(0, f)),
        ("yield_fiber", |s, f| s.yield_fiber(f)),
        ("finish", |s, f| {
            s.finish(
                f.task_index,
                0,
                TaskOutcome::Cancelled {
                    out: Vec::new(),
                    stderr: Vec::new(),
                },
            );
        }),
        ("owner_blocked", |s, _| s.lock().owner_blocked(false)),
        ("close_body", |s, _| s.close_body(0)),
        ("leave_inflight", |s, _| {
            s.inflight.fetch_add(1, Ordering::Relaxed);
            s.leave_inflight();
        }),
        ("builder_joined", |s, _| s.lock().builder_joined(0)),
        ("block_shared", |s, _| {
            let _party = s.quiesce.block_shared(
                Arc::new(quiesce::PartyWait::Send(Pending::new())),
                crate::vm::block::WakeSet::default(),
                None,
            );
        }),
        ("set_body_wait block", |s, _| {
            s.set_body_wait(0, None, true, false)
        }),
        ("set_body_wait unblock", |s, _| {
            s.set_body_wait(0, None, false, true)
        }),
        ("scope_retired", |s, _| s.lock().scope_retired(0)),
        ("settle_job", |s, _| {
            let mut c = s.lock();
            c.job_settle.insert(
                0,
                JobSettle {
                    ch: empty_core(),
                    cancel: WireValue::Int(7),
                },
            );
            c.settle_job(0, true, &mut JobStep::default());
        }),
    ];
    for (name, leave) in rows {
        let sched = mk_sched(1);
        sched.seed(vec![mk_fiber(0)]);
        let f0 = take_run(&sched);
        let e0 = sched.quiesce.left();
        leave(&sched, f0);
        assert!(
            sched.quiesce.left() > e0,
            "{name} left the counts without moving the epoch"
        );
    }
}

/// TICKET-236 — a cut job's seal is its last channel effect, and the epoch moves AFTER it, inside
/// the lock hold that ends the job. Moved before the seal, a judge could open its bracket after
/// the bump, read the unsealed handle, then read the finished sched: a false verdict with an
/// unmoved epoch. The test holds the handle's queue lock so `settle_job` stops at the seal.
#[test]
fn a_cut_job_seal_moves_the_epoch_after_the_seal() {
    let sched = mk_sched(1);
    let ch = empty_core();
    let q = ch.q.lock().unwrap();
    let e0 = sched.quiesce.left();
    std::thread::scope(|sc| {
        let cutter = sc.spawn(|| {
            let mut c = sched.lock();
            c.job_settle.insert(
                0,
                JobSettle {
                    ch: Arc::clone(&ch),
                    cancel: WireValue::Int(7),
                },
            );
            c.settle_job(0, true, &mut JobStep::default());
        });
        // Until the cutter holds the sched lock; it then blocks at the seal, on `q`.
        while sched.core.try_lock().is_ok() {
            std::thread::yield_now();
        }
        // The sleep can only let a wrong order pass on a stalled box, never fail the right one.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(
            sched.quiesce.left(),
            e0,
            "the epoch moved before the handle was sealed"
        );
        drop(q);
        cutter.join().unwrap();
    });
    assert!(ch.q.lock().unwrap().is_sealed());
    assert!(sched.quiesce.left() > e0);
}

/// TICKET-232 — a sched that judges its own deadlock latches the verdict as a run halt BEFORE it
/// flags a victim. A flagged leaf unwinds and frees what it holds; with no halt latched, a party
/// waiting on that takes it (a ready wait outranks a halt, DEC-194) and runs on.
#[test]
fn the_sched_idle_judge_latches_the_verdict_before_it_flags_a_leaf() {
    let sched = mk_sched(1);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    sched.park(core_key(&core), Arc::clone(&core), f0);
    let _party = sched.quiesce.block_shared(
        Arc::new(quiesce::PartyWait::Send(crate::vm::core::Pending::new())),
        crate::vm::block::WakeSet::default(),
        Some(("x", Span::RUNTIME)),
    );
    assert!(matches!(sched.take_runnable(0, 1, 0), Take::Stop));
    assert_eq!(sched.quiesce.run_halt(), RunHalt::Deadlock);
}

/// TICKET-236 — the latch is set only by a verdict its own licence accepts. A latch never
/// un-latches (DEC-232), so a verdict latched before a licence that then declines the cut is
/// irrevocable: the run halts on a deadlock no victim was faulted for.
#[test]
fn a_declined_licence_latches_nothing() {
    let sched = mk_sched(1);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    sched.park(core_key(&core), Arc::clone(&core), f0);
    let _party = sched.quiesce.block_shared(
        Arc::new(quiesce::PartyWait::Send(crate::vm::core::Pending::new())),
        crate::vm::block::WakeSet::default(),
        Some(("x", Span::RUNTIME)),
    );
    assert!(sched.latch_own_verdict(|_| false).is_none());
    assert_eq!(sched.quiesce.run_halt(), RunHalt::Running);
    assert!(sched.latch_own_verdict(|_| true).is_some());
    assert_eq!(sched.quiesce.run_halt(), RunHalt::Deadlock);
}

/// TICKET-228 — the `no match arm` fault names a carrier by the pattern that matches it.
#[test]
fn no_arm_text_prints_pattern_syntax() {
    use crate::base::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
    assert_eq!(Vm::carrier_pattern_text(VID_SOME), Some("?_"));
    assert_eq!(Vm::carrier_pattern_text(VID_OK), Some("?_"));
    assert_eq!(Vm::carrier_pattern_text(VID_ERR), Some("!_"));
    assert_eq!(Vm::carrier_pattern_text(VID_NONE_VARIANT), Some("None"));
    // A user variant keeps its name: the function answers nothing for it.
    assert_eq!(Vm::carrier_pattern_text(VID_NONE_VARIANT + 1), None);
}

/// TICKET-228 — the standalone test path (`run_capture`) and the real-graph path type a prelude
/// method result the same way, so a carrier pattern on it lowers on both.
#[test]
fn standalone_path_lowers_carrier_patterns_like_the_graph_path() {
    let cells = [
        (
            "ch := Channel[int]()\nmatch ch.try_recv():\n    ?v:\n        print(\"got {v}\")\n    None:\n        print(\"empty\")\n",
            "empty\n",
        ),
        (
            "xs := [1]\nmatch xs.pop():\n    ?v:\n        print(v + 0)\n    None:\n        print(-1)\n",
            "1\n",
        ),
        (
            "match \"7\".parse_int():\n    ?v:\n        print(v + 0)\n    !e:\n        print(e)\n",
            "7\n",
        ),
    ];
    for (src, want) in cells {
        assert_eq!(run_capture(src).unwrap(), want, "standalone: {src}");
        assert_eq!(super::golden_tests::golden_entry(src), want, "graph: {src}");
    }
}

/// TICKET-228 (D6) — no runtime text (print, str, interpolation, a container element, an assert
/// or fault message, `recover:` text) names a removed spelling.
#[test]
fn runtime_text_names_no_removed_spelling() {
    let show = "print(x)\nprint(\"{x}\")\nprint(str(x))\nprint([x])\nprint({\"k\": x})\n";
    let progs = [
        format!("x: int? = 1\n{show}"),
        format!("x: int? = None\n{show}"),
        format!("x: int!str = !\"bad\"\n{show}"),
        format!("x: int!str = 2\n{show}"),
        "x: int? = 1\nassert x == None\n".to_string(),
        "x := recover: panic(\"p\")\nprint(x)\nprint([x])\n".to_string(),
        "fn f() -> int!str:\n    return !\"bad\"\nv := f()?\nprint(v)\n".to_string(),
    ];
    for src in &progs {
        let text = match run_capture(src) {
            Ok(out) => out,
            Err(e) => e.message,
        };
        let hit = text
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|w| ["Option", "Result", "Some", "Ok", "Err"].contains(w));
        assert_eq!(hit, None, "{src}\n-> {text}");
        assert!(!text.is_empty(), "{src}");
    }
}

#[test]
fn spawn_one_line_native_call_that_waits_does_not_panic() {
    let src = "fn main():\n    ch := Channel[int](1)\n    n := 0\n    parallel:\n        for _ in 0..50:\n            spawn ch.send(1)\n        for _ in 0..50:\n            n += ch.recv()\n    print(n)\nmain()\n";
    assert_eq!(run_capture(src).expect("spawn ch.send"), "50\n");
}

/// TICKET-235 — the receiver and the arguments of a one-line `spawn` are evaluated in the parent,
/// at the `spawn` (Go's `go f(x)`); only the call runs in the task. Goes red when an operand is
/// evaluated inside the entry thunk: `body` then prints before `pick`, and the second program
/// stops reporting the deadlock.
#[test]
fn spawn_call_operands_are_evaluated_in_the_parent() {
    assert_eq!(
        run_capture("fn pick(c: Channel[int]) -> Channel[int]:\n    print(\"pick\")\n    return c\nfn val() -> int:\n    print(\"val\")\n    return 7\nfn main():\n    ch := Channel[int](1)\n    parallel:\n        spawn pick(ch).send(val())\n        print(\"body\")\n        print(ch.recv())\nmain()\n").unwrap(),
        "pick\nval\nbody\n7\n"
    );
    let e = run_capture("fn main():\n    a := Channel[int](1)\n    parallel:\n        spawn print(a.recv())\n        a.send(1)\nmain()\n")
        .expect_err("the parent's recv has no sender yet");
    assert!(
        e.message.contains("recv on an empty channel: deadlock"),
        "{}",
        e.message
    );
}

/// TICKET-235 — a fault inside a one-line native spawn names the user's `spawn` line, in the
/// fault position and in the one trace frame the entry thunk adds. Before the fix the trace was
/// empty.
#[test]
fn a_fault_in_a_one_line_native_spawn_names_the_spawn_line() {
    let src = "fn main():\n    ch := Channel[int](1)\n    ch.close()\n    parallel:\n        spawn ch.send(1)\n    print(\"ok\")\nmain()\n";
    let entry = write_temp_chz("t235_trace", src);
    let (_out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    let e = res.expect_err("expected a fault");
    assert!(
        e.message.contains("send on a closed channel"),
        "got: {}",
        e.message
    );
    assert_eq!(e.span.line, 5);
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, ["<spawned task>"]);
    assert_eq!(e.trace[0].span.line, 5);
}

/// TICKET-219, moved from `tests/chz` by TICKET-243: the std internals `_settle` and
/// `_submit_settled` seal only a channel; any other value faults. No user program can name them
/// (a `_` name of a std module is hidden), so the two guards are driven from Rust.
#[test]
fn the_settle_internals_reject_a_non_channel() {
    use crate::native::Host;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let mut host = VmHost {
        vm: &mut vm,
        args: vec![Value::int(5), Value::int(1)],
    };
    let e = host
        .arg_settle_channel(0, 1)
        .expect_err("_settle(5, 1) must fault");
    assert!(e.message.contains("Channel"), "{}", e.message);
    let h = vm.alloc_str("x".to_string()).as_obj().unwrap();
    let e = vm
        .executor_method(
            h,
            "_submit_settled",
            &[Value::nil(), Value::int(5)],
            Span::RUNTIME,
        )
        .expect_err("_submit_settled with a non-channel must fault");
    assert!(e.message.contains("requires a Channel"), "{}", e.message);
}

/// TICKET-246 — the copy walk reads a heap it only borrows: `WalkSrc` holds `&Heap` and the
/// module table of ANOTHER `Vm`, and the value decodes into this one.
#[test]
fn walk_src_copies_out_of_a_heap_it_only_borrows() {
    let mut owner = Vm::new(Arc::new(empty_program()));
    let s = owner.heap.alloc(Obj::Str("s".into()));
    let inner = owner
        .heap
        .alloc(Obj::List(vec![Value::int(1), Value::int(2)]));
    let outer = owner
        .heap
        .alloc(Obj::List(vec![Value::obj(inner), Value::obj(s)]));
    let src = super::airlock_out::WalkSrc {
        heap: &owner.heap,
        module_objs: &owner.module_objs,
        active_generators: &owner.active_generators,
        program: &owner.program,
        walk_base: 0,
        origin: owner.heap.id(),
    };
    let wire = src
        .to_wire_crossable_memo(
            Value::obj(outer),
            Span::default(),
            &mut super::airlock_out::WireMemo::default(),
        )
        .expect("plain data crosses");
    let mut reader = Vm::new(Arc::new(empty_program()));
    assert_ne!(reader.heap.id(), owner.heap.id());
    let copy = reader.from_wire(wire);
    assert_eq!(reader.display(copy), "[[1, 2], 's']");
}

/// TICKET-246 — `with_box` undoes its swap on unwind, not only on return: after a Rust panic
/// inside the closure the `Vm` runs on the caller's heap again, the box context is back in its
/// slot and no caller frame is left on the chain. The second half panics two boxes deep, inside
/// `as_box_caller`.
#[test]
fn with_box_restores_the_context_when_the_closure_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"caller");
    let mut outer = Some(vm.box_new());
    let mut inner = Some(vm.box_new());
    let (heap_id, stack_len) = (vm.heap.id(), vm.stack.len());

    let seen_inside = std::cell::Cell::new(0);
    let r = catch_unwind(AssertUnwindSafe(|| {
        vm.with_box(&mut outer, |vm| {
            seen_inside.set(vm.heap.id());
            assert_eq!(vm.out, b"caller", "out stays the caller's inside a box");
            vm.stack.push(Value::int(7));
            panic!("boom");
        })
    }));
    assert!(r.is_err());
    assert_ne!(
        seen_inside.get(),
        heap_id,
        "the closure ran on the box heap"
    );
    assert_eq!(vm.heap.id(), heap_id);
    assert_eq!(vm.stack.len(), stack_len);
    assert_eq!(vm.out, b"caller");
    assert!(vm.box_chain.is_empty());
    assert!(outer.is_some());

    let r = catch_unwind(AssertUnwindSafe(|| {
        vm.with_box(&mut outer, |vm| {
            vm.with_box(&mut inner, |vm| vm.as_box_caller(|_| panic!("deep")))
        })
    }));
    assert!(r.is_err());
    assert_eq!(vm.heap.id(), heap_id);
    assert_eq!(vm.stack.len(), stack_len);
    assert_eq!(vm.out, b"caller");
    assert!(vm.box_chain.is_empty());
    assert!(outer.is_some() && inner.is_some());
    // The box contexts still work after both unwinds.
    assert_eq!(vm.with_box(&mut outer, |vm| vm.stack.len()), 1);
}

/// TICKET-246 (F1) — every renderer terminates on every box cycle, with the container cycle
/// marker. Grid: renderer {`Vm::display`, `Vm::display_wire`, `stringify`} x box {Shared,
/// RwShared, Atomic} x shape {no cycle, holds itself, through a `List`, through a struct, two
/// boxes holding each other}. Each cell runs on its own thread under a ten-second wait, so a
/// renderer that recurses while it holds a payload lock fails its cell instead of hanging the run.
#[test]
fn display_box_grid_terminates_on_every_cycle() {
    use std::sync::mpsc;
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Shared,
        RwShared,
        Atomic,
    }
    #[derive(Clone, Copy, Debug)]
    enum Shape {
        NoCycle,
        Itself,
        ThroughList,
        ThroughStruct,
        EachOther,
    }
    #[derive(Clone, Copy, Debug)]
    enum Renderer {
        Display,
        DisplayWire,
        Stringify,
    }
    // A box of `kind` as a wire handle, plus the way to set what it holds.
    fn new_box(vm: &mut Vm, kind: Kind) -> WireValue {
        match kind {
            Kind::Shared => WireValue::Shared(vm.new_shared_core(WireValue::Nil)),
            Kind::RwShared => WireValue::RwShared(vm.new_rwshared_core(WireValue::Nil)),
            Kind::Atomic => WireValue::Atomic(Arc::new(AtomicCore::default())),
        }
    }
    fn set(vm: &mut Vm, b: &WireValue, held: WireValue) {
        match b {
            WireValue::Shared(c) => {
                let mut st = c.state.lock();
                vm.box_store(&mut st, held);
            }
            WireValue::RwShared(c) => {
                let mut st = c.state.write();
                vm.box_store(&mut st, held);
            }
            WireValue::Atomic(c) => *c.v.lock().unwrap() = held,
            _ => unreachable!(),
        }
    }
    fn cell(kind: Kind, shape: Shape, renderer: Renderer) -> String {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let b = new_box(&mut vm, kind);
        match shape {
            Shape::NoCycle => set(&mut vm, &b, WireValue::Int(1)),
            Shape::Itself => set(&mut vm, &b, b.clone()),
            Shape::ThroughList => set(
                &mut vm,
                &b,
                WireValue::List {
                    id: 0,
                    items: vec![b.clone()],
                },
            ),
            Shape::ThroughStruct => set(
                &mut vm,
                &b,
                WireValue::Struct {
                    id: 0,
                    name: "N".into(),
                    fields: vec![("b".into(), b.clone())],
                },
            ),
            Shape::EachOther => {
                let other = new_box(&mut vm, kind);
                set(&mut vm, &other, b.clone());
                set(&mut vm, &b, other);
            }
        }
        match renderer {
            Renderer::DisplayWire => vm.display_wire(&b),
            Renderer::Display => {
                let v = vm.from_wire(b);
                vm.display(v)
            }
            Renderer::Stringify => {
                let v = vm.from_wire(b);
                vm.stringify(v, Span::default(), 0).expect("renders")
            }
        }
    }
    for kind in [Kind::Shared, Kind::RwShared, Kind::Atomic] {
        let l = format!("{kind:?}");
        for (shape, want) in [
            (Shape::NoCycle, format!("{l}(1)")),
            (Shape::Itself, format!("{l}({l}(...))")),
            (Shape::ThroughList, format!("{l}([{l}(...)])")),
            // A `Shared` / `RwShared` decodes what it holds into its own heap, and this fixture's
            // empty program declares no struct `N`, so the struct prints with no name. The cell's
            // subject is the cycle marker; `tests/chz/suites/box_grid_test.chz` runs the same
            // shape with a declared struct.
            (
                Shape::ThroughStruct,
                match kind {
                    Kind::Atomic => format!("{l}(N(b={l}(...)))"),
                    _ => format!("{l}(?(0={l}(...)))"),
                },
            ),
            (Shape::EachOther, format!("{l}({l}({l}(...)))")),
        ] {
            for renderer in [
                Renderer::Display,
                Renderer::DisplayWire,
                Renderer::Stringify,
            ] {
                let (tx, rx) = mpsc::channel();
                std::thread::Builder::new()
                    .stack_size(VM_STACK_BYTES)
                    .spawn(move || {
                        let _ = tx.send(cell(kind, shape, renderer));
                    })
                    .unwrap();
                let got = rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("{kind:?} x {shape:?} x {renderer:?}: no render"));
                assert_eq!(got, want, "{kind:?} x {shape:?} x {renderer:?}");
            }
        }
    }
    // Shape "state lock held by another thread" (a writer sits in a slow closure): the `&self`
    // renderer builds fault messages and must not wait, so it shows `<locked>`.
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let mut vm = Vm::new(Arc::new(empty_program()));
            let core = vm.new_shared_core(WireValue::Int(1));
            let (held_tx, held_rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel::<()>();
            let holder_core = Arc::clone(&core);
            let holder = std::thread::spawn(move || {
                let _st = holder_core.state.lock();
                held_tx.send(()).unwrap();
                let _ = done_rx.recv();
            });
            held_rx.recv().unwrap();
            let h = vm.heap.alloc(Obj::Shared(Arc::clone(&core)));
            let shown = vm.display(Value::obj(h));
            done_tx.send(()).unwrap();
            holder.join().unwrap();
            let _ = tx.send((shown, vm.display(Value::obj(h))));
        })
        .unwrap();
    let (busy, free) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("Vm::display waited for a held Shared state");
    assert_eq!(busy, "Shared(<locked>)");
    assert_eq!(free, "Shared(1)");
    // The same shape for `RwShared`: a writer holds the write lock. A reader beside it (the read
    // lock held by another thread) does NOT block the render: readers share.
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let mut vm = Vm::new(Arc::new(empty_program()));
            let core = vm.new_rwshared_core(WireValue::Int(1));
            let h = vm.heap.alloc(Obj::RwShared(Arc::clone(&core)));
            let mut shown = Vec::new();
            for write in [true, false] {
                let (held_tx, held_rx) = mpsc::channel();
                let (done_tx, done_rx) = mpsc::channel::<()>();
                let holder_core = Arc::clone(&core);
                let holder = std::thread::spawn(move || {
                    let _w = write.then(|| holder_core.state.write());
                    let _r = (!write).then(|| holder_core.state.read());
                    held_tx.send(()).unwrap();
                    let _ = done_rx.recv();
                });
                held_rx.recv().unwrap();
                shown.push(vm.display(Value::obj(h)));
                done_tx.send(()).unwrap();
                holder.join().unwrap();
            }
            shown.push(vm.display(Value::obj(h)));
            let _ = tx.send(shown);
        })
        .unwrap();
    let shown = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("Vm::display waited for a held RwShared state");
    assert_eq!(shown, ["RwShared(<locked>)", "RwShared(1)", "RwShared(1)"]);
}

/// TICKET-248 — a missing-method fault and an operand fault name a carrier value by its kind in
/// plain words and by the way the user writes its type, never `enum`, `Option` or `Result`.
/// `run_capture` skips the checker; no checker-clean program reaches the two method-miss sites.
#[test]
fn a_method_miss_names_a_carrier_as_the_user_writes_it() {
    const OPT: &str = "an optional value (`T?`)";
    const RES: &str = "a result value (`T!E`)";
    let cells = [
        (
            "o: int? = ?5\nprint(o.foo())\n",
            format!("{OPT} has no method 'foo'"),
        ),
        (
            "o: int? = None\nprint(o.foo())\n",
            format!("{OPT} has no method 'foo'"),
        ),
        (
            "o: int!str = ?5\nprint(o.foo())\n",
            format!("{RES} has no method 'foo'"),
        ),
        (
            "o: int!str = !\"e\"\nprint(o.foo())\n",
            format!("{RES} has no method 'foo'"),
        ),
        (
            "o: int? = ?5\nprint(o + o)\n",
            format!("{OPT} has no 'add' method"),
        ),
        (
            "o: int!str = ?5\nprint(o + o)\n",
            format!("{RES} has no 'add' method"),
        ),
        (
            "o: int? = ?5\nprint(o + 1)\n",
            format!("cannot apply Add to {OPT} and int"),
        ),
    ];
    for (src, expect) in cells {
        let got = match run_capture(src) {
            Ok(o) => format!("ran: {o:?}"),
            Err(e) => e.message,
        };
        assert_eq!(got, expect, "src: {src}");
    }
}

/// TICKET-252: a hole's text is read once, as written. Expected values are CPython 3.12+ f-string output.
#[test]
fn interpolation_hole_text_is_read_once() {
    let cells: Vec<(&str, &str)> = vec![
        (
            "a := \"{'\\\\n'}\"\nprint(a.len(), a == \"\\\\n\")\n",
            "2 true\n",
        ),
        (
            "line := \"a\\\\nb\"\nprint(\"{line.replace('\\\\n', ' ')}\")\n",
            "a b\n",
        ),
        ("x := 5\nprint(\"\\u{7B}x\\u{7D}\")\n", "{x}\n"),
        ("x := 5\nprint(\"\"\"{x +\n 1}\"\"\")\n", "6\n"),
    ];
    let run = |src: &str| match run_capture(src) {
        Ok(o) => o,
        Err(e) => e.message,
    };
    for (src, expect) in cells {
        assert_eq!(run(src), expect, "src: {src}");
    }

    // Every expected value below is CPython 3.14.7 f-string output (python3 run on the equivalent
    // f-string, the same cell in all four literal kinds). Deliberate differences, each on its row:
    // DEC-018 (`a{4}b`), every literal interpolates (`'}}'`), a Chezzi `str` has no `\x` escape, and
    // a hole is code, so a backslash there is an error.
    //
    // In a hole template `¤` is the quote other than the literal's own, `§` is the literal's own
    // quote and `↵` is a real line break. `¤` in an expected value is the other quote too.
    const PRELUDE: &str = "x := 5\nw := 4\nc := true\nd := {\"k\": 7}\nxs := [\"a\", \"b\"]\n";
    // (open, close, the other quote)
    const KINDS: [(&str, &str, &str); 4] = [
        ("\"", "\"", "'"),
        ("'", "'", "\""),
        ("\"\"\"", "\"\"\"", "'"),
        ("'''", "'''", "\""),
    ];

    // HOLE axis x KIND axis: content, layout and spec of one hole.
    let holes: Vec<(&str, &str)> = vec![
        (r#"x"#, "5"),
        (r#"¤\\n¤.len()"#, "2"),
        (r#"¤\n¤.len()"#, "1"),
        (r#"¤\\\\¤.len()"#, "2"),
        (r#"¤a\\tb¤.len()"#, "4"),
        (r#"¤it\¤s¤"#, "it¤s"),
        (r#"¤\u{41}¤"#, "A"),
        (r#"r¤\\d¤.len()"#, "3"),
        (r#"r¤\d¤.len()"#, "2"),
        (r#"b¤\\x41¤"#, "b'\\\\x41'"),
        // A Chezzi `str` has no `\x`; a `b` literal has it, so CPython's `b'A'` holds here.
        (r#"b¤\x41¤"#, "b'A'"),
        (r#"d[§k§]"#, "7"),
        (r#"§{x}§"#, "5"),
        (r#"¤#¤"#, "#"),
        (r#"¤:¤"#, ":"),
        // DELIBERATE: every Chezzi literal interpolates, a nested one included, so it writes `}}`
        // where CPython writes `'}'`.
        (r#"¤}}¤"#, "}"),
        (r#" {¤a¤: 1}[¤a¤] "#, "1"),
        (r#"[y * 2 for y in [1, 2]]"#, "[2, 4]"),
        (r#"xs[0..1].len()"#, "1"),
        (r#"(if c: 1 else: 2)"#, "1"),
        (r#"(fn(): 1)()"#, "1"),
        (r#"x + 1"#, "6"),
        (r#"x +↵ 1"#, "6"),
        (r#"x↵ + 1"#, "6"),
        (r#"(x +↵ 1)"#, "6"),
        (r#"x + 1 # c↵"#, "6"),
        (r#"xs↵ .len()↵ .str()"#, "2"),
        (r#"x:>4"#, "   5"),
        (r#"x:>{w}"#, "   5"),
        (r#"x:\u{3E}4"#, "   5"),
        (r#"x:\u{7B}>4"#, "{{{5"),
        (r#"x:¤>4"#, "¤¤¤5"),
        (r#"d[§k§]:>{w}"#, "   7"),
    ];
    for (open, close, other) in KINDS {
        let own = &open[..1];
        for (hole, expect) in &holes {
            let hole = hole
                .replace('¤', other)
                .replace('§', own)
                .replace('↵', "\n");
            let expect = expect.replace('¤', other);
            let src = format!("{PRELUDE}print({open}{{{hole}}}{close})\n");
            assert_eq!(run(&src), format!("{expect}\n"), "src: {src}");
        }
    }

    // OUTER axis: an escaped special in the literal's own text is text, not syntax.
    let outers: Vec<(&str, &str)> = vec![
        (r#"\u{7B}x\u{7D}"#, "{x}"),
        (r#"a\u{7B}{x}\u{7D}b"#, "a{5}b"),
        (r#"a\u{7D}"#, "a}"),
        (r#"\u{7B}"#, "{"),
        (r#"\\{x}"#, "\\5"),
        (r#"{{x}} {{{x}}}"#, "{x} {5}"),
        // DELIBERATE (DEC-018): a hole of ASCII digits is text. CPython prints `a4b`.
        (r#"a{4}b"#, "a{4}b"),
    ];
    for (open, close, _) in KINDS {
        for (text, expect) in &outers {
            let src = format!("{PRELUDE}print({open}{text}{close})\n");
            assert_eq!(run(&src), format!("{expect}\n"), "src: {src}");
        }
    }

    // KIND axis: raw and bytes literals do not interpolate.
    assert_eq!(run(&format!("{PRELUDE}print(r\"{{x}}\")\n")), "{x}\n");
    assert_eq!(run(&format!("{PRELUDE}print(b\"{{x}}\")\n")), "b'{x}'\n");

    // ESCAPE axis: each escape resolves once. CPython 3.14.7: `len('\L') == 1`, `'\L' == "\L"` and
    // `len('\\L') == 2` for the seven letters.
    for (letter, _) in crate::lexer::ESCAPES {
        let escaped = format!("'\\{letter}'");
        let written = if letter == '\\' || letter == '\'' {
            format!("'\\\\\\{letter}'")
        } else {
            format!("'\\\\{letter}'")
        };
        let src = format!("print(\"{{{escaped}.len()}}\", {escaped} == \"\\{letter}\")\n");
        assert_eq!(run(&src), "1 true\n", "src: {src}");
        let src = format!("print(\"{{{written}.len()}}\")\n");
        assert_eq!(run(&src), "2\n", "src: {src}");
    }

    // ERROR axis: (literal, a substring of the message). CPython rejects each one except the two
    // marked: `'}'` (DELIBERATE, see above) and `\x41` (a Chezzi `str` has no `\x`).
    let errors: Vec<(&str, &str)> = vec![
        (r##""{d[\"k\"]}""##, "backslash"),
        (r##""{x\t+ 1}""##, "backslash"),
        (r##""{x\u{3A}>4}""##, "backslash"),
        (r##""{x \u{23} tail}""##, "backslash"),
        (r##""{if c: 1 else: 2}""##, "parentheses"),
        (r##""{1 + if c: 1 else: 2}""##, "parentheses"),
        (r##""{fn(): 1}""##, "parentheses"),
        (r##""{x # c}""##, "'#' starts a comment"),
        (r##""a{x""##, "unterminated '{'"),
        (
            r##""{x:{w:3}}""##,
            "nested field cannot carry its own format spec",
        ),
        (r##""{x)}""##, "unmatched"),
        (r##""{'}'}""##, "unmatched '}'"),
        (r##""{'\x41'}""##, "unknown escape"),
    ];
    for (lit, expect) in errors {
        let src = format!("{PRELUDE}print({lit})\n");
        match run_capture(&src) {
            Ok(o) => panic!("src: {src} ran: {o:?}"),
            Err(e) => assert!(
                e.message.contains(expect),
                "src: {src} message: {}",
                e.message
            ),
        }
    }
}
