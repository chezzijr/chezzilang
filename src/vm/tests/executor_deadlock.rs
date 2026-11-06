//! VM tests: Executor shutdown, stuck jobs, deadlock verdicts, eager handshakes, `wait` timers.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// `cancel` is "attempts to stop" (decision D4) — COOPERATIVE, not preemptive, exactly like
/// Java's `shutdownNow`. The M:N engine starts the job at its `submit`, so by the time
/// `cancel` trips the cancel flag the job may already have run to completion — a job with no
/// cancellation point in it (this one is a single `print`) cannot be stopped at all. So the only
/// thing assertable is that `cancel` returns promptly and the program finishes. `cancel` does not
/// wait (TICKET-289), so a job that ran on prints after `main`'s `0`.
///
/// Asserting the result as a SET rather than a sequence is what keeps this honest: pinning it to
/// either outcome would encode a race as a guarantee.
#[test]
fn executor_cancel_is_cooperative_not_preemptive() {
    let src = "import Executor from std.concurrency\nfn j():\n    print(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    ex.cancel()\n    print(0)\nmain()\n";
    let mn = run_capture(src).expect("mn run");
    assert!(
        mn == "0\n" || mn == "99\n0\n" || mn == "0\n99\n",
        "cancel must either stop the job or let it finish, never anything else; got {mn:?}"
    );
}

/// EAGER execution, the headline regression guard. A job that blocks on an empty `recv` must WAIT for
/// a value its submitter sends later — it must not declare a deadlock.
///
/// This is the exact program the first (rejected) attempt at eager execution broke: it faulted
/// `recv on an empty channel: deadlock — no runnable task can send`. That verdict is true only while
/// jobs run at the drain, where the submitter really is stuck inside `shutdown()`; once a job starts
/// at its `submit` the submitter is still running and may send on the very next line, so the fault is
/// a lie.
///
/// The `ready` handshake is what gets the job to the blocking `recv` FIRST — without it the main
/// thread usually wins the race, queues the value, and the empty-`recv` path is never exercised at
/// all (that is why the CLI repro for this looked green until a sleep was added; `std.time` is not
/// resolvable through the single-module test helper, so a handshake stands in for the sleep). If main
/// wins anyway the assertion still holds, so it costs coverage in the rare case, never a flake.
///
/// The handshake leg depends on EAGER dispatch: the job must already be running when top-level
/// `main` reaches `ready.recv()`. Under the lazy queue-at-`submit` model (decision D3, which the
/// since-removed cooperative engine used) the same program is a genuine deadlock. The second program
/// drops the handshake and pins the same end state without depending on dispatch timing at all.
///
/// Watchdogged because the failure mode of getting this wrong the OTHER way (never waking) is a hang.
#[test]
fn executor_job_blocking_recv_waits_for_a_later_send() {
    // Eager-only: the job reaches its blocking `recv` before the value is sent.
    let handshake = r#"
import Atomic, Executor from std.concurrency
ch := Channel[int](1)
ready := Atomic(0)
fn worker():
    ready.add(1)
    print("job got {ch.recv()}")
ex := Executor()
ex.submit(worker)
spins := 0
while ready.load() == 0:
    spins = spins + 1
ch.send(7)
ex.shutdown()
print("done")
"#;
    // Engine-agnostic: whenever the job reaches the `recv`, the value is there or it waits for it.
    let plain = r#"
import Executor from std.concurrency
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
ch.send(7)
ex.shutdown()
print("done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((
            run_capture(handshake),
            run_capture(plain),
            run_capture(plain),
        ));
    });
    let (mn_handshake, serial_plain, mn_plain) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a blocking job must wake on the send, not hang");
    let want = "job got 7\ndone\n";
    assert_eq!(mn_handshake.expect("mn handshake run"), want);
    assert_eq!(serial_plain.expect("run"), want);
    assert_eq!(mn_plain.expect("mn run"), want);
}

/// W7-12 — the MIRROR of the test above: a job blocked on a channel that only its own joiner could
/// have filled must FAULT, not hang. `main` sends only after `shutdown()`, so it is stuck waiting for
/// the job while the job waits for it — a real deadlock, and the caller's mistake.
///
/// Eager execution (§2c) regressed this from "faults in 0s" to "hangs forever on M:N" (the
/// since-removed cooperative engine still faulted, which is how it was caught): the `recv` arm
/// decides by "am I inside a scheduler?", which cannot tell this
/// program apart from the one above. The fix asks the one question that can — is this executor already
/// being JOINED, with every job it still owes parked — so the two programs keep their opposite verdicts.
///
/// The M:N fault travels out of a worker `Vm` through `reduce_task_slots` rather than an inline
/// drain, so the message and exit status are byte-identical to the pre-eager behaviour — a real
/// claim, not a formality, and the golden below is what holds it.
///
/// Watchdogged: getting this wrong is a hang, which would otherwise stall the suite rather than fail it.
#[test]
fn executor_job_blocked_during_shutdown_faults_both_engines() {
    let src = r#"
import Executor from std.concurrency
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
ex.shutdown()
ch.send(42)
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a job blocked on its own joiner must fault, not hang");
    let serial = err_text(&serial.expect_err("serial: this program is a deadlock"));
    let mn = err_text(&mn.expect_err("M:N: this program is a deadlock"));
    assert!(
        serial.contains("recv() on an empty channel"),
        "serial fault was: {serial}"
    );
    assert_eq!(serial, mn, "the deadlock verdict must be engine-identical");
}

/// `W7-12r` residual (a), CLOSED by the process-wide verdict (`future.md` §2d step 0): TWO jobs of one
/// executor both blocked on an empty channel, with `main` inside `shutdown()`, must fault.
///
/// W7-12's per-executor predicate could only judge `outstanding == 1` — anything with a sibling job
/// was declined, because no counter can tell a parked cap-1 handshake from an unfeedable one — so this
/// program, the commonest accidental executor deadlock there is, hung forever on M:N. Go reports it
/// (`fatal error: all goroutines are asleep - deadlock!`, measured, rc=2, two goroutines on one empty
/// channel behind a `WaitGroup`); CPython hangs. We now match Go.
///
/// Depends on EAGER dispatch: under the lazy queue-at-`submit` model (decision D3) this program
/// faults for an unrelated reason, so it would be no evidence about anything this change touches.
///
/// A blocked eager job now hands its pool slot to a replacement (TICKET-052), so this shape no
/// longer needs a second free pool thread — the second job dispatches at `--threads=1` too.
///
/// Watchdogged — the failure mode is a hang, which would stall the suite rather than fail it.
#[test]
fn two_blocked_jobs_in_one_executor_fault_instead_of_hanging() {
    let src = r#"
import Executor from std.concurrency
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("j1 {ch.recv()}"))
ex.submit(fn(): print("j2 {ch.recv()}"))
ex.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let mn = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("two jobs deadlocked in one executor must fault, not hang");
    let msg = err_text(&mn.expect_err("both jobs wait on a channel nobody can fill — a deadlock"));
    assert!(
        msg.contains("recv() on an empty channel"),
        "fault was: {msg}"
    );
}

/// `W7-12r` residual (b), CLOSED: two executors deadlocking EACH OTHER must fault.
///
/// W7-12's predicate swept the executor registry and went silent while any other executor still owed
/// work — necessary then, because "y still owes a job" was the only available proxy for "y might yet
/// send", and over-ruling it faulted a program both ancestors complete (kept green by
/// `executor_job_keeps_waiting_while_another_executor_still_owes_work`). The process-wide verdict does
/// not need the proxy: it asks whether y's job is itself BLOCKED with nothing satisfiable, so a live
/// producer in y still vetoes while a deadlocked one does not. Go reports this program (measured,
/// rc=2); CPython hangs.
///
/// M:N-only, for the same D3 reason as the test above. A blocked eager job now hands its pool slot
/// to a replacement (TICKET-052), so this shape no longer needs a second free pool thread.
/// Watchdogged.
#[test]
fn two_executors_deadlocking_each_other_fault() {
    let src = r#"
import Executor from std.concurrency
c1 := Channel[int](1)
c2 := Channel[int](1)
x := Executor()
y := Executor()
x.submit(fn(): c1.send(c2.recv()))
y.submit(fn(): c2.send(c1.recv()))
x.shutdown()
y.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let mn = rx.recv_timeout(std::time::Duration::from_secs(60)).expect(
        "two mutually-deadlocked executors must fault, not hang — or this host has <2 free pool \
             threads (see the sibling test above)",
    );
    let msg = err_text(&mn.expect_err("each executor's job waits for the other's — a deadlock"));
    assert!(
        msg.contains("recv() on an empty channel"),
        "fault was: {msg}"
    );
}

/// TICKET-208 — a deadlocked job is reported at its OWN blocking op, with the text the in-place
/// path raises for that op: line 4 (the `recv`), not line 3 (the `Executor()`), and never the
/// nursery text. Watchdogged like its neighbours.
#[test]
fn a_deadlocked_job_is_reported_at_its_own_recv_not_at_the_executor() {
    let src = "import Executor from std.concurrency\nch := Channel[int](1)\nex := Executor()\nex.submit(fn(): print(ch.recv()))\nex.shutdown()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let res = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a job parked on a channel nobody feeds must fault, not hang");
    let err = res.expect_err("the job waits on a channel nobody sends to — a deadlock");
    assert!(
        err_text(&err).contains("recv() on an empty channel"),
        "fault was: {}",
        err_text(&err)
    );
    assert_eq!(
        err.span.line, 4,
        "the report must name the job's `recv`, got {:?}",
        err.span
    );
}

/// `W7-12r` residual (c), CLOSED: a blocked job with NO explicit `shutdown()` must fault at the
/// program-exit drain rather than hang there.
///
/// W7-12's `JoinGuard` was armed at the `shutdown()` call site and deliberately NOT at the exit drain,
/// because the drain joins every live executor one at a time and a per-executor verdict would have let
/// REGISTRY ORDER decide whose job faulted. A process-wide verdict has no such ordering problem, so
/// `join_executor` registers its party for every join, this one included.
///
/// Neither ancestor faults here, and that is stated rather than glossed: Go returns from `main` and
/// ABANDONS the goroutine (measured, rc=0), CPython's `ThreadPoolExecutor` joins its non-daemon
/// threads at exit and hangs (measured, rc=124). Chezzi joins at exit (decision D1 — CPython's model),
/// so with CPython's join and Go's verdict rule the answer here is stricter than both, deliberately.
///
/// M:N-only (D3). Watchdogged.
#[test]
fn blocked_job_with_no_shutdown_faults_at_the_exit_drain() {
    let src = r#"
import Executor from std.concurrency
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let mn = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit drain must fault on a deadlocked job, not hang");
    let msg = err_text(&mn.expect_err("the job waits on a channel nobody can fill — a deadlock"));
    assert!(
        msg.contains("recv() on an empty channel"),
        "fault was: {msg}"
    );
}

/// The WRONG ANSWER the process-wide verdict also fixes, found while measuring `W7-12r`: `main`
/// blocking on a channel an eager `Executor` job is about to fill used to FAULT.
///
/// ```text
/// ex.submit(fn(): ch.send(42))
/// print("main got {ch.recv()}")     # ← used to be: recv on an empty channel: deadlock
/// ```
///
/// Both ancestors print `main got 42` (measured: CPython `ThreadPoolExecutor` + `queue.Queue(1)`, and
/// Go with a goroutine over a buffered channel). Chezzi faulted, on BOTH engines. The cause is the
/// stale premise `future.md` §2d names: `chan_recv_step`'s "I have no scheduler ⇒ nobody can ever
/// send" `else` arm, which stopped being true the moment eager execution put running jobs outside
/// every scheduler. `main` now blocks there like any other counted party, and the verdict declines
/// while the job is live.
///
/// Depends on EAGER dispatch: under the lazy queue-at-`submit` model (D3) the job cannot run before
/// `main`'s `recv`, so the program faults there for an unrelated reason. The since-removed
/// cooperative engine did exactly that, and the divergence was recorded rather than defended — an
/// engine is never the standard of correctness, the ancestor is.
#[test]
fn main_recv_completes_when_an_eager_job_sends() {
    let src = r#"
import Executor from std.concurrency
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): ch.send(42))
print("main got {ch.recv()}")
ex.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("main must receive the job's value, not hang")
        .expect("this program has no deadlock — both ancestors run it");
    assert_eq!(out, "main got 42\n");
}

/// D2 — a top-level `wait:` with a CLOSED arm takes that arm (it binds `!RecvError`), as Go's
/// `select` picks `<-closed`; it neither faults nor hangs.
///
/// History: before D2 the poll SKIPPED a closed+empty recv arm (W7-13r(a)), and folding it into
/// `PartyWait` as "satisfiable" vetoed the deadlock verdict forever — this program HUNG (rc 124).
/// Now closed IS ready, so the arm is taken. The deadlock half of that fence is
/// [`a_top_level_wait_on_open_empty_arms_still_reports_the_deadlock`].
#[test]
fn a_top_level_wait_with_a_closed_arm_takes_it() {
    let src = r#"
c1 := Channel[int](1)
c2 := Channel[int](1)
c1.close()
wait:
    v := c1.recv():
        match v:
            ?x: print("c1 {x}")
            !e: print("c1 {e.message()}")
    w := c2.recv():
        print("c2 {w}")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a closed arm is ready: the wait must take it, not hang")
        .expect("a closed arm is ready: no deadlock");
    assert_eq!(out, "c1 receiving on a closed channel\n");
}

/// The deadlock fence kept from the HANG regression above, in the same top-level context: a `wait:`
/// whose arms are all OPEN, empty and unfeedable (nothing else runs) must still reach its deadlock
/// fault — counting closed arms as ready must not make an open one look satisfiable.
#[test]
fn a_top_level_wait_on_open_empty_arms_still_reports_the_deadlock() {
    let src = r#"
c1 := Channel[int](1)
c2 := Channel[int](1)
wait:
    v := c1.recv():
        print("c1 {v}")
    w := c2.recv():
        print("c2 {w}")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    let msg = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("an all-open, unfeedable wait must reach the deadlock verdict, not hang")
        .expect_err("nothing can feed c1 or c2 — this is a deadlock");
    let msg = err_text(&msg);
    assert!(
        msg.contains("wait: on channels that are all empty"),
        "fault was: {msg}"
    );
}

/// The process-wide verdict's FALSE-FAULT regression, also caught by adversarial review: an
/// already-drained `shutdown()` must not be read as a blocked joiner.
///
/// `join_executor` registers its `PartyWait::Join` before it can take the executor lock, so a join
/// with nothing outstanding — `Executor(); e.shutdown()`, and the whole window while the last job's
/// `finish` wakes a real joiner — briefly puts a party in the registry for a thread that is about to
/// return and keep running. While `Join` answered a flat "never satisfiable", a sibling sampling in
/// that window faulted a LIVE program: measured 2/20 runs on this shape, where Go and CPython both
/// print `got 1`. A join's wait condition is exactly `outstanding() == 0`, so that is what it answers.
///
/// The loop is the amplifier — one drained shutdown is a nanosecond-wide window; 20 000 of them beside
/// a genuinely blocked consumer hit it reliably enough to have shown 2/20, and the whole program still
/// runs in ~16 ms. Repeated here for the same reason.
#[test]
fn a_drained_shutdown_is_not_mistaken_for_a_blocked_joiner() {
    let src = r#"
import Executor from std.concurrency
c := Channel[int](1)
exA := Executor()
exA.submit(fn(): print("got {c.recv()}"))
i := 0
while i < 20000:
    e := Executor()
    e.shutdown()
    i = i + 1
c.send(1)
exA.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..15 {
            match run_capture(src) {
                Ok(o) if o == "got 1\n" => {}
                other => bad.push(format!("{other:?}")),
            }
        }
        let _ = tx.send(bad);
    });
    let bad = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a drained shutdown must not hang either");
    assert!(
        bad.is_empty(),
        "a live program was reported deadlocked in {}/15 runs: {bad:#?}",
        bad.len()
    );
}

/// W7-12's BOUNDARY, and the reason its join predicate asks who is joining rather than just whether
/// anyone is: a `shutdown()` running inside a nursery task says nothing about whether a value can
/// still arrive, because a SIBLING task can be the producer. Here `spawn: ex.shutdown()` runs while
/// `spawn: … ch.send(42)` is one handshake away from sending, so the job must still WAIT.
///
/// Caught by review, not by the suite: the first cut armed the predicate at every explicit
/// `shutdown()`, which made this exact program fault while the since-removed cooperative engine
/// printed `job got 42` — re-opening, in a new place, precisely the divergence W7-12 exists to close.
/// `job got 42` is the answer CPython's `ThreadPoolExecutor` gives, and it is what this pins.
///
/// Run through the FILE helpers, not `run_capture`, for one reason: the producer has to be slower
/// than the verdict's own debounce (2 × `DEMOTE_POLL_BACKOFF`) or the send lands first and the program
/// would pass even with the bug present — verified by mutation. That needs a real `timer`, and
/// `std.time` only resolves through a module graph.
#[test]
fn executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer() {
    let src = "
import Executor from std.concurrency
import timer from std.time
ch: Channel[int] = Channel[int](1)
ex := Executor()
ex.submit(fn(): print(\"job got {ch.recv()}\"))
parallel:
    spawn:
        timer(200).recv()
        ch.send(42)
    spawn:
        ex.shutdown()
print(\"end\")
";
    let entry = write_temp_chz("w712_sibling_producer", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (out, _err, res, _code) = run_file(&e);
        let _ = tx.send((out, res));
    });
    let (out, res) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a live sibling producer must keep the job waiting, not hang it");
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "run faulted: {res:?}");
    assert_eq!(out, "job got 42\nend\n", "output");
}

/// W7-12's other boundary, and the one that matters most: `x.shutdown()` says nothing about whether a
/// job of a DIFFERENT executor `y` is about to send. Executors are independent.
///
/// The measure of correct here is the ancestor, not the parity oracle. Python's `ThreadPoolExecutor`
/// runs this program to completion — `x.submit(consumer)`, `y.submit(producer)`, `x.shutdown()` prints
/// `got 1` — so reporting a deadlock is a WRONG ANSWER about a live program, not a tolerable engine
/// difference. The first cut of this fix did exactly that, and was defended with "the other engine
/// faults there too", which is an argument about agreement and not about correctness.
///
/// W7-12's predicate bought that by sweeping the executor registry and going silent while any OTHER
/// executor still owed work — at the cost of the opposite error, two mutually-deadlocked executors
/// HANGING. The process-wide verdict keeps this program correct WITHOUT the proxy: it asks whether
/// y's job is itself blocked with nothing satisfiable, so a live producer vetoes and a deadlocked one
/// does not, and the cost is gone (`two_executors_deadlocking_each_other_fault`).
///
/// Depends on EAGER dispatch for the same reason as its sibling above: under the lazy
/// queue-at-`submit` model (decision D3) x's consumer would not exist until `x.shutdown()` drains it,
/// and the shape would fault regardless of anything W7-12 touches.
#[test]
fn executor_job_keeps_waiting_while_another_executor_still_owes_work() {
    let src = "
import Executor from std.concurrency
import timer from std.time
ch: Channel[int] = Channel[int](1)
fn consumer():
    print(\"got {ch.recv()}\")
fn producer():
    timer(200).recv()
    ch.send(1)
x := Executor()
y := Executor()
x.submit(consumer)
y.submit(producer)
x.shutdown()
y.shutdown()
print(\"end\")
";
    let entry = write_temp_chz("w712_cross_executor", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let _ = tx.send(run_file(&e));
    });
    let (mo, _me, mr, _mc) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a live producer in another executor must keep the job waiting, not hang it");
    let _ = std::fs::remove_file(&entry);
    assert!(
        mr.is_ok(),
        "M:N run faulted where CPython completes: {mr:?}"
    );
    assert_eq!(mo, "got 1\nend\n");
}

/// W7-12's sharpest boundary: **parked is not unfeedable.** A bounded producer/consumer pipeline
/// inside ONE executor spends its whole life with one job parked on a full `send` and the other on a
/// momentarily-empty `recv` — that is the healthy steady state of a cap-1 handshake, and the two jobs
/// feed EACH OTHER.
///
/// The predicate's first form (`blocked >= outstanding`, i.e. "every job this executor owes is
/// parked") read that as a deadlock and faulted `send on a full channel` in 2–7 of 30 runs of this
/// exact program — a NONDETERMINISTIC wrong answer on code Go and CPython run to completion, and the
/// worst outcome available. The two-observation debounce does not help: consecutive 5 ms samples both
/// land in successive parked windows.
///
/// So the verdict is now restricted to `outstanding == 1`, where there is no sibling to hand off with
/// and the counters cannot be misread. This test is the fence, and it LOOPS because one pass proves
/// nothing about a race — the buggy form passed most runs.
///
/// Depends on EAGER dispatch, and not for lack of trying: under the lazy queue-at-`submit` model
/// (decision D3) the producer runs to completion before the consumer exists, so the program faults
/// `send on a full channel` regardless of anything W7-12 touches. A progress-counter variant of the
/// predicate that would have allowed multi-job
/// verdicts was tried and measured to fail this very test (6/40) — see `docs/contributing/gaps.md` W7-12.
#[test]
fn executor_bounded_pipeline_is_not_mistaken_for_a_deadlock() {
    let src = "
import Executor from std.concurrency
a: Channel[int] = Channel[int](1)
fn prod():
    for i in 0..50:
        a.send(i)
    a.close()
fn cons():
    s := 0
    for v in a:
        s = s + v
    print(\"sum {s}\")
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
";
    let entry = write_temp_chz("w712_bounded_pipeline", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..12 {
            let (o, _e2, r, _c) = run_file(&e);
            if r.is_err() || o != "sum 1225\n" {
                bad.push(format!("{r:?} / out={o:?}"));
            }
        }
        let _ = tx.send(bad);
    });
    let bad = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a bounded pipeline must not hang");
    let _ = std::fs::remove_file(&entry);
    assert!(
        bad.is_empty(),
        "a healthy cap-1 handshake was reported as a deadlock in {}/12 runs: {bad:#?}",
        bad.len()
    );
}

/// W7-56 (defect 1, the false verdict) — an eager `Executor` job feeding a channel a `parallel:` task
/// is parked on must WORK, not be pre-emptively declared a deadlock.
///
/// Measured pre-fix on the release binary: `child waiting`, then the nursery deadlock fault, rc=1, at
/// **~7 ms — before `feeder`'s 50 ms sleep had even elapsed**. `MnSched::is_deadlocked`'s five gates
/// model this sched's own fibers only, and an eager job has none: it runs on the shared pool, bumping
/// neither `running`/`runnable` nor `inflight`. Go's equivalent (a goroutine feeding a channel a
/// WaitGroup'd goroutine reads) prints `job sending` / `child got 7` and exits 0.
///
/// **Depends on EAGER dispatch, and no predicate change can or should alter that.** Under a lazy
/// queue-at-submit / drain-at-`shutdown` model (decision D3) `feeder` genuinely cannot run before the
/// join and the program IS a real deadlock. Same precedent as
/// `executor_bounded_pipeline_is_not_mistaken_for_a_deadlock`.
///
/// Watchdogged: an over-corrected veto turns this into a hang, which stalls the suite instead of
/// failing it.
#[test]
fn executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch := Channel[int]()
fn feeder():
    time.sleep_ms(50)
    print(\"job sending\")
    ch.send(7)
fn waiter():
    print(\"child waiting\")
    v := ch.recv()
    print(\"child got {v}\")
ex := Executor()
ex.submit(feeder)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w756_job_feeds_nursery", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("an executor job feeding a parked nursery task must not hang");
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "a live program was faulted: {res:?}");
    assert!(
        out.contains("child got 7") && out.contains("after nursery"),
        "the job's value must reach the parked task: {out:?}"
    );
}

/// W7-56 (defect 2, the LOST WAKEUP) — the same program with an `inflight` sibling holding the
/// predicate vetoed for 2 s, which isolates the wake bug from the verdict bug.
///
/// This is the one that pins R2 independently of the veto. Pre-fix output, measured: `child waiting`
/// → `job sending` **at 50 ms — the send really happened** → `sleeper done` at 2 s → nursery deadlock
/// at 2008 ms, **with the value sitting in the channel queue**. `child got 7` never printed. An eager
/// job's `Vm` gets neither `mn` nor `mn_enlist_sched` (`spawn_worker` sets neither; the only `mn`
/// assignment in the tree is `spawn_shell`'s), so `channel_send_wire` took the no-sched branch and
/// `Vm::wake_on_send` returned immediately on its empty `scheduler_stack` — while the waiter sat in
/// `SchedCore::parked`, drainable only by that sched's `wake_bucket`, whose idle workers `cv.wait`
/// with no timeout. Fixed by the `Vm::sched_registry` walk in `wake_on_send`.
///
/// So: had R1 (the veto) landed alone, this program would have hung silently forever instead of
/// faulting. M:N-only + watchdogged for the reasons on the test above.
#[test]
fn an_executor_jobs_send_wakes_a_task_parked_on_another_scheds_channel() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch := Channel[int]()
fn feeder():
    time.sleep_ms(50)
    ch.send(7)
fn waiter():
    v := ch.recv()
    print(\"child got {v}\")
fn sleeper():
    time.sleep_ms(2000)
ex := Executor()
ex.submit(feeder)
parallel:
    spawn waiter()
    spawn sleeper()
";
    let entry = write_temp_chz("w756_job_send_wakes_park", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the job's send must wake the parked task, not strand it");
    let _ = std::fs::remove_file(&entry);
    // Delivery IS the pin, and it is a clean binary one: with only R1 (the veto) this program behaves
    // exactly as it did pre-fix — `sleeper` holds the predicate vetoed for 2 s, the value lands in the
    // queue at 50 ms and nothing ever wakes the parked waiter, so the run ends in the deadlock fault
    // with `child got 7` unprinted. Only the `wake_on_send` registry walk delivers it.
    //
    // No timing assertion, deliberately: this returns at the JOIN, which waits out `sleeper`'s 2 s in
    // both the fixed and broken builds, so wall time here says nothing about when the waiter woke.
    // (On the CLI it is visible directly — post-fix `child got 7` prints at 50 ms, before
    // `sleeper done`; M:N capture buffers per task and flushes in slot order, so the captured string
    // cannot show it.)
    assert!(res.is_ok(), "a live program was faulted: {res:?}");
    assert!(out.contains("child got 7"), "output was: {out:?}");
}

/// W7-56 (the R3 fence — the veto must EXPIRE): a job that finishes WITHOUT sending must leave the
/// nursery free to report its genuine deadlock, not asleep forever.
///
/// This is the direction that makes W7-56 dangerous. Idle M:N workers `cv.wait` with no timeout, so a
/// veto nobody revokes is a permanent SILENT HANG — strictly worse than the false fault it replaces,
/// and it would have converted this exact program (which correctly faults at ~8 ms today) into a
/// hang. `spawn_into`'s completion closure therefore pokes every live sched after `finish()`,
/// taking each core lock briefly rather than a bare `notify_all` so a worker that already read
/// `outstanding == 1` cannot miss it. Post-fix this faults at ~59 ms (once the job ends) instead of
/// ~8 ms — later, but still a fault.
///
/// M:N-only + watchdogged for the reasons on the two tests above; here the watchdog IS the assertion.
#[test]
fn a_finished_executor_job_lets_the_genuine_nursery_deadlock_fire() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch := Channel[int]()
fn bail():
    time.sleep_ms(50)
fn waiter():
    v := ch.recv()
    print(\"child got {v}\")
ex := Executor()
ex.submit(bail)
parallel:
    spawn waiter()
";
    let entry = write_temp_chz("w756_finished_job_lifts_veto", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (_out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a job that ends without sending must let the deadlock fire — never hang");
    let _ = std::fs::remove_file(&entry);
    let msg = res
        .expect_err("nobody can ever feed `ch` once the job is done — a real deadlock")
        .message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
}

// ----- gaps.md W7-58 — a stuck job PLUS a stuck nursery owner is a deadlock, not a hang -----

/// W7-58, THE REPRO. A `parallel:` nursery whose task can never be fed, beside an eager `Executor`
/// job that can never be fed either, HUNG FOREVER. Measured pre-fix on the release binary: rc=124 at
/// a 20 s timeout, with `job blocking` and `child waiting` printed and nothing after. Post-fix it
/// faults in ~7 ms. Go reports `all goroutines are asleep - deadlock!` for the same shape.
///
/// The two vetoes were pointing at each other. `MnSched::is_deadlocked` declined because a job was
/// outstanding (W7-56 — an outstanding job is an uncounted sender, which is RIGHT when the job is
/// running), and the process-wide verdict declined because `live = 1 + outstanding = 2` while only
/// the job had registered: `main`, the `1 +`, was sitting in `mn_worker_loop`, which registered
/// nothing. So `parties.len() == 1 < 2` — "somebody is still running" — and that somebody was `main`,
/// which was exactly the thread that was stuck. Fixed by registering the nursery OWNER
/// (`PartyWait::Nursery`, satisfiable iff its nursery can still move) and by giving an idle worker of
/// that nursery the job of ASKING the verdict (an owner never reaches `block_halt_check`).
///
/// **W7-56's veto is untouched** — this does not weaken it, it teaches the OTHER verdict to see the
/// owner. `executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock` is the fence that it
/// stayed untouched, and it is also this change's audit (a): it runs the healthy shape (a job that
/// sleeps 50 ms and then feeds the parked task) with the owner now registered, and must still print
/// `child got 7`, rc=0.
///
/// **Depends on EAGER dispatch.** Under the lazy queue-at-`submit` / drain-at-`shutdown` model
/// (decision D3) `other.recv()` never runs before the join and the program is a DIFFERENT (also real)
/// deadlock, reported at a different site. Same precedent as the W7-56 tests above.
///
/// Watchdogged: pre-fix this HANGS, and a regression must fail the suite rather than stall it.
#[test]
fn a_stuck_executor_job_beside_a_stuck_nursery_faults_instead_of_hanging() {
    let src = "
import Executor from std.concurrency
ch := Channel[int]()
other := Channel[int]()
fn blocked():
    print(\"job blocking\")
    print(other.recv())
fn waiter():
    print(\"child waiting\")
    print(ch.recv())
ex := Executor()
ex.submit(blocked)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w758_stuck_job_and_stuck_nursery", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a stuck job beside a stuck nursery must FAULT — pre-fix this hung forever");
    let _ = std::fs::remove_file(&entry);
    let msg = res
        .expect_err("nothing in this run can ever move again")
        .message;
    assert!(
        msg.contains("deadlock"),
        "the nursery must report its deadlock; fault was: {msg}"
    );
    assert!(
        out.contains("child waiting"),
        "the task ran before it parked: {out:?}"
    );
    assert!(
        !out.contains("after nursery"),
        "the nursery join must NOT complete: {out:?}"
    );
}

/// W7-58, THE ANTI-FALSE-FAULT FENCE, and the highest-value test of this change. A healthy **cap-1**
/// producer/consumer pipeline running across an eager `Executor` job and a `parallel:` nursery task
/// must complete, every time.
///
/// This is the shape the `parked-is-not-stuck` family keeps killing: at steady state the party list
/// is `[Send(job), Nursery(owner)]` with `parties.len() == live`, so the COUNT does not save it and
/// satisfiability has to. It holds because the window in which neither party's wait looks satisfiable
/// (a value pushed, the parked consumer not yet requeued) is OCCUPIED by the sending thread, which is
/// either a fiber (so `running >= 1` and the nursery arm answers satisfiable) or a counted party
/// mid-`send` (so it is UNREGISTERED, `parties.len() < live`, and the count vetoes).
///
/// Both directions are exercised — the job producing into the nursery and the nursery producing into
/// the job — because a cap-1 channel is only ever half of a handshake. 300 handoffs each, which is
/// the density that made W7-12's three false-fault predicates show up as 2–7 failures in 30 rather
/// than never. A single green run here proves nothing; the suite gate is that it is green EVERY run.
/// (Measured on the release binary at the CLI: 30/30 at the default width and 30/30 at
/// `CHEZZI_THREADS=2`.)
///
/// Depends on eager dispatch + watchdogged, for the reasons on the tests above.
#[test]
fn a_cap1_pipeline_across_a_job_and_a_nursery_is_not_mistaken_for_a_deadlock() {
    let cases = [
        // The JOB produces, the nursery task consumes.
        "
import Executor from std.concurrency
data := Channel[int](1)
done := Channel[int](1)
fn producer():
    for i in 0..300:
        data.send(i)
    data.close()
fn consumer():
    total := 0
    for v in data:
        total = total + v
    done.send(total)
ex := Executor()
ex.submit(producer)
parallel:
    spawn consumer()
got := done.recv() else: panic(\"done closed\")
print(\"sum {got}\")
ex.shutdown()
",
        // …and the reverse: the nursery task produces, the job consumes and answers, so BOTH
        // directions of the cap-1 handshake cross the job/nursery boundary.
        "
import Executor from std.concurrency
req := Channel[int](1)
resp := Channel[int](1)
fn server():
    for v in req:
        resp.send(v * 2)
    resp.close()
fn client():
    total := 0
    for i in 0..300:
        req.send(i)
        got2 := resp.recv() else: panic(\"resp closed\")
        total = total + got2
    req.close()
    print(\"sum {total}\")
ex := Executor()
ex.submit(server)
parallel:
    spawn client()
ex.shutdown()
",
    ];
    let expect = ["sum 44850", "sum 89700"];
    for (i, src) in cases.iter().enumerate() {
        let entry = write_temp_chz(&format!("w758_cap1_pipeline_{i}"), src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_with(
                &run_entry,
                crate::native::HostConfig::default(),
            ));
        });
        let (out, _err, res, _code) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a healthy cap-1 pipeline must not hang");
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "a live cap-1 pipeline was faulted: {res:?}");
        assert!(
            out.contains(expect[i]),
            "case {i} lost handoffs: {out:?} (wanted {})",
            expect[i]
        );
    }
}

/// W7-58, THE JUDGE FENCE (R3). Two stuck nurseries and NOT ONE polling party: the top-level
/// `parallel:` owner and the eager job that opened its own `parallel:` are both sitting in
/// `mn_worker_loop`, which never calls `block_halt_check`. So with the party registration alone this
/// program still hangs — nobody ever ASKS the verdict. An idle worker of a quiesced sched therefore
/// escalates to the process-wide question on the owner's behalf.
///
/// Measured pre-fix: rc=124 at 20 s. Post-fix: ~7 ms.
///
/// **Asserts the fault, not the exact stdout.** Which of the two nurseries reports first is a
/// legitimate race between two idle workers, and pinning it would be pinning a scheduling accident.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn two_stuck_nurseries_with_no_polling_party_still_fault() {
    let src = "
import Executor from std.concurrency
ch := Channel[int]()
ch2 := Channel[int]()
fn innerjob():
    print(ch2.recv())
fn job():
    parallel:
        spawn innerjob()
fn waiter():
    print(ch.recv())
ex := Executor()
ex.submit(job)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w758_two_stuck_nurseries", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("two stuck nurseries with no polling party must fault — pre-fix this hung");
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("neither nursery can ever move").message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(
        !out.contains("after nursery"),
        "the outer join must not complete: {out:?}"
    );
}

/// W7-58 — the judge must be LEVEL-triggered, not EDGE-triggered. A stuck sched that asked the
/// process-wide verdict and was told "not yet" must ask again when the answer changes — and the
/// answer changes with the PARTY SET, which lives outside this sched and notifies nothing here.
///
/// Measured with the judge's idle wait left untimed: **5/5 permanent hangs, rc=124 at 20 s**. The
/// job's nursery quiesces immediately, judges (`parties.len() == 1 < live == 2` — `main` is awake and
/// unregistered), and sleeps forever; `main`'s `Join` registration 300 ms later reaches nobody, and
/// the run has no judge at all. The tell that isolated it: adding a third job that finishes later
/// made the same program FAULT, because `finish` pokes every sched and the judge re-ran. Post-fix it
/// faults at ~310 ms — the first moment the verdict is actually true.
///
/// The `sleep_ms` is load-bearing: without it `main` reaches `shutdown()` before the nursery
/// quiesces, so the registration lands before the judge's first look and the bug is invisible.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn a_nursery_judge_re_asks_the_verdict_when_a_party_registers_later() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch := Channel[int]()
fn innerjob():
    print(ch.recv())
fn job():
    parallel:
        spawn innerjob()
ex := Executor()
ex.submit(job)
time.sleep_ms(300)
ex.shutdown()
print(\"done\")
";
    let entry = write_temp_chz("w758_judge_reasks", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a party registering AFTER the judge's last look must still be judged — this hung");
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("the job's nursery can never move").message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(!out.contains("done"), "the join must not return: {out:?}");
}

/// W7-58 residual — a cycle of `Executor` joins with NO other party in it hung forever, because
/// `join_executor` REGISTERED a party and then waited on a plain untimed condvar: it never asked
/// the verdict it had just made answerable.
///
/// Measured pre-fix: three executors whose jobs each `shutdown()` the next, plus `main` joining the
/// first — 4 parties, `live == 4`, every `Join` unsatisfiable, rc=124 at 15 s. The sibling shapes
/// (`two_executors_deadlocking_each_other_fault`) fault in milliseconds only because SOME party in
/// them happens to be channel-blocked and therefore polls; the fault was an accident of the shape.
/// The join now polls at the same `DEMOTE_POLL_BACKOFF` cadence every other blocking-in-place site
/// pays, and asks — but ONLY when every registered party is a `Join`, since any other kind of party
/// has a judge of its own whose fault names the real blocking SITE (which is why
/// `two_executors_deadlocking_each_other_fault` still reports `recv on an empty channel`, unchanged).
///
/// **The ring is STRUCTURAL and the OUTCOME is deterministic, and those are two different problems.**
/// Two earlier cuts of this test were races and both are recorded, because the second failure is the
/// non-obvious one:
///
/// 1. Three executors whose jobs each `shutdown()` the next, gated only on their `recv`s. The gate
///    did not cover the `shutdown` that closes the ring, so the program was often not a cycle at all
///    — measured on the release binary, it completed normally **7 of 40 runs**.
/// 2. Two executors with a proper start BARRIER (each job announces on an unbuffered channel, `main`
///    collects both, then releases both). That ring really is structural — instrumented with
///    `recover:`, **60 of 60 runs detected the deadlock**. It was still **39/40** as a test, because
///    WHICH party reports first is a legitimate race, and when `jobB` (the job of `ex2`) reports it,
///    its fault lands in `ex2`'s slots — an executor nobody in that program ever reduces — so `jobA`
///    completes cleanly, `main`'s `ex1.shutdown()` returns Ok, and the process exits 0.
///
/// 3. A **one-job SELF-join ring**: `main` and the executor's only job both wait on that same
///    executor. It was 40/40 and it kept the fault reaching `main`, but its ring was an ARTEFACT of
///    the bug this file now fixes, not a property of the program. The job was waiting for
///    `outstanding` to reach 0 while being the only thing keeping it above 0 — waiting for itself.
///    `Vm::join_executor` now discounts the joiner's own slot, so that program is healthy (it is
///    the `cancel`-from-inside-its-own-job shape, which CPython 3.14.6 completes: measured
///    `A\nC\nend`, rc 0, 40/40) and this test would have been pinning the false fault in place.
///
/// **TWO jobs of ONE executor, each joining it**, keeps every virtue of cut 3 and is a genuine
/// deadlock: each job waits for the executor to owe nothing but itself, i.e. for the OTHER job to
/// finish, and neither ever will. `main` waits for both. Three parties, all `Join`, none satisfiable
/// — the exact W7-58 residual shape. Still one executor and one slot vec, the one `main` reduces, so
/// whichever party detects it the fault reaches `main`. Measured on the debug binary: 40/40 fault
/// after the self-join fix, 30/30 before it (the fix does not touch this shape — `slack` is 1 on
/// each job and `outstanding` is 2). For contrast, the cross-executor ring cut 1 warns about is
/// still a race under the same harness: three executors each shutting down the next measured 28/30
/// fault, 2/30 clean exit, IDENTICALLY before and after the fix.
///
/// The barrier is still load-bearing: without it `ex.submit` races `ex.shutdown()` and a job can be
/// dispatched after `main` has already drained, or rejected by the `shut` flag the first job sets.
///
/// A blocked eager job now hands its pool slot to a replacement (TICKET-052), so this shape no
/// longer needs a second free pool thread — both jobs dispatch at `--threads=1` too.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn a_cycle_of_executor_joins_faults_instead_of_hanging() {
    let src = "
import Executor from std.concurrency
ex := Executor()
started := Channel[int]()
go := Channel[int]()
fn job():
    started.send(1)
    go.recv()
    ex.shutdown()
ex.submit(job)
ex.submit(job)
started.recv()
started.recv()
go.send(1)
go.send(1)
ex.shutdown()
print(\"after\")
";
    let entry = write_temp_chz("w758_join_cycle", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a cycle of executor joins must fault, not hang");
    let _ = std::fs::remove_file(&entry);
    let err = res.expect_err("every party is joining an executor that owes work");
    let msg = err.message.clone();
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(!out.contains("after"), "the join must not return: {out:?}");
    assert_ne!(
        err.span.file, 0,
        "the join-deadlock headline must name a file, got {:?}",
        err.span
    );
}

/// The program of `a_self_shut_executor_still_has_its_slots_reduced_at_program_exit`.
const SELF_SHUT_PROGRAM: &str = r#"
import Executor from std.concurrency
ex := Executor()
done := Channel[int]()
fn closer():
    ex.shutdown()
    done.send(1)
ex.submit(fn(): print("A"))
ex.submit(fn(): print("C"))
ex.submit(closer)
done.recv()
print("end")
"#;

/// TICKET-270: `SELF_SHUT_PROGRAM` submits a third job after `closer`, so it faults whenever a pool
/// thread runs `closer` first. This test forces that order: main waits for `closer` to finish
/// `shutdown()` before the third submit. The program must be `Ok` under that order.
#[test]
fn a_self_shut_program_does_not_submit_after_its_own_shutdown() {
    let forced = SELF_SHUT_PROGRAM
        .replace("ex.submit(closer)", "ex.submit(closer)\ndone.recv()")
        .replace("done.recv()\nprint", "print");
    let r = run_capture(&forced);
    assert!(
        r.is_ok(),
        "closer ran first and the third submit faulted: {r:?}"
    );
}

/// A job that shuts down the executor it is running under leaves that core's submission slots
/// UNREDUCED — its own slot is still live, so `join_executor` may not `mem::take` the vector. The
/// program-exit drain must therefore pick the core up even though it is already `shut`, or every
/// sibling's buffered output and every sibling's fault is dropped on the floor.
///
/// **Rust and not `tests/chz/`, on purpose.** The observable is the run's CAPTURED stdout — the
/// buffered sink at the Rust capture boundary. Chezzi cannot read its own stdout, so `assert`
/// genuinely cannot express this. It is also invisible from `chezzi run` (a streamed `print` already
/// reached fd 1 when it ran) and from `chezzi test` (the runner takes each test's `out` BEFORE the
/// file-level `reap_after_tests` drain, so it discards this output for a never-shut executor too —
/// measured 6/6). `run_capture` is the only sink where the slot IS the only copy, which is exactly
/// what every embedder gets.
///
/// The `done` channel is load-bearing: it makes `main` reach the drain only AFTER the inner
/// `shutdown()` has returned, so the core is reliably `shut`-with-unreduced-slots rather than
/// racing `main` into an ordinary `slack == 0` join that would have reduced it anyway.
///
/// Measured on the pre-fix binary: the first program captured `"end\n"` (A and C silently lost,
/// 10/10) and the second exited **Ok** with the sibling's assertion failure swallowed.
///
/// `closer` runs its OWN self-join (`ex.shutdown()`) synchronously on the worker thread that
/// dispatched it, and that join waits for siblings `A`/`C`. A blocked eager job now hands its pool
/// slot to a replacement (TICKET-052), so `closer`'s join no longer starves `A`/`C` of a thread.
///
/// **No watchdog (TICKET-059).** The regression FAILS an assertion (measured 10/10 pre-fix) rather
/// than hanging, so a channel-receive deadline never catches the defect -- only a starved host, which
/// needs at least 2 free pool threads. Measured `taskset -c 0` (1 CPU): `test result: FAILED. 0
/// passed; 1 failed; ... finished in 60.00s`, the watchdog expiring, not an assertion. The skip below
/// checks that host precondition directly instead.
#[test]
fn a_self_shut_executor_still_has_its_slots_reduced_at_program_exit() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 2 || crate::vm::worker_count() < 2 {
        eprintln!(
            "SKIP: {cores} CPU -- closer's self-join needs 2 free pool threads (pool.rs risk G3)"
        );
        return;
    }

    let out = run_capture(SELF_SHUT_PROGRAM)
        .expect("a job shutting down its own executor is healthy — see the self-join fix");
    // `end` first: `main` prints it before the exit drain, which is what then flushes the slots.
    // A before C is the W7-5c per-slot flush on the buffered sink, in SUBMISSION order — that half
    // IS ordered, unlike the streamed `chezzi run` path (see `the Executor's sched`'s doc).
    assert_eq!(
        out, "end\nA\nC\n",
        "a self-shut executor's buffered job output must survive to the captured stdout"
    );

    // The same hand-off carries FAULTS: a sibling's assertion failure must reach its Task
    // (TICKET-289: it no longer ends the run), not be dropped by the self-shut hand-off.
    let (out, res) = run_program(
        "
import Executor from std.concurrency
ex := Executor()
done := Channel[int]()
fn closer():
    ex.shutdown()
    done.send(1)
fn boom():
    assert 1 == 2
t := ex.submit(boom)
_ := ex.submit(closer)
done.recv()
match t.get():
    ?_: print(\"no fault\")
    !e: print(\"task fault: {e.message()}\")
",
    );
    res.expect("a job fault must not end the run");
    assert!(
        out.contains("task fault: assertion failed"),
        "the sibling job's assertion failure must reach its Task: {out:?}"
    );
}

/// W7-60 — an `Executor.cancel()` must reach a job that is itself parked in an INNER
/// `shutdown()`. Before this, `join_executor` observed only the deadlock verdict, so the inner
/// join was uncancellable and the outer `cancel` waited for work it had just asked to stop.
///
/// **The inner job is deliberately uninterruptible.** `process.run` is documented as having no
/// cancellation checkpoint until it returns (`docs/reference/stdlib/` §std.process), so the ONLY party that
/// can end this wait early is the joiner itself — which is exactly the rung under test. A job
/// blocked on a `Channel` instead would be ended by the deadlock verdict that already existed, and a
/// `time.sleep_ms` job would be ended by its own cancel checkpoint (`W7-16`); neither would fail
/// pre-fix, and neither would be testing this.
///
/// Measured on the release binary, `chezzi run`: pre-fix **6 011 ms** (the full `sleep 6`), post-fix
/// **213 ms** (with `sleep 6`; the fixture below uses `sleep 2` so it strands a process-global
/// pool worker for as little time as still discriminates) — against `docs/reference/stdlib/`'s own "ends the wait within ~5 ms" promise. The cancelled
/// joiner's own output is SWALLOWED, as every cancelled task's is, so `job past inner join` must NOT
/// appear; that is the observable half of the assertion and the reason it is not a pure timing test.
///
/// Depends on eager dispatch (a lazy queue-at-`submit` model has no running job to cancel) +
/// watchdogged, for the reasons on the tests above.
#[test]
fn cancel_cancels_a_job_parked_in_a_nested_executor_join() {
    let src = "
import Executor, Shared from std.concurrency
import std.process
import std.time\nimport timer from std.time
run_started: Channel[int] = Channel[int](1)
joining: Channel[int] = Channel[int](1)
sleep_over: Shared[bool] = Shared[bool](false)
fn uninterruptible():
    run_started.send(0)
    r := recover: process.run(\"sleep 2\")
    sleep_over.set(true)
fn nested():
    inner := Executor()
    inner.submit(uninterruptible)
    _ := run_started.recv()
    print(\"job joining inner\")
    joining.send(0)
    inner.shutdown()
    print(\"job past inner join\")
ex := Executor()
ex.submit(nested)
_ := joining.recv()
print(\"inner still running: \" + \"{not sleep_over.get()}\")
ex.cancel()
print(\"main done\")
";
    let entry = write_temp_chz("w760_cancel_nested_join", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("cancel must not wait out an uninterruptible nested job");
    let _ = std::fs::remove_file(&entry);
    res.expect("the program itself completes normally");
    assert!(
        out.contains("job joining inner") && out.contains("main done"),
        "the run must complete: {out:?}"
    );
    // A swallowed-print assertion alone cannot separate the W7-60 defect from host starvation: both
    // print the same three lines when main is starved past the fixture's 2s sleep before it can call
    // `cancel` (measured 2026-09-05). `sleep_over`, read immediately before `cancel`, is
    // the one instant that tells them apart -- checking the host precondition directly, the same
    // shape as the skip on `a_self_shut_executor_still_has_its_slots_reduced_at_program_exit`
    // (TICKET-059).
    if out.contains("inner still running: false") {
        eprintln!(
            "SKIP: the host starved main past the fixture's 2 s sleep -- the joiner had already \
             left the inner join before cancel was called: {out:?}"
        );
        return;
    }
    assert!(
        out.contains("inner still running: true"),
        "the fixture must report whether the inner job was still running: {out:?}"
    );
    assert!(
        !out.contains("job past inner join"),
        "the cancelled joiner's outcome is swallowed, so its later print must not appear: {out:?}"
    );
}

/// W7-13 — a healthy cap-1 handshake must be driven by WAKEUPS, not by the poll timeout.
///
/// `block_wait_tick` used to hand a freshly-taken `core.q` guard straight to `cv.wait_timeout` with no
/// predicate. Its caller had already dropped that lock to attempt its `send`/`recv`, and then ran
/// `block_halt_check` (which takes `exec_registry` + per-core `eager`) before re-taking it — so a
/// consumer's `notify_all` landing in that window hit a condvar nobody was on yet and was lost, and
/// the sender slept a whole `DEMOTE_POLL_BACKOFF` on a channel that already had room.
///
/// **This asserts on the DEFECT ITSELF, not on how long the pipeline took, and that is what makes it
/// load-independent.** [`crate::vm::blocking::BLOCK_WAITS_SLEPT_WHILE_READY`] counts waits that burned a
/// whole `DEMOTE_POLL_BACKOFF` tick and then woke to find the channel READY — the lost wakeup, in one
/// number. `wait_timeout_while` re-evaluates the predicate under the guard after each inner wait, so
/// `timed_out()` implies "still not ready" and that counter is structurally ZERO in a fixed build, on
/// an idle machine and on a hammered one alike. Mutation-verified by reverting the call to the old
/// bare `wait_timeout` (which has no such guarantee): **0 of 323 waits fixed, 309 of 1014 broken**,
/// over these same 6 runs. The bug is that dense, so 6 runs replace the 30 the old timing bound
/// needed, and the test costs 0.05 s instead of 0.21 s idle / 5.07 s loaded.
///
/// **Two earlier designs failed, and both are recorded here because neither failure is obvious.**
///
/// 1. **A wall-clock bound on 30 pipelines.** Mutation separation was real (0.14 s fixed vs 2.19 s
///    broken on the release binary) but useless in the suite: the same 30 runs took **5.07 s** under
///    an ordinary full `cargo test` at `RUST_TEST_THREADS=4` — CPU contention costs MORE than the bug
///    does, so every bound loose enough not to fail on a busy machine also passed while the bug was
///    live. It failed on every full-suite run, permanently.
/// 2. **A process-global count of EXPIRED waits.** libtest runs the whole file in ONE process, and
///    the eager tests a dozen slots away in name order each park a job on a `timer(200)` — ~40
///    expired ticks apiece, onto the same global. That version passed alone, passed when the suite
///    scheduled kindly, and FAILED at 24 under `--test-threads=8`; it had already reported one false
///    green.
///
/// The counter used here is process-global too, and is immune to (2) for a reason worth stating: it
/// counts only an event a healthy build CANNOT produce. A neighbour's honest 5 ms timer park is a
/// wait that expired while genuinely not ready and never touches it; a neighbour could pollute this
/// only by hitting the same defect, and then failing is the correct outcome. `BLOCK_WAITS` — total
/// waits — is polluted by neighbours and so is used only as a `>=` COVERAGE floor: it asserts this
/// program still reaches `block_wait_tick` at all, so a future refactor that stopped blocking there
/// cannot leave the test passing vacuously.
#[test]
fn eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout() {
    let src = "
import Executor from std.concurrency
a: Channel[int] = Channel[int](1)
fn prod():
    for i in 0..200:
        a.send(i)
    a.close()
ex := Executor()
ex.submit(prod)
s := 0
for v in a:
    s = s + v
print(\"sum {s}\")
ex.shutdown()
";
    let entry = write_temp_chz("w713_handshake_wakeups", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    use std::sync::atomic::Ordering::Relaxed;
    let waits0 = crate::vm::blocking::BLOCK_WAITS.load(Relaxed);
    let stalls0 = crate::vm::blocking::BLOCK_WAITS_SLEPT_WHILE_READY.load(Relaxed);
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..30 {
            let (o, _e2, r, _c) = run_file(&e);
            if r.is_err() || o != "sum 19900\n" {
                bad.push(format!("{r:?} / out={o:?}"));
            }
        }
        let _ = tx.send(bad);
    });
    // A worker panic drops `tx` and returns `Disconnected` immediately, which is NOT a hang — say so,
    // rather than reporting every failure as one.
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(120));
    let _ = std::fs::remove_file(&entry);
    let bad = match outcome {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("a bounded cap-1 pipeline hung for 120 s")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the pipeline worker panicked — see the panic above this one")
        }
    };
    let waits = crate::vm::blocking::BLOCK_WAITS.load(Relaxed) - waits0;
    let stalls = crate::vm::blocking::BLOCK_WAITS_SLEPT_WHILE_READY.load(Relaxed) - stalls0;
    assert!(
        bad.is_empty(),
        "the pipeline must still be correct: {bad:#?}"
    );
    assert_eq!(
        stalls, 0,
        "{stalls} of {waits} blocking waits slept a full 5 ms poll tick and then woke to a channel \
         that was ALREADY ready — a healthy handshake is driven by notifications, and W7-13's lost \
         wakeup is the only thing that produces this (measured: 0 fixed, 309 of 1014 waits with \
         `block_wait_tick` reverted to a bare `wait_timeout`, same 6 runs)"
    );
    // Coverage floor, not a measurement — a concurrent test's own blocking also lands in
    // `BLOCK_WAITS`, so this can only ever be too GENEROUS. 30 runs × 200 handoffs at cap 1 cannot
    // hand off without blocking; if this trips, the program stopped exercising `block_wait_tick` and
    // the assertion above went vacuous. TICKET-208 — 30 runs, not 6: the producer is a parking
    // fiber now, so `main` is the only party that blocks in place (measured alone: 45 to 304 waits
    // per 6 runs, 193 to 968 per 30).
    assert!(
        waits >= 100,
        "only {waits} blocking waits — this cap-1 pipeline no longer reaches `block_wait_tick`, so \
         the stall assertion above is vacuous"
    );
}

/// W7-13r(c) — an eager job blocked on a FULL channel must fault when that channel is CLOSED, not
/// wait for something else to notice.
///
/// `enqueue_bounded` never consults `closed`, and the eager block loop never returns to the
/// top-of-`send` closed guard, so a blocked sender had no way to observe a close at all. Measured on
/// the release binary, same program:
///
/// | | pre-fix | post-fix | Go |
/// |---|---|---|---|
/// | with `shutdown()` | 112 ms, but reports `send on a full channel: deadlock` about a CLOSED channel | 105 ms, `send on a closed channel` | 104 ms, `panic: send on closed channel` |
/// | no `shutdown()` (this test) | **HANGS** — killed at 12 s | 105 ms | — |
///
/// Without an explicit `shutdown()` the W7-12 verdict cannot fire (`joining == 0`), so nothing else
/// caught it — that is why this test drops the `shutdown()`.
///
/// **The `ready` handshake is what makes the test prove anything**, and it was added after review:
/// with `closer` merely sleeping, an unlucky schedule could run it BEFORE `blocker`, and then
/// `ch.send(1)` faults at the pre-existing top-of-`send` guard — every assertion below passes for a
/// reason that has nothing to do with this fix, on the PRE-fix binary. Waiting for `ready` makes the
/// first `send` provably succeed, so the fault can only come from the blocked second one.
///
/// A blocked eager job now hands its pool slot to a replacement (TICKET-052), so `closer` dispatches
/// at `--threads=1` too. The 30 s guard below turns a genuine hang into a clear failure rather than
/// a hung suite.
///
/// **Depends on real interleaving.** Under the lazy queue-at-`submit` model both jobs are queued and
/// the drain runs them one at a time, so `blocker` would fault `FULL_SEND_DEADLOCK` before `closer`
/// got to run its `close()` — that model cannot express this program at all (it is NOT that the
/// closer is absent: it runs, and closes the channel, after the fault). `docs/contributing/gaps.md` W7-13r records
/// that as a deliberate divergence of the since-removed cooperative engine, under the standing rule
/// that correctness outranks engine agreement.
#[test]
fn eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch: Channel[int] = Channel[int](1)
ready: Channel[bool] = Channel[bool](1)
fn blocker():
    ch.send(1)
    print(\"filled\")
    ready.send(true)
    ch.send(2)
    print(\"blocker finished\")
fn closer():
    _ := ready.recv()
    _2 := time.timer(50).recv()
    ch.close()
ex := Executor()
t := ex.submit(blocker)
_ := ex.submit(closer)
match t.get():
    ?_: print(\"blocker ok\")
    !e: print(\"task fault: {e.message()}\")
";
    let entry = write_temp_chz("w713c_send_sees_close", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = match outcome {
        Ok(v) => v,
        // Two very different causes, so do not accuse the bug by default.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
            "no result in 30 s: the W7-13r(c) hang is back (a blocked eager `send` not observing \
             `close()`)"
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the VM thread panicked — see the panic above this one")
        }
    };
    // Proves the blocked path was actually exercised: without this, a `closer`-first schedule faults
    // at the top-of-`send` guard instead and the test passes on the unfixed code.
    assert!(
        out.contains("filled"),
        "the first send must have succeeded, or this test is not exercising the blocked path: \
         out={out:?} r={rdbg}"
    );
    // TICKET-289: the job's fault lives in its Task, so the run itself ends clean.
    assert!(
        !failed,
        "a job fault must not end the run: out={out:?} r={rdbg}"
    );
    assert!(
        out.contains("task fault: send on a closed channel"),
        "the Task must hold the CLOSE fault, not a full-channel deadlock (that was the wrong answer \
         this fixed): out={out:?} r={rdbg}"
    );
    assert!(
        !out.contains("blocker finished"),
        "the send must not appear to succeed: {out:?}"
    );
}

/// W7-13r(a) — an eager `wait:` block must be woken by its first arm, not by the poll timeout.
///
/// `op_wait_poll`'s eager branch was a bare `thread::sleep(DEMOTE_POLL_BACKOFF)`, so EVERY wake cost a
/// full 5 ms tick however fast the value arrived. It now waits on ARM 0's condvar with the tick as the
/// timeout — the trick `demote_wait_block` already used for the same N-arms-no-single-condvar problem,
/// which is why the earlier "this needs a new multi-channel wait primitive" note was wrong.
///
/// TICKET-270 -- the blocks floor is gated on `WAIT_ARM0_ENTRIES`: a `main` descheduled after
/// `gate.send(true)` finds the value queued and never enters the block branch (W15-8: `got 149`,
/// `got 178`, `got 229`). No entry skips the floor; any entry needs `blocks >= 1`. The `skewed_even`
/// cell makes that interleaving on half the rounds with no host load.
///
/// Every blocking `wait:` wakeup must block on arm 0's condvar and none of them may sleep its
/// whole tick with arm 0 already ready (TICKET-059: `WAIT_ARM0_BLOCKS` / `WAIT_ARM0_SLEPT_WHILE_READY`
/// replace a wall-clock bound, which was NOT redundant — measured, a blind `thread::sleep(tick)`
/// revert still prints `44850` and passes every output assertion, only the timing failed
/// (`took 1.552389135s`), so the counted floor is what actually catches a reverted wakeup path: it
/// falls from 300 to 0. `WAIT_ARM0_SLEPT_WHILE_READY` is zero by construction on a healthy build
/// (`wait_timeout_while` re-checks the predicate under the guard before returning) and reads 3 under a
/// plain `wait_timeout` revert (W7-13's own defect).
#[test]
fn an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout() {
    use crate::vm::blocking::{WAIT_ARM0_BLOCKS, WAIT_ARM0_ENTRIES, WAIT_ARM0_SLEPT_WHILE_READY};
    use std::sync::atomic::Ordering::Relaxed;
    // `skewed_even` delays `main` between its `gate.send` and its `wait:` on even rounds, so `prod`
    // wins that race and `main` finds the value queued: the interleaving a descheduled `main` makes
    // under host load (W15-8), reproduced with no load.
    let cells = [
        ("plain", ""),
        (
            "skewed_even",
            "    if i % 2 == 0:\n        time.sleep_ms(2)\n",
        ),
    ];
    for (name, skew) in cells {
        let entries0 = WAIT_ARM0_ENTRIES.load(Relaxed);
        let blocks0 = WAIT_ARM0_BLOCKS.load(Relaxed);
        let stalls0 = WAIT_ARM0_SLEPT_WHILE_READY.load(Relaxed);
        // The `gate` handshake is what makes this test exercise the path at all: `cons` announces that it
        // is about to block, and only then does `prod` send. Without it `prod` races ahead, every `wait:`
        // finds its value already queued, and the block branch is never reached — measured: the vacuous
        // version passed even with the wait stubbed back to a blind sleep.
        let src = format!(
            "
import Executor from std.concurrency
import std.time
data: Channel[int] = Channel[int](1)
other: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
fn prod():
    for i in 0..300:
        _ := gate.recv()
        data.send(i)
ex := Executor()
ex.submit(prod)
s := 0
for i in 0..300:
    gate.send(true)
{skew}    wait:
        v := data.recv():
            n := v else: panic(\"data closed\")
            s = s + n
        w := other.recv():
            m := w else: panic(\"other closed\")
            s = s + m
ex.shutdown()
print(s)
"
        );
        let entry = write_temp_chz("w713a_wait_wakeups", &src);
        let (tx, rx) = std::sync::mpsc::channel();
        let e = entry.clone();
        std::thread::spawn(move || {
            let (o, _e2, r, _c) = run_file(&e);
            let _ = tx.send((o, format!("{r:?}"), r.is_err()));
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
        let _ = std::fs::remove_file(&entry);
        let (out, rdbg, failed) = outcome.expect("300 `wait:` wakeups did not finish in 60 s");
        assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
        assert_eq!(
            out, "44850\n",
            "every arm value must still be received exactly once"
        );
        let entries = WAIT_ARM0_ENTRIES.load(Relaxed) - entries0;
        let blocks = WAIT_ARM0_BLOCKS.load(Relaxed) - blocks0;
        let stalls = WAIT_ARM0_SLEPT_WHILE_READY.load(Relaxed) - stalls0;
        assert_eq!(
            stalls, 0,
            "{name}: an eager `wait:` slept its whole tick with arm 0 already ready -- the W7-13 lost \
         wakeup is back (measured 0 fixed, 3 under a plain wait_timeout)"
        );
        if entries == 0 {
            eprintln!(
                "SKIP {name}: no `wait:` reached the block branch (main arrived after every send), so \
             this run cannot tell a condvar wait from a blind sleep"
            );
            continue;
        }
        assert!(
            blocks >= 1,
            "{name}: {entries} `wait:` entries reached the block branch and none waited on arm 0's \
         condvar -- a blind thread::sleep is back (measured 0 under that revert)"
        );
    }
}

/// D2 — an eager `wait:` (an Executor job) with a CLOSED arm 0 beside a live arm takes the closed
/// arm at once, binding `!RecvError`: Go's `case <-done:` with `done` closed as a broadcast cancel.
/// With the closed arm switched OFF by its guard, the live arm's value is still delivered.
///
/// History (W7-13r(a)): when the poll SKIPPED a closed+empty arm, an arm-0 predicate that read
/// `|| g.closed` as ready spun the eager wait at 99% CPU. Under D2 closed is ready AND taken, so a
/// wake on it ends the wait. The guarded-off case keeps the old shape — a closed arm the poll will
/// not take, beside a live arm — and pins that it still completes with the live value.
///
/// **What this test can and cannot catch.** It pins the SEMANTICS (the right arm, no hang). It does
/// NOT measure CPU: a predicate that spun on the guarded-off closed arm would still print 7.
#[test]
fn an_eager_wait_takes_a_closed_arm_and_a_guarded_off_one_yields_the_live_arm() {
    for (live, expected) in [("true", "-1\n"), ("false", "7\n")] {
        let src = format!(
            "
import Executor from std.concurrency
import std.time
done: Channel[bool] = Channel[bool](1)
work: Channel[int] = Channel[int](1)
out: Channel[int] = Channel[int](1)
DONE_ON: bool = {live}
fn cons():
    wait:
        _ := done.recv() if DONE_ON:
            out.send(-1)
        v := work.recv():
            n := v else: panic(\"work closed\")
            out.send(n)
fn prod():
    _ := time.timer(60).recv()
    work.send(7)
done.close()
ex := Executor()
ex.submit(cons)
ex.submit(prod)
ex.shutdown()
r := out.recv() else: panic(\"out closed\")
print(r)
"
        );
        let entry = write_temp_chz("w713a_closed_arm0", &src);
        let (tx, rx) = std::sync::mpsc::channel();
        let e = entry.clone();
        std::thread::spawn(move || {
            let (o, _e2, r, _c) = run_file(&e);
            let _ = tx.send((o, format!("{r:?}"), r.is_err()));
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        let (out, rdbg, failed) =
            outcome.expect("a `wait:` with one closed arm hung (DONE_ON={live})");
        assert!(!failed, "DONE_ON={live}: out={out:?} r={rdbg}");
        assert_eq!(
            out, expected,
            "DONE_ON={live}: an ON closed arm must be taken at once; an OFF one leaves the live arm"
        );
    }
}

/// W7-13r(c)'s ORDERING fence — the closed-check must come AFTER the enqueue retry, never before.
///
/// This is the regression the first draft of that fix shipped, caught by adversarial review and not
/// by any existing test. The shape is the most ordinary one there is: a consumer takes the last item
/// and then closes.
///
/// ```text
/// consumer:  a := ch.recv()   # frees the slot FOR the blocked sender
///            ch.close()       # …then wins the race back to `core.q`
/// ```
///
/// Go completes this program — its receive hands the value to a waiting sender ATOMICALLY inside the
/// recv, so by the time `close` runs the send has already happened. Chezzi's eager sender is
/// retry-based: it is only woken and must re-take the slot, so a closed-check placed BEFORE the retry
/// let the close deterministically beat it. Measured 5/5 each way: Go and pre-fix Chezzi print
/// `sent both`; closed-check-first faulted `send on a closed channel` on every run.
///
/// Checking `closed` only after the retry has failed is also exactly the drain-before-close rule the
/// top-of-`send` guard already documents at the head of `channel_method`.
#[test]
fn a_blocked_eager_send_still_completes_when_a_recv_frees_its_slot_before_the_close() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
ch: Channel[int] = Channel[int](1)
fn blocker():
    ch.send(1)
    ch.send(2)
    print(\"sent both\")
fn consumer():
    _ := time.timer(100).recv()
    a := ch.recv()
    ch.close()
    print(\"got {a}\")
ex := Executor()
ex.submit(blocker)
ex.submit(consumer)
ex.shutdown()
";
    let entry = write_temp_chz("w713c_recv_then_close", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect("no result in 30 s — a hang");
    assert!(
        !failed,
        "a send whose slot was freed by a `recv` must COMPLETE, as Go does — faulting here is the \
         closed-check-before-retry regression: out={out:?} r={rdbg}"
    );
    assert!(
        out.contains("sent both"),
        "the freed slot must be taken by the blocked sender: out={out:?}"
    );
}

/// W7-14 — a `wait:` timer arm must LOSE to a sibling value that arrives first for every waiter that
/// owns its OS thread, exactly as `WAIT-1` already made it lose inside a `parallel:` nursery.
///
/// The timeout arm was beating the thing it is a timeout *for*: `op_wait_poll`'s cooperative
/// inline-sleep slept to the timer deadline and took the timer without re-reading the siblings, and
/// WAIT-1's timed-park fix (`0b72ad60`) never reached these paths because it is gated on
/// `self.mn.is_some()` — none of them has an `MnSched`. So a `wait:` with any timer arm degenerated
/// into a plain sleep. Release binary, the same program per waiter (with `timer(300)`, a value at
/// 50 ms):
///
/// | waiter | before | after |
/// |---|---|---|
/// | `parallel:` / `spawn:` fiber (WAIT-1's path) | `value 9` @ 54 ms | unchanged |
/// | eager `Executor` job | **`timer` @ 306 ms** | **`value 9` @ 56 ms** |
/// | top-level `main` | **`timer` @ 306 ms** | **`value 9` @ 56 ms** |
/// | top-level `main` INSIDE a native callback | **`timer` @ 308 ms** | **`value 9` @ 57 ms** |
///
/// Go's `select` takes the value. Not an eager-execution regression — pre-eager `main` (`b6cb9201`)
/// measures the same 306 ms. The fix is NOT a ported `send_wake`: WAIT-1 injects a background
/// deadline send because a parked fiber has no thread of its own, while these parties own their OS
/// thread and can simply clamp the in-place wait to the deadline. Mutation-verified: with the gate
/// reverted, each of these tests reads `timer`.
///
/// **Why `timer(3000)` here when the bug reproduces at 300 ms, and why the discriminator is the
/// OUTPUT rather than a wall-clock bound (TICKET-059).** `value 9` vs `timer` is byte-exact and needs
/// no threshold. Measured: reverting `op_wait_poll`'s inline-sleep gate (`src/vm/netio.rs`) to the
/// pre-W7-14 `if let Some((i, deadline)) = soonest {` makes all three of this test's family read
/// `left: "timer"` verbatim.
///
/// The `gate` handshake orders the two jobs so the producer's 50 ms starts only after the consumer
/// has entered its `wait:` — without it a producer that races ahead leaves the value already queued
/// and the POLL takes it, which passes even with the bug. (It is the 50 ms sleep that makes the block
/// branch reachable; the gate is what stops the sleep from starting too early.)
#[test]
fn an_eager_wait_timer_arm_loses_to_a_sibling_value() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
fn cons():
    t := time.timer(3000)
    gate.send(true)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
";
    let entry = write_temp_chz("w714_eager_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect("the eager `wait:` hung");
    assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
    assert_eq!(
        out, "value 9\n",
        "the 50 ms value must beat the 3 s timer arm (W7-14: the inline-sleep took the timer)"
    );
}

/// W7-14 on the **top-level `main`** thread — see the sibling above for the full account.
///
/// `main` is a counted party with no scheduler under it (`mn == None`), so it took the identical
/// inline-sleep and gave the identical wrong answer while an eager job was about to send. Fixing only
/// `the job's sched` parties would have left this live.
///
/// Also fences the verdict: `main` registers as a blocked party for this wait, so a timer arm MUST
/// veto the process-wide deadlock verdict (`quiesce::PartyWait::Wait` answers satisfiable for any
/// `core.timer.is_some()` arm). If that ever stops holding, this test faults instead of printing.
#[test]
fn a_top_level_wait_timer_arm_loses_to_an_eager_job() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
ex := Executor()
ex.submit(prod)
t := time.timer(3000)
gate.send(true)
wait:
    _ := t.recv():
        print(\"timer\")
    v := work.recv():
        print(\"value {v}\")
ex.shutdown()
";
    let entry = write_temp_chz("w714_main_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect("the top-level `wait:` hung");
    assert!(
        !failed,
        "the program must succeed — a timer arm must veto the deadlock verdict: out={out:?} r={rdbg}"
    );
    assert_eq!(
        out, "value 9\n",
        "the 50 ms value must beat the 3 s timer arm on the `main` thread too (W7-14)"
    );
}

/// W7-14's THIRD waiter — the top-level `main` thread **inside a native callback** (here a list HOF;
/// `Shared.update` and an FFI callback are the same shape). Found by adversarial review of the first
/// two, which shipped green while this path still answered `timer` @ 308 ms.
///
/// The block-in-place gate of the time required `native_reentry == 0`, so a `main` thread with a
/// host frame under it fell through to the inline-sleep. The exclusion was about the deadlock
/// verdict being unable to JUDGE such a party — not about whether it may block — and a LIVE TIMER
/// ARM removes that risk, because the wait provably ends at the deadline. The first fix was a
/// separate `timed_block` term; TICKET-062 made it redundant, and `block::mode` now blocks every
/// `OwnThread` `wait:` in place (`vm_wait_in_native_callback_no_sender_deadlocks` fences the
/// no-timer half).
#[test]
fn a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
t: Channel[bool] = time.timer(3000)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
fn pick(x: int) -> int:
    gate.send(true)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
    return x
ex := Executor()
ex.submit(prod)
_ := [1].map(pick)
ex.shutdown()
";
    let entry = write_temp_chz("w714_callback_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect("the in-callback `wait:` hung");
    assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
    assert_eq!(
        out, "value 9\n",
        "a `main` thread inside a native callback owns its OS thread too — the 50 ms value must beat \
         the 3 s timer arm there as well (W7-14, adversarial-review round)"
    );
}

/// W7-14's second half, found while measuring the fix — **a timer arm made an eager `wait:`
/// UNCANCELLABLE**, and the job then ran the timer arm's body after the cancel was requested.
///
/// `std::thread::sleep(deadline - now)` observes nothing. `op_wait_poll`'s cancellation checkpoint is
/// at the top of the op, so it can only fire if something returns to the dispatch loop — and the
/// inline-sleep never did until the deadline had passed, at which point it TOOK the timer arm. So
/// `cancel()` (D4's cooperative stop) could not interrupt `wait:` over `timer(3000)`:
///
/// | | before | after |
/// |---|---|---|
/// | `cancel()` at 50 ms, job waiting on `timer(3000)` | `timer` printed, exit @ **3007 ms** | nothing printed, exit @ **57 ms** |
///
/// Measured on release binaries built in separate target dirs. The block-in-place path re-checks
/// `block_halt_check` (cancel included) once per tick, so the cancel now lands within one
/// `DEMOTE_POLL_BACKOFF` instead of one timer deadline — and the arm body never runs.
#[test]
fn a_timer_armed_eager_wait_is_cancellable_by_cancel() {
    let src = "
import Executor from std.concurrency
import std.time\nimport timer from std.time
work: Channel[int] = Channel[int](1)
ready: Channel[int] = Channel[int](1)
fn cons():
    t := time.timer(3000)
    ready.send(0)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
ex := Executor()
ex.submit(cons)
_ := ready.recv()
ex.cancel()
print(\"main done\")
";
    let entry = write_temp_chz("w714_cancel_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}")));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg) = outcome.expect("the cancelled `wait:` hung");
    assert!(
        !out.contains("timer"),
        "a cancelled job must not run its timer arm's body: out={out:?} r={rdbg}"
    );
}

/// W7-16 — a nursery task **already inside** a `sleep_ms` must die when a sibling faults, not at its
/// own deadline. This is the case the existing parity fence never covered: with the fault ORDERED
/// AFTER the sleep begins, `parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines`'s
/// contract is delivered by the *entry* checkpoint at `invoke_native` — reorder the fault by 50 ms and
/// pre-fix M:N ran the full 3005 ms and printed `napper woke`.
///
/// | | before | after |
/// |---|---|---|
/// | sibling faults at ~100 ms, task sleeping 3 s | `napper woke` printed, exit @ **3005 ms** | nothing printed, exit @ **~105 ms** |
///
/// **M:N-only, deliberately.** Serial cannot preempt: nothing else runs while the fiber sleeps, so the
/// fault can only precede or follow the sleep and serial keeps taking the entry checkpoint. Asserting
/// this shape would be asserting a physics claim, not a contract — `CLAUDE.md`'s
/// "correctness outranks engine agreement". The `--timeout` half of the same contract IS engine-common
/// and is fenced in `test_runner` (`timeout_aborts_a_sleeping_test_everywhere`).
///
/// The `gate` handshake is what makes the ordering deterministic rather than raced: `boom` cannot
/// fault until `napper` has announced it is about to sleep, and then waits another 100 ms. (Losing
/// that race would only fall back to the entry checkpoint — the test cannot flake red.)
#[test]
fn a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault() {
    let src = "
import std.time\nimport timer from std.time
gate: Channel[int] = Channel[int](1)
fn napper():
    defer print(\"cleanup ran\")
    print(\"napper start\")
    gate.send(0)
    time.sleep_ms(3000)
    print(\"napper woke\")
fn boom() -> int:
    _ := gate.recv()
    time.sleep_ms(100)
    return 1 / 0
fn main():
    r := recover:
        parallel:
            spawn napper()
            spawn boom()
        0
    print(\"end\")
main()
";
    let entry = write_temp_chz("w716_nursery_mid_flight", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file(&e);
        let _ = tx.send((o, format!("{r:?}"), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, elapsed) = outcome.expect("the cancelled sleeper hung");
    assert!(
        !out.contains("napper woke"),
        "a task cancelled DURING its sleep must not run past it: out={out:?} r={rdbg}"
    );
    // …and its `defer` still runs. The cancel is delivered by the timer job as a resumed `Err`, an
    // arm that returns WITHOUT re-entering `run_until` — so it must unwind explicitly or every
    // registered cleanup is silently skipped, while the same cancel arriving 50 ms earlier (the entry
    // checkpoint, which faults inside the VM) runs them. Caught by adversarial review, not by the
    // "did it stop promptly" assertions above: a task that skips its cleanup stops just as promptly.
    assert!(
        out.contains("cleanup ran"),
        "a task cancelled DURING its sleep must still unwind through its `defer`s \
         (docs/reference/concurrency/: cleanup does not depend on scheduler timing): out={out:?} r={rdbg}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the cancel took {elapsed:?} to reach a sleeping sibling (fixed: ~105 ms, \
         un-rearmed timer offload: ~3005 ms)"
    );
}

/// W7-5b — an `Executor` constructed INSIDE a task, never explicitly shut down, must still have its
/// work run and waited for at program exit.
///
/// It did not before: `Vm.executors` is a `Vec<GcRef>`, heap-keyed and swapped per fiber, so a nested
/// executor landed in the task's throwaway worker list and was dropped when the task finished — its
/// jobs never ran, were never reaped, and raised no fault. Verified on the pre-change binary: M:N
/// printed `main done` alone, while the since-removed cooperative engine (one shared heap, so its
/// list survived) ran both jobs — which is how the heap-keying was identified as the cause.
/// The fix is the heap-independent `ExecRegistry`, which every worker shares.
///
/// The jobs print their own evidence rather than a counter being read at the end, because the exit
/// join happens AFTER the last top-level statement — a `print` of an accumulator would race the jobs
/// it is trying to observe. Compared as a line SET: the jobs are concurrent, so their order is free.
#[test]
fn executor_created_inside_a_task_is_joined_at_exit_both_engines() {
    let src = r#"
import Executor from std.concurrency
parallel:
    spawn:
        inner := Executor()
        inner.submit(fn(): print("inner job A"))
        inner.submit(fn(): print("inner job B"))
print("main done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit join must not hang");
    let mut want = ["inner job A", "inner job B", "main done"];
    want.sort_unstable();
    for (engine, got) in [("serial", serial), ("mn", mn)] {
        let mut lines: Vec<&str> = got
            .as_deref()
            .expect("run")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, want, "{engine}: a task-created executor lost work");
    }
}

/// An `Executor` created BY a job that the program-exit join is itself running must also be joined.
///
/// Pre-existing and silent: the exit reap iterated a SNAPSHOT of the executor list
/// taken before it started, so an executor born during the reap was never in it and its work simply
/// vanished (verified on the pre-change binary — neither engine printed the inner line). Both sides
/// now re-scan until no un-shut executor is left, which terminates because `shutdown` marks a core
/// `shut` before running anything.
///
/// Worth a test of its own rather than folding into the W7-5b case: that one is about an executor
/// created inside a TASK (a heap-visibility problem), this one is about an executor created inside the
/// JOIN (an iteration-order problem). Fixing either does not fix the other.
#[test]
fn executor_created_by_a_joined_job_is_also_joined_both_engines() {
    let src = r#"
import Executor from std.concurrency
fn makes_another():
    inner := Executor()
    inner.submit(fn(): print("job created BY a job"))
ex := Executor()
ex.submit(makes_another)
print("main done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit join must not hang");
    let mut want = ["job created BY a job", "main done"];
    want.sort_unstable();
    for (engine, got) in [("serial", serial), ("mn", mn)] {
        let mut lines: Vec<&str> = got
            .as_deref()
            .expect("run")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort_unstable();
        assert_eq!(
            lines, want,
            "{engine}: an executor born during the join was lost"
        );
    }
}

/// `cancel()` must reach a job running inside a NESTED executor, not just the job it submitted
/// directly.
///
/// `spawn_into` was the one seam in the tree that installed a cancel token WITHOUT the
/// `scope_ancestors()` half every nursery seam pairs it with (`spawn_shell`, `run_one_fiber`), so an
/// inner executor's job had the flag chain `[inner.cancel]` and never observed the outer trip. The
/// program then paid the full inner sleep at the exit drain.
///
/// The inconsistency this pins is Chezzi-vs-Chezzi, not a borrowed ancestor rule: `outer`'s own
/// `sleep_ms(8000)` is already cancelled at 50 ms (its chain holds the outer flag), while the
/// IDENTICAL sleep one executor deeper ran to completion. Measured ancestors are split — CPython
/// `ThreadPoolExecutor` does not propagate (nested pool: 8.04 s wall, `nap finished` printed, for both
/// `shutdown(wait=False)` and `wait=True`), Go's derived `context.WithCancel(parent)` does (child
/// cancelled at 50 ms) — so the deciding argument is W7-16's, applied one level down: an executor that
/// disagrees with the nursery beside it is the defect.
///
/// Depends on eager `submit`: under a lazy queue-and-drain model there is no running inner job to
/// cancel. Wall-clock asserted under a `recv_timeout`
/// watchdog so a regression FAILS loud instead of hanging the suite for 8 s a run.
#[test]
fn nested_executor_job_is_cancelled_by_an_outer_cancel_mn() {
    let src = r#"
import Executor from std.concurrency
import std.time
import timer from std.time
started: Channel[int] = Channel[int](1)
fn nap():
    time.sleep_ms(8000)
    print("nap finished")
fn outer():
    inner := Executor()
    inner.submit(nap)
    started.send(0)
    time.sleep_ms(8000)
    print("outer finished")
ex := Executor()
ex.submit(outer)
_ := started.recv()
ex.cancel()
print("done")
"#;
    let entry = write_temp_chz("nested_cancel", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = run_file(&run_entry);
        let _ = tx.send(out);
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, _err, res, _code) =
        result.expect("hung: an outer cancel never reached the nested executor's job");
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        !out.contains("nap finished"),
        "the nested job outlived the outer cancel: {out:?}"
    );
    assert!(
        !out.contains("outer finished"),
        "the outer cancel did not cancel its own job: {out:?}"
    );
    assert!(out.contains("done"), "main must still finish: {out:?}");
}

/// The NEGATIVE twin of the test above: a `cancel()` must reach only the executors its own job
/// CREATED, never one that merely had work submitted to it from inside that job.
///
/// An `Executor` crosses a spawn by `Arc`, so any job can `submit` to any executor it can name. A
/// first cut keyed cancel inheritance on the SUBMITTER (`if self.the job's sched.is_some() {
/// rw.worker.cancel_outer = self.scope_ancestors() }`), which handed `other`'s cancel chain to a job
/// belonging to `main`'s executor: `other.cancel()` then killed an already-started job that was
/// none of its business, AND `shared.shutdown()` — a GRACEFUL join that promises to wait for its work
/// — returned silently with that work dropped ("job started" printed, "shared job ran" never; before
/// the nested-cancel change both printed). Keying it on the executor's CREATOR
/// (`ExecutorCore::creator_cancel`, captured at `Op::NewExecutor`) is what makes both this and the
/// nested case right at once: `shared` was created by `main`, so it inherits nothing from anyone.
///
/// Depends on eager `submit`, for the same reason as its twin.
#[test]
fn a_job_submitted_to_mains_executor_survives_another_executors_cancel_mn() {
    let src = r#"
import Executor from std.concurrency
import std.time
import timer from std.time
started: Channel[int] = Channel[int](1)
fn work():
    time.sleep_ms(300)
    print("shared job ran")
fn jobber():
    print("job started")
    shared.submit(work)
    started.send(0)
    time.sleep_ms(8000)
    print("jobber finished")
shared := Executor()
other := Executor()
other.submit(jobber)
_ := started.recv()
other.cancel()
shared.shutdown()
print("done")
"#;
    let entry = write_temp_chz("cross_executor_cancel", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = run_file(&run_entry);
        let _ = tx.send(out);
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, _err, res, _code) = result.expect("hung");
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("job started"),
        "the outer job must run: {out:?}"
    );
    assert!(
        out.contains("shared job ran"),
        "an unrelated executor's cancel killed main's executor's job: {out:?}"
    );
    assert!(out.contains("done"), "main must still finish: {out:?}");
    assert!(
        !out.contains("jobber finished"),
        "the outer job's own sleep was not cancelled by other.cancel(): {out:?}"
    );
}

/// The THIRD case in the W7-39 family, and the one the two tests above left silent: an executor
/// CREATED inside an eager job, whose handle outlives the job's cancellation.
///
/// `creator_cancel` is captured once at `Op::NewExecutor` and never reset — sticky by design, the way
/// Go's derived `context.WithCancel(parent)` stays cancelled once the parent is. So after the creating
/// job's executor is `cancel`-ed, EVERY later job this core dispatches starts already-cancelled.
/// That is correct; what was indefensible is that it was SILENT: `main` holds the only handle here,
/// its `submit` vanished, and its own GRACEFUL `shutdown()` — which promises to wait for its work —
/// returned having run nothing (measured pre-fix: `main done`, rc=0, `inner job ran` never printed).
/// A `submit` the executor cannot honour now settles its Task with the refusal, exactly like a
/// `submit` after `shutdown()` (TICKET-289: neither ends the run).
///
/// **The checkpoint trap**: `work` MUST contain a cancellation point (the `sleep_ms`). With a bare
/// `print` the job finishes before reaching one and runs to completion (D4 is cooperative, not
/// preemptive) — a test written without it is a false green that proves nothing.
///
/// Depends on eager `submit`: `creator_cancel` is captured at `Op::NewExecutor` and only ever read
/// on the eager path.
#[test]
fn submit_to_an_executor_whose_creating_job_was_cancelled_faults_mn() {
    let src = r#"
import Executor from std.concurrency
import std.time
import timer from std.time
fn work():
    time.sleep_ms(10)
    print("inner job ran")
fn jobber(ch: Channel[Executor]):
    inner := Executor()
    ch.send(inner)
    time.sleep_ms(2000)
ch := Channel[Executor](1)
outer := Executor()
outer.submit(fn(): jobber(ch))
inner := ch.recv() else: panic("ch closed")
time.sleep_ms(50)
outer.cancel()
t := inner.submit(work)
inner.shutdown()
print("main done")
match t.get():
    ?_: print("accepted")
    !e: print("task fault: {e.message()}")
"#;
    let entry = write_temp_chz("submit_after_creator_cancel", src);
    let (out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    res.expect("a refused submit settles its Task; it does not end the run");
    assert!(
        out.contains("task fault: submit on an Executor whose creating job was cancelled"),
        "the Task must hold the refusal: {out:?}"
    );
    assert!(
        !out.contains("inner job ran") && !out.contains("accepted"),
        "the poisoned submit must not run: {out:?}"
    );
}

/// Negative control 1 — the SAME program with a graceful `outer.shutdown()`. Nothing is cancelled, so
/// the inherited chain stays untripped and the submitted job runs normally.
#[test]
fn submit_to_a_nested_executor_after_a_graceful_outer_shutdown_runs_mn() {
    let src = r#"
import Executor from std.concurrency
import std.time
import timer from std.time
fn work():
    time.sleep_ms(10)
    print("inner job ran")
fn jobber(ch: Channel[Executor]):
    inner := Executor()
    ch.send(inner)
    time.sleep_ms(200)
ch := Channel[Executor](1)
outer := Executor()
outer.submit(fn(): jobber(ch))
inner := ch.recv() else: panic("ch closed")
time.sleep_ms(50)
outer.shutdown()
inner.submit(work)
inner.shutdown()
print("main done")
"#;
    let entry = write_temp_chz("submit_after_creator_graceful", src);
    let (out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("inner job ran"),
        "the job was dropped: {out:?}"
    );
    assert!(out.contains("main done"), "main must finish: {out:?}");
}

/// Negative control 2 — an executor created by `main` has NO `creator_cancel` at all, so an unrelated
/// executor's `cancel()` cannot reach it. This is the case the new guard must leave untouched.
#[test]
fn submit_to_mains_own_executor_after_an_unrelated_cancel_runs_mn() {
    let src = r#"
import Executor from std.concurrency
import std.time
import timer from std.time
fn work():
    time.sleep_ms(10)
    print("mine ran")
fn nap():
    time.sleep_ms(8000)
mine := Executor()
other := Executor()
other.submit(nap)
time.sleep_ms(50)
other.cancel()
mine.submit(work)
mine.shutdown()
print("done")
"#;
    let entry = write_temp_chz("submit_mains_own_after_unrelated_now", src);
    let (out, _err, res, _code) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("mine ran"),
        "main's own job was dropped: {out:?}"
    );
    assert!(out.contains("done"), "main must finish: {out:?}");
}

/// TICKET-270 — a job that ends the run (an `os.exit`) keeps its own output wherever main is when
/// the run end lands. The `quiesce` seam holds the window between the run-end cell and the job's
/// slot store open for 300 ms. Measured on the unfixed engine: every `exit/` cell red with
/// `out=""`. TICKET-289 removed the `fault/` cells: a job fault no longer ends the run, it stays in
/// the job's Task.
#[test]
fn a_job_that_ends_the_run_keeps_its_output_at_every_drain_position() {
    let ends = [("exit", "    os.exit(3)\n", 3)];
    let positions = [
        ("alone", "b := Executor()\nb.submit(job)\n"),
        (
            "same_exec_waiter",
            "b := Executor()\nb.submit(waiter)\nb.submit(job)\n",
        ),
        (
            "other_exec_first",
            "a := Executor()\na.submit(waiter)\nb := Executor()\nb.submit(job)\n",
        ),
        (
            "other_exec_after",
            "b := Executor()\nb.submit(job)\na := Executor()\na.submit(waiter)\n",
        ),
        (
            "main_recv",
            "b := Executor()\nb.submit(job)\n_ := gate.recv()\n",
        ),
        (
            "main_recv_other_first",
            "a := Executor()\na.submit(waiter)\nb := Executor()\nb.submit(job)\n_ := gate.recv()\n",
        ),
    ];
    let mut red = Vec::new();
    for (end, body, code) in ends {
        for (pos, tail) in positions {
            let src = format!(
                "import Executor from std.concurrency\nimport std.os\ngate: Channel[int] = Channel[int]()\nfn waiter():\n    _ := gate.recv()\nfn job():\n    print(\"job output\")\n{body}{tail}"
            );
            let entry = write_temp_chz("t270_run_end_window", &src);
            let (out, _e, r, c) = run_file(&entry);
            let _ = std::fs::remove_file(&entry);
            let rs = format!("{r:?}");
            let ended = c == Some(code);
            if !out.contains("job output") || !ended {
                red.push(format!("{end}/{pos}: out={out:?} code={c:?} r={rs}"));
            }
        }
    }
    assert!(red.is_empty(), "red cells: {red:?}");
}

/// TICKET-270 — W7-57's fence: an `os.exit` beside a task that spins in its `defer` still ends
/// the run, and the exiting party's output is kept. A flush waits until the exiting fiber stored
/// its slot, so an exit whose unwind waited on the spinner would hang here. Measured on the
/// unfixed engine: every cell ends; `job_body_exits`, `job_child_exits` and
/// `job_exits_other_exec_spins` lose `quit`.
#[test]
fn an_exit_beside_a_spinning_defer_ends_the_run_and_keeps_its_output() {
    let head = "import Executor from std.concurrency\nimport std.os\nch: Channel[int] = Channel[int]()\nfn spin():\n    defer:\n        while true:\n            pass\n    _ := ch.recv()\nfn quit():\n    print(\"quit\")\n    os.exit(3)\n";
    let cells = [
        (
            "job_body_exits",
            "fn job():\n    parallel:\n        spawn spin()\n        print(\"quit\")\n        os.exit(3)\nb := Executor()\nb.submit(job)\n",
        ),
        (
            "job_child_exits",
            "fn job():\n    parallel:\n        spawn spin()\n        spawn quit()\nb := Executor()\nb.submit(job)\n",
        ),
        (
            "main_child_exits",
            "parallel:\n    spawn spin()\n    spawn quit()\n",
        ),
        (
            "job_exits_other_exec_spins",
            "a := Executor()\na.submit(spin)\nb := Executor()\nb.submit(quit)\n_ := ch.recv()\n",
        ),
    ];
    let mut red = Vec::new();
    for (name, tail) in cells {
        let entry = write_temp_chz("t270_exit_beside_spin", &format!("{head}{tail}"));
        let e = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file(&e));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok((out, _e, _r, c)) if out.contains("quit") && c == Some(3) => {}
            Ok((out, _e, r, c)) => red.push(format!("{name}: out={out:?} code={c:?} r={r:?}")),
            Err(_) => red.push(format!("{name}: no end in 60 s")),
        }
        let _ = std::fs::remove_file(&entry);
    }
    assert!(red.is_empty(), "red cells: {red:?}");
}

/// TICKET-270 — DEC-213 beside a RUNNING sibling: a job exits while a task of its own nursery
/// spins in a CPU loop (not parked) with a `defer` that prints. The exit code must be readable
/// before the exit's cancel reaches the spinner (`finish_with` trips it under the core lock with
/// the slot), so no `defer` runs; the job's output is kept and the code is 3. 20 runs at
/// `CHEZZI_THREADS=1` and 20 at `=2`. Measured: base engine with the seams 20 red at T=2
/// (`out=""`); the round-3 code, which stored a fiber's exit only in `finish`, 19 red at T=2
/// (`defer ran`).
#[test]
fn an_exit_beside_a_running_sibling_runs_no_defer_and_keeps_its_output() {
    if crate::vm::rerun_in_child(
        module_path!(),
        "an_exit_beside_a_running_sibling_runs_no_defer_and_keeps_its_output",
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
    let src = "import Executor from std.concurrency\nimport std.os\nstarted: Channel[int] = Channel[int]()\nfn busy():\n    defer:\n        print(\"defer ran\")\n    started.send(1)\n    i := 0\n    while true:\n        i += 1\nfn job():\n    parallel:\n        spawn busy()\n        _ := started.recv()\n        print(\"job output\")\n        os.exit(3)\nb := Executor()\nb.submit(job)\n";
    let mut red = Vec::new();
    for t in [1usize, 2] {
        crate::vm::set_worker_count(t);
        for i in 0..20 {
            let entry = write_temp_chz(&format!("t270_running_sibling_{t}_{i}"), src);
            let e = entry.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(run_file(&e));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(60)) {
                Ok((out, _e, _r, c))
                    if out.contains("job output") && !out.contains("defer ran") && c == Some(3) => {
                }
                Ok((out, _e, r, c)) => {
                    red.push(format!("T={t} run {i}: out={out:?} code={c:?} r={r:?}"))
                }
                Err(_) => red.push(format!("T={t} run {i}: no end in 60 s")),
            }
            let _ = std::fs::remove_file(&entry);
        }
    }
    assert!(red.is_empty(), "{} red run(s): {red:?}", red.len());
}

/// TICKET-284 — a nursery child of an Executor job calls `os.exit` and the job adopts the exit.
/// The child is the exit origin, so `take_finished_streams` waits for the child's slot only; the
/// job's join takes that slot, and the job's own slot, which holds the taken output, is stored
/// later. This drives the sched in that order by hand: the child stores its `Exit` slot and ends
/// its count, the job's join takes the slot, a reader runs, then the job stores its slot and ends
/// its count. The reader must not return before the output is back in a slot.
#[test]
fn a_job_that_adopts_its_childs_exit_keeps_the_childs_output() {
    let sched = Arc::new(mk_sched(1)); // scope 0: the job, slot 0
    let child = sched.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // slot 1
    sched.quiesce.begin_exit_origin();
    sched.lock().slots[1] = Some(TaskOutcome::Exit {
        code: 3,
        out: b"quit\n".to_vec(),
        stderr: Vec::new(),
    });
    sched.quiesce.end_exit_origin();
    let taken = sched.take_scope_slots(child);
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = Arc::clone(&sched);
    std::thread::spawn(move || {
        let _ = tx.send(reader.take_finished_streams().0);
    });
    let early = rx.recv_timeout(std::time::Duration::from_secs(2));
    sched.lock().slots[0] = taken.into_iter().flatten().next();
    sched.quiesce.end_exit_origin();
    let out = match early {
        Ok(out) => out,
        Err(_) => rx
            .recv()
            .expect("the reader returns once the job stored its slot"),
    };
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.contains("quit"),
        "job_child_exits: out={out:?} code=Some(3) (the reader ran after the job took the slot)"
    );
}
