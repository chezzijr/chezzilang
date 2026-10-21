//! Golden tests: cancellation points and defer, Writer/Reader, atomics, task-local module globals,
//! module scope verdicts.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

/// BUG 1 / the PROBE shape — no token: `consumer` PRINTS, THEN registers `defer cleanup(s)`, then
/// parks; `boom` faults immediately. With cancel observed at EVERY instruction the consumer could be
/// killed between its first statement and its `defer` line, so the defer never registered and cleanup
/// silently never ran (measured: 0/20 on M:N). With CANCELLATION POINTS (loop back-edges + blocking
/// ops) a STARTED task always runs its straight-line prologue, so the defer is registered by
/// construction — deterministically 42. The pre-defer `print` must survive too (a cancelled M:N
/// task's buffer is flushed at its task slot; the since-removed serial engine printed live).
#[test]
fn parity_probe_defer_runs_when_cancelled_before_its_defer_line() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn consumer(ch: Channel[int], s: Shared[int]):\n    print(\"consumer started\")\n    defer cleanup(s)\n    ch.recv()\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        parallel:\n            spawn consumer(ch, s)\n            spawn boom()\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture(src).expect("the fault is recovered, so the program completes");
    assert!(
        serial.contains("42\n"),
        "serial: the started consumer's defer ran: {serial:?}"
    );
    assert!(
        mn.contains("42\n"),
        "M:N: the started consumer's defer ran: {mn:?}"
    );
    // Cross-task ORDER is nondeterministic; the line SET is the contract.
    assert_same_lines(&serial, &mn);
}

/// `os.exit` inside a CANCELLED task's `defer`: the exit code wins and is
/// never demoted to the sibling's catchable fault (Exit > Fault, lowest task index wins via
/// M:N's `reduce_task_slots`). The line SET is asserted; order is not.
#[test]
fn parity_os_exit_inside_a_cancelled_tasks_defer() {
    let src = "import std.os\n\
               fn bye():\n    print(\"cleanup\")\n    os.exit(7)\n\
               fn consumer(ch: Channel[int], go: Channel[int]):\n    defer bye()\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    r := recover:\n        parallel:\n            spawn consumer(ch, go)\n            spawn boom(go)\n        0\n    print(\"unreachable\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mo, _me, _mr, mc) = run_file(&entry);
    let (so, _se, _sr, sc) = run_file(&entry);
    assert_eq!(sc, Some(7), "serial: the cancelled defer's os.exit wins");
    assert_eq!(mc, Some(7), "M:N: the cancelled defer's os.exit wins");
    assert!(
        !so.contains("unreachable") && !mo.contains("unreachable"),
        "os.exit hard-halts: serial={so:?} mn={mo:?}"
    );
    assert_same_lines(&so, &mo);
}

/// The probe shape with the FAULTER SPAWNED FIRST — the shape that exposed the deterministic
/// serial-vs-M:N divergence the cancellation-point change first shipped with. M:N is structurally
/// forced to start every spawned fiber (a scope completes only at `done == total`; `take_runnable`
/// never consults the scope cancel), so `talker` runs its prologue, prints, registers its `defer` and
/// unwinds at its `recv` checkpoint — even though `boom` already faulted. Serial's cancel drain used
/// to SKIP a never-started (`Pending`) sibling, so it emitted `{"0"}` while M:N emitted `{"hi","42"}`
/// (20/20 — near-deterministic, not a rare race). The drain now starts every not-`Done` sibling with
/// the cancel tripped, so the task runs and its `defer` runs too.
#[test]
fn parity_probe_faulter_spawned_first_still_runs_the_siblings_defer() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn talker(ch: Channel[int], s: Shared[int]):\n    print(\"hi\")\n    defer cleanup(s)\n    ch.recv()\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        parallel:\n            spawn boom()\n            spawn talker(ch, s)\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture(src).expect("the fault is recovered, so the program completes");
    assert!(serial.contains("42\n"), "serial: the defer ran: {serial:?}");
    assert!(mn.contains("42\n"), "M:N: the defer ran: {mn:?}");
    assert!(
        serial.contains("hi\n"),
        "serial: the prologue ran: {serial:?}"
    );
    assert_same_lines(&serial, &mn);
}

/// A never-started straight-line sibling with NO defer, faulter first: M:N runs it to completion
/// (`Done`, output flushed) — the line SET is the contract.
#[test]
fn parity_straight_line_sibling_runs_even_when_the_scope_is_already_cancelled() {
    let src = "fn noisy():\n    print(\"hi\")\n\
               fn boom():\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn noisy()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture(src).expect("recovered");
    assert!(serial.contains("hi\n"), "serial: {serial:?}");
    assert_same_lines(&serial, &mn);
}

/// Cancellation points and a NATIVE-driven loop: `xs.map(f)` iterates in RUST (`for e in ..
/// guarded(|vm| vm.invoke_value(f, ..))`, call.rs) and emits no `Op::Jump`, so `jump_checked`'s
/// back-edge cannot fire inside it and a straight-line `f` has no back-edge of its own. Without a
/// checkpoint at the native re-entry a cancelled task burns every remaining element to completion
/// (measured on the first cut of this change: "map finished" printed after the sibling had faulted,
/// 5M callbacks deep). The `guarded` checkpoint delivers the cancel between elements — that Rust
/// loop's back-edge.
///
/// The faulter is spawned FIRST so the scope cancel is already tripped when `work` starts: no timing
/// race — M:N must abort the map, popping its still-queued fiber after the cancel (the since-removed
/// serial engine's drain used to start `work` with the cancel already tripped). A CPU-bound task that
/// is ALREADY RUNNING when the cancel trips was a different, pre-existing story on the since-removed
/// serial engine — it was cooperative, so it simply ran to its next park before any sibling got to
/// fault (that is why the `while true:` spinner test is M:N-only).
#[test]
fn parity_native_hof_loop_is_cancellable() {
    let src = "fn sq(x: int) -> int:\n    return x * x\n\
               fn work(xs: List[int]):\n    ys := xs.map(sq)\n    print(\"map finished\")\n    print(ys.len())\n\
               fn boom():\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    xs := []\n    i := 0\n    while i < 200000:\n        xs.push(i)\n        i = i + 1\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn work(xs)\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture(src).expect("recovered");
    assert!(
        !serial.contains("map finished"),
        "serial: the cancelled task's native map must abort at a per-element checkpoint: {serial:?}"
    );
    assert!(
        !mn.contains("map finished"),
        "M:N: the cancelled task's native map must abort at a per-element checkpoint: {mn:?}"
    );
    assert_same_lines(&serial, &mn);
}

/// A NESTED nursery's deadlock, with an outer sibling PARKED and holding a registered `defer`, is
/// FATAL through an outer `recover:` (TICKET-135, D1): the `recover:` is transparent to the verdict,
/// so the program aborts and the `print(s.get())` after it never runs.
#[test]
fn nested_deadlock_is_fatal_through_an_outer_recover() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn a(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn dead(d: Channel[int]):\n    d.recv()\n\
               fn b(go: Channel[int]):\n    go.recv()\n    d := Channel[int]()\n    parallel:\n        spawn dead(d)\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn a(ch, go, s)\n            spawn b(go)\n        0\n    print(s.get())\nmain()\n";
    let err = run_capture(src).expect_err("the nested deadlock is fatal");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// A GENUINE deadlock (every task parked, nothing cancelled) is still DETECTED — not hung — and is
/// FATAL through `recover:` (TICKET-135, D1). The cancel drain must never swallow it: it is reported
/// from `run_scheduler_level`'s `None` arm, which never routes through `drain_cancelled_children`.
#[test]
fn genuine_deadlock_is_detected_and_fatal() {
    let src = "fn waiter(ch: Channel[int]):\n    ch.recv()\n\
               fn main():\n    ch := Channel[int]()\n    r := recover:\n        parallel:\n            spawn waiter(ch)\n            spawn waiter(ch)\n        0\n    print(\"caught\")\nmain()\n";
    let err = run_capture(src).expect_err("the deadlock is fatal");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// A `defer` is the cleanup the cancel exists to RUN — so no cancellation checkpoint fires INSIDE a
/// deferred call (`Vm::deferring` ⇒ `cancel_requested()` is false). Without that, every defer of a
/// task that ends on the NORMAL-return path (or faults on its own) while a sibling has already
/// tripped the scope cancel hit the checkpoint at the top of `guarded` (`run_one_deferred` runs every
/// deferred call through it) with `cancelled` still false: the FIRST (LIFO) deferred call returned
/// `cancelled` BEFORE its body ran, and only the rest of the defers executed. Measured on both
/// engines: `cleanup2` was silently swallowed — arbitrary PARTIAL cleanup (one fd released, the next
/// not), while parity stayed green because both engines dropped it identically.
#[test]
fn parity_every_defer_of_a_normally_returning_task_runs_under_a_tripped_cancel() {
    let src = "fn boom() -> int:\n    return 1 / 0\n\
               fn tidy():\n    defer print(\"cleanup1\")\n    defer print(\"cleanup2\")\n    print(\"start\")\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn tidy()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered");
    let mn = run_capture(src).expect("recovered");
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("cleanup1\n") && out.contains("cleanup2\n"),
            "{engine}: EVERY registered defer runs (LIFO), not all-but-the-first: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// Structured concurrency — cancelling a scope cancels its DESCENDANT scopes. A nested `parallel:`
/// entered from a task that is then cancelled used to be UNCANCELLABLE: the nested scope got a fresh
/// cancel flag (M:N) / serial handed the level `cancel = None`, so the nested spinner's back-edge
/// checkpoint had no tripped flag to read and looped forever — the teardown never finished and the
/// program HUNG on BOTH engines (measured: `timeout` on both). The nested scope now INHERITS its
/// enclosing scopes' flags (`JoinScope::ancestors` / `Vm::cancel_outer`; serial inherits the `Arc`).
#[test]
fn parity_nested_nursery_inside_a_cancelled_task_is_cancellable() {
    let src = "fn boom() -> int:\n    return 1 / 0\n\
               fn spin():\n    i := 0\n    while true:\n        i = i + 1\n\
               fn t():\n    parallel:\n        spawn spin()\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn t()\n        0\n    print(\"end\")\nmain()\n";
    let serial = run_capture(src).expect("recovered — must not hang");
    let mn = run_capture(src).expect("recovered — must not hang");
    assert_eq!(serial, "end\n", "serial: the nested spinner was cancelled");
    assert_eq!(mn, "end\n", "M:N: the nested spinner was cancelled");
}

/// A blocking native (`sleep_ms` / `io.*` / `fs.*` / `request`) is an **ENTRY** cancellation
/// checkpoint — the check sits OUTSIDE the M:N-only offload gate. It used to be
/// M:N-only, so a cancelled SERIAL task slept the full duration (stalling the whole teardown —
/// `sleep_ms(60000)` would freeze it for a minute) and then, having no other checkpoint, ran every
/// straight-line statement AFTER the sleep to completion: `{napper start, napper woke, end}` on the
/// since-removed serial engine vs `{napper start, end}` on M:N, deterministically. The line SET is the contract.
///
/// **Scope, tightened by W7-16.** This fence covers only the case where the cancel is ALREADY TRIPPED
/// when the blocking call is reached — which is what its fixture produced by accident, `boom()`
/// faulting before `napper` could enter the sleep. It said nothing about a cancel arriving DURING the
/// sleep, and that case was broken (measured 3005 ms M:N / 3054 ms serial, `napper
/// woke` printed) until W7-16. The mid-flight half is
/// `tests::a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault`, M:N-only because
/// serial cannot preempt a sleeping fiber at all (nothing else runs), so its line SET legitimately
/// differs there.
///
/// The fixture is left ungated ON PURPOSE: `boom()` divides by zero as its first act while `napper`
/// still has a `print` ahead of the sleep, so the cancel is reliably tripped BEFORE the blocking call
/// is reached — which is the ordering this fence is for. A `Channel` handshake would only move the
/// race, not remove it (nothing can order "boom's fault" against "napper's sleep ENTRY" from inside
/// the language), and would blur which checkpoint is under test.
#[test]
fn parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines() {
    let src = "import std.time\n\
               fn boom() -> int:\n    return 1 / 0\n\
               fn napper():\n    print(\"napper start\")\n    time.sleep_ms(3000)\n    print(\"napper woke\")\n\
               fn main():\n    r := recover:\n        parallel:\n            spawn boom()\n            spawn napper()\n        0\n    print(\"end\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let t0 = std::time::Instant::now();
    let (mn, _me, _mr, _mc) = run_file(&entry);
    let (serial, _se, _sr, _sc) = run_file(&entry);
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            !out.contains("napper woke"),
            "{engine}: the cancelled task dies AT the blocking native, and never runs past it: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
    assert!(
        t0.elapsed() < std::time::Duration::from_millis(1500),
        "neither engine's teardown waits out the cancelled task's 3s sleep: {:?}",
        t0.elapsed()
    );
}

/// Run one engine under a HARD DEADLINE: a scheduler bug in this area is a HANG, and a hung test
/// would just wedge the suite. On timeout we panic with the engine name (the hung VM thread is left
/// to die with the test process).
#[cfg(test)]
fn run_with_deadline(
    engine: &str,
    f: impl FnOnce() -> (String, Result<(), RuntimeError>) + Send + 'static,
) -> (String, Result<(), RuntimeError>) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let _ = tx.send(f());
        })
        .expect("failed to spawn the deadline thread");
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .unwrap_or_else(|_| {
            panic!("{engine} HUNG: a defer that can never complete must be REPORTED as a deadlock, never a silent hang")
        })
}

/// A `defer` that can NEVER complete (its body `recv`s on a channel nobody will ever send to) inside
/// a CANCELLED task is REPORTED as a deadlock — never a silent hang. A defer is not itself cancellable
/// (`cancel_requested`'s `deferring == 0`), so on M:N that cleanup demotes and blocks in place; the
/// scope is then incomplete BECAUSE of it, which made the N4 cancel-teardown veto
/// (`any_cancelled_scope_awaiting_drain`, mod.rs) permanent and the M:N engine hung SILENTLY while
/// serial reported (measured: M:N `timeout` rc=124, serial rc=1). The veto is now bounded to the
/// trip→`cancel_drain` window (an UNDRAINED PARKED fiber), so the quiesce is detected, the demoted
/// fiber faults in place, and — the task being already cancelled — its error is swallowed and the
/// SIBLING'S real fault is what propagates, identically.
#[test]
fn parity_a_defer_that_can_never_complete_is_reported_not_hung() {
    let src = "fn cleanup(dead: Channel[int]):\n    print(\"CLEANUP-ENTER\")\n    dead.recv()\n    print(\"CLEANUP-DONE\")\n\
               fn consumer(ch: Channel[int], go: Channel[int], dead: Channel[int]):\n    defer cleanup(dead)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    dead := Channel[int]()\n    parallel:\n        spawn consumer(ch, go, dead)\n        spawn boom(go)\nmain()\n";
    let s = src.to_string();
    let (mn, mn_res) = run_with_deadline("M:N", move || run_program(&s));
    let s = src.to_string();
    let (serial, se_res) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out, res) in [("serial", &serial, &se_res), ("M:N", &mn, &mn_res)] {
        assert!(
            out.contains("CLEANUP-ENTER\n"),
            "{engine}: the cleanup RAN (it is the cancel's whole point): {out:?}"
        );
        assert!(
            !out.contains("CLEANUP-DONE\n"),
            "{engine}: it can never finish — its recv has no sender: {out:?}"
        );
        let err = res.as_ref().expect_err("the program must FAIL, not hang");
        assert!(
            err.message.contains("index 9 out of bounds"),
            "{engine}: the sibling's real fault is what is reported (the stuck cleanup's own error \
             is swallowed with its cancelled task): {err:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// The regression guard for the demote-path cancel fix: a `defer` whose body BLOCKS (here
/// `time.sleep_ms`) in a CANCELLED task runs to COMPLETION — cleanup is never truncated mid-body. The
/// M:N demote loops used to read the raw `self.cancel` flag instead of `cancel_requested()`, so the
/// blocking op inside the cleanup saw the already-tripped scope cancel and aborted the defer at that
/// call: `CLEANUP-ENTER` then nothing, sentinel 0 on M:N vs 42 on serial. Blocking cleanup is what
/// real cleanup DOES (close a socket, send a final message, flush).
#[test]
fn parity_a_blocking_defer_body_completes_when_the_task_is_cancelled() {
    let src = "import Shared from std.concurrency\nimport std.time\n\
               fn cleanup(s: Shared[int]):\n    print(\"CLEANUP-ENTER\")\n    time.sleep_ms(20)\n    s.set(42)\n    print(\"CLEANUP-DONE\")\n\
               fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(\"sentinel=\" + str(s.get()))\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (mn, _me, _mr, _mc) = run_file(&entry);
    let (serial, _se, _sr, _sc) = run_file(&entry);
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("CLEANUP-DONE\n") && out.contains("sentinel=42\n"),
            "{engine}: a blocking op inside a defer must NOT truncate the cleanup: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// The cleanup's OWN work must survive too: a `parallel:` (or `spawn`) opened INSIDE a cancelled task's
/// `defer` runs to completion on BOTH engines. The `deferring > 0` suppression that makes a defer
/// uncancellable is per-`Vm` and does NOT cross the airlock — a worker fiber is a fresh `Vm` with
/// `deferring == 0` — while the cancel-flag chain DOES cross it (`Vm::scope_ancestors` →
/// `JoinScope::ancestors` → `cancel_outer`). So the child of a cleanup's nursery inherited the
/// already-tripped enclosing flag and died at its first checkpoint (back-edge / blocking op): M:N
/// printed `CLEANUP-ENTER|CLEANUP-DONE|sentinel=0` (the child silently dropped, rc 0) against serial's
/// `sentinel=42` — serial severs the flag in a defer (`run_scheduler`'s `in_defer`). `scope_ancestors`
/// now severs identically, so cleanup that DELEGATES is as uncancellable as cleanup that blocks inline.
#[test]
fn parity_a_nursery_inside_a_cancelled_tasks_defer_runs_to_completion() {
    let src = "import Shared from std.concurrency\n\
               fn setit(s: Shared[int]):\n    i := 0\n    while i < 1000:\n        i = i + 1\n    s.set(42)\n    print(\"CHILD-DONE\")\n\
               fn cleanup(s: Shared[int]):\n    print(\"CLEANUP-ENTER\")\n    parallel:\n        spawn setit(s)\n    print(\"CLEANUP-DONE\")\n\
               fn worker(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn worker(ch, go, s)\n            spawn boom(go)\n        0\n    print(\"sentinel=\" + str(s.get()))\nmain()\n";
    let s = src.to_string();
    let (mn, _) = run_with_deadline("M:N", move || run_program(&s));
    let s = src.to_string();
    let (serial, _) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("CHILD-DONE\n") && out.contains("sentinel=42\n"),
            "{engine}: a task spawned by a cancelled task's cleanup must NOT inherit that cancel: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

/// N4 (demoted half), at the PROGRAM level: a DEMOTED fiber (a `recv` inside a native HOF callback —
/// a registered waiter, not `parked`) whose cancel flag is tripped is about to resume and unwind, which is
/// live progress `is_deadlocked`'s counters cannot see. Without the waiter veto
/// the quiesce between the faulting sibling's `finish` and that fiber's next `DEMOTE_POLL_BACKOFF` poll
/// reads as a deadlock, and `flag_deadlock` then drops EVERY parked fiber in the sched — including the
/// INNOCENT outer-scope sibling `b`, which is waiting for the value the cleanup is about to send. Fails
/// 7/8 runs with the veto removed (`B-GOT` replaced by a bogus nursery deadlock error).
#[test]
fn parity_a_cancel_wakeable_demoted_fiber_is_not_a_deadlock() {
    let src = "fn b(ch2: Channel[int]):\n    v := ch2.recv()\n    print(\"B-GOT=\" + str(v))\n\
               fn acleanup(ch2: Channel[int]):\n    ch2.send(7)\n    print(\"A-CLEANUP-DONE\")\n\
               fn a(ch: Channel[int], go: Channel[int], ch2: Channel[int]):\n    defer acleanup(ch2)\n    go.send(0)\n    xs := [1]\n    ys := xs.map(fn(x: int) -> int: ch.recv())\n    print(\"A-NEVER\")\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    zs := [1]\n    print(zs[9])\n\
               fn main():\n    ch := Channel[int]()\n    ch2 := Channel[int]()\n    go := Channel[int]()\n    parallel:\n        spawn b(ch2)\n        r := recover:\n            parallel:\n                spawn a(ch, go, ch2)\n                spawn boom(go)\n            0\nmain()\n";
    let s = src.to_string();
    let (mn, _) = run_with_deadline("M:N", move || run_program(&s));
    let s = src.to_string();
    let (serial, _) = run_with_deadline("serial", move || run_program(&s));
    for (engine, out) in [("serial", &serial), ("M:N", &mn)] {
        assert!(
            out.contains("A-CLEANUP-DONE\n") && out.contains("B-GOT=7\n"),
            "{engine}: an innocent parked sibling must not be faulted while a demoted fiber is \
             resuming on a tripped cancel: {out:?}"
        );
        assert!(
            !out.contains("deadlock"),
            "{engine}: no false deadlock: {out:?}"
        );
    }
    assert_same_lines(&serial, &mn);
}

// ===== R2 — std.io Writer / file-handle type (buffered + streaming file output) =====

/// R2 — `create(path)` opens a truncating write handle; `write` + `close` land the bytes; `read_file`
/// reads them back. Serial and M:N each.
#[test]
fn writer_create_roundtrip_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("out.txt");
        let src = format!(
            "import create, read_file from std.io\nfn main():\n    w := create(\"{}\")?\n    w.write(\"hi\")?\n    w.close()?\n    print(read_file(\"{}\")?)\nmain()\n",
            f.display(),
            f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "hi\n");
    }
}

/// R2 — `append` never truncates: create+write "a", close; append+write "b", close; file == "ab".
#[test]
fn writer_append_no_truncate_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("log.txt");
        let src = format!(
            "import create, append, read_file from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.write(\"a\")?\n    w.close()?\n    w2 := append(\"{f}\")?\n    w2.write(\"b\")?\n    w2.close()?\n    print(read_file(\"{f}\")?)\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "ab\n");
    }
}

/// R2 — a method on a CLOSED writer is a clean `Result::Err` (contains "closed writer"), NOT a panic.
#[test]
fn writer_use_after_close_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("x.txt");
        let src = format!(
            "import create from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.close()?\n    match w.write(\"z\"):\n        ?n: print(\"wrote \" + str(n))\n        !e: print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (should be a clean Err, not a fault): {r:?}"
        );
        assert!(
            out.contains("closed writer"),
            "want a clean closed-writer Err, got: {out:?}"
        );
    }
}

/// R2 — `write_bytes` round-trips arbitrary binary through `read_bytes`.
#[test]
fn writer_write_bytes_binary_roundtrip_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("bin");
        let src = format!(
            "import create, read_bytes from std.io\nfn main():\n    w := create(\"{f}\")?\n    w.write_bytes(bytes([0, 1, 255]))?\n    w.close()?\n    b := read_bytes(\"{f}\")?\n    print(str(b.len()) + \":\" + str(b[0]) + \",\" + str(b[1]) + \",\" + str(b[2]))\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "3:0,1,255\n");
    }
}

/// R2 — `buffered(stdout())`: N writes then one flush; the whole batch reaches stdout, and the output
/// is BYTE-IDENTICAL serial vs M:N (single task → exact-match). Routes through `emit_out` (the oracle).
#[test]
fn buffered_stdout_one_flush_byte_identical() {
    let t = TmpDir::new();
    let src = "import stdout, buffered from std.io\nfn main():\n    bw := buffered(stdout())\n    bw.write(\"a\")?\n    bw.write(\"b\")?\n    bw.write(\"c\")?\n    bw.flush()?\nmain()\n";
    let entry = t.write("main.chz", src);
    let (serial, _se, sr, _sc) = run_file(&entry);
    let (mn, _me, mr, _mc) = run_file(&entry);
    assert!(sr.is_ok() && mr.is_ok(), "faulted: s={sr:?} m={mr:?}");
    assert_eq!(serial, "abc");
    assert_eq!(serial, mn, "serial vs M:N byte-identical");
}

/// R2 — a buffered file writer dropped WITHOUT close still lands its tail (best-effort drop-flush at
/// program exit / heap teardown). Both engines.
#[test]
fn buffered_file_drop_flushes_best_effort_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("drop.txt");
        // Write via a buffered file writer, NEVER flush/close — the handle just goes out of scope.
        let src = format!(
            "import create, buffered from std.io\nfn go():\n    bw := buffered(create(\"{f}\")?)\n    bw.write(\"tail\")?\nfn main():\n    go()\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (_out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        let got = std::fs::read_to_string(&f).unwrap_or_default();
        assert_eq!(got, "tail", "drop-flush must land the buffered tail");
    }
}

/// R2 — `import Writer from std.io` (a pure TYPE with no runtime member value) and `import buffered
/// from std.io` in a RUNNING program: no runtime fault (the `bind_import` skip). Both engines.
#[test]
fn import_writer_type_and_buffered_runs_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let src = "import Writer, buffered, stdout from std.io\nfn tag(w: Writer) -> Writer:\n    return w\nfn main():\n    bw := tag(buffered(stdout()))\n    bw.write(\"ok\")?\n    bw.flush()?\nmain()\n";
        let entry = t.write("main.chz", src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (bind_import skip missing?): {r:?}");
        assert_eq!(out, "ok");
    }
}

/// R2 — a shared `Writer` written by two spawned tasks: order is UNSPECIFIED (Fork A), so compare the
/// line MULTISET (`assert_same_lines`), not exact-match. Both engines.
#[test]
fn shared_writer_across_tasks_same_lines() {
    let t = TmpDir::new();
    let f = t.0.join("shared.txt");
    let src = format!(
        "import create, read_file from std.io\nfn main():\n    w := create(\"{f}\")?\n    parallel:\n        spawn:\n            w.write(\"A\\n\")?\n        spawn:\n            w.write(\"B\\n\")?\n    w.close()?\n    print(read_file(\"{f}\")?)\nmain()\n",
        f = f.display()
    );
    let entry = t.write("main.chz", &src);
    let (serial, _se, sr, _sc) = run_file(&entry);
    let (mn, _me, mr, _mc) = run_file(&entry);
    assert!(sr.is_ok() && mr.is_ok(), "faulted: s={sr:?} m={mr:?}");
    assert_same_lines(&serial, &mn);
}

/// R2 — `create` into a nonexistent directory is a clean `Result::Err`, not a panic. Both engines.
#[test]
fn create_into_missing_dir_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("no_such_dir").join("x.txt");
        let src = format!(
            "import create from std.io\nfn main():\n    match create(\"{f}\"):\n        ?w: print(\"opened\")\n        !e: print(\"ERR\")\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (should be a clean Err): {r:?}");
        assert_eq!(out, "ERR\n");
    }
}

/// R2 — a full `Writer` program TYPE-CHECKS clean through `check_graph` (run_file skips the checker, so
/// this is the guard that the `Ty::Writer` method arm + the harvested method-table seed actually
/// resolve `w.write(...)`/`w.close(...)` at check time — the CLI path).
#[test]
fn writer_program_type_checks_clean() {
    // `main` must return Result to use `?` (no `fn main` exception — see the try-in-nil-fn soundness fix).
    let src = "import create, buffered, stdout, Writer from std.io\n\
               fn tag(w: Writer) -> Writer:\n    return w\n\
               fn main() -> int!:\n    w := create(\"/tmp/x\")?\n    w.write(\"a\")?\n    w.write_bytes(bytes([1]))?\n    w.flush()?\n    w.close()?\n    bw := tag(buffered(stdout(), 4096))\n    bw.write(\"b\")?\n    bw.flush()?\n    return ?0\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let r = crate::checker::check_graph(&graph);
    assert!(
        r.is_ok(),
        "a well-typed Writer program must check clean, got: {r:?}"
    );
}

// ===== R2b — std.io Reader / file-handle type (line/chunk streaming file INPUT) =====

/// R2b — `open(path)` opens a read-only handle; `read_line()` streams the file line-by-line
/// (trailing newline stripped, `None` = EOF); `close()` releases the fd. Serial and M:N each.
#[test]
fn reader_open_read_line_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            ?ln: print(ln)\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\ntwo\nthree\n");
    }
}

/// R2b — a file whose last line has NO trailing newline: `read_line` still yields it, then `None`.
#[test]
fn reader_read_line_no_trailing_newline_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "a\nb").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            ?ln: print(\"[\" + ln + \"]\")\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "[a]\n[b]\n");
    }
}

/// R2b — line-terminator stripping matches the module-level `io.read_line` UNCONDITIONALLY: a CRLF
/// (`\r\n`) line and a final bare-`\r` line (classic-Mac / no trailing `\n`) both come back with the
/// `\r` gone. Guards the anti-drift contract (a nested `\r`-only-if-`\n` strip retained the bare `\r`).
#[test]
fn reader_read_line_strips_bare_cr_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "a\r\nb\r").unwrap(); // CRLF line, then a bare-CR final line (no \n)
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    while true:\n        match r.read_line():\n            ?ln: print(\"[\" + ln + \"]\")\n            None: break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "[a]\n[b]\n");
    }
}

/// R2b — `r.lines()` is a BODIED Chezzi method on the `native struct Reader` (mixing Rust-backed
/// `native fn` sigs with a pure-Chezzi generator method on one native handle). It streams the file
/// line-by-line lazily (a generator over `read_line()`). Both engines.
#[test]
fn reader_lines_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    for ln in r.lines():\n        print(ln)\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\ntwo\nthree\n");
    }
}

/// R2b — `r.lines()` yields LAZILY: an early `break` after the first line must NOT drain the file
/// into a list. Proves the generator suspends between lines (does not snapshot the file). Both engines.
#[test]
fn reader_lines_lazy_early_break_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    for ln in r.lines():\n        print(ln)\n        break\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "one\n");
    }
}

/// R2b — `read_bytes(n)` chunks the file: exactly-n bytes until the short final chunk, then empty
/// bytes (`len == 0`) = EOF. Both engines.
#[test]
fn reader_read_bytes_chunk_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("bin");
        std::fs::write(&f, [10u8, 20, 30, 40, 50, 60]).unwrap(); // 6 bytes
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    c1 := r.read_bytes(4)?\n    c2 := r.read_bytes(4)?\n    c3 := r.read_bytes(4)?\n    print(str(c1.len()) + \":\" + str(c1[0]) + \",\" + str(c1[3]))\n    print(str(c2.len()) + \":\" + str(c2[0]) + \",\" + str(c2[1]))\n    print(str(c3.len()))\n    r.close()?\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted: {r:?}");
        assert_eq!(out, "4:10,40\n2:50,60\n0\n");
    }
}

/// R2b — `read_bytes` on a CLOSED reader is a clean `Result::Err` (contains "closed reader"), NOT a
/// panic. Both engines.
#[test]
fn reader_use_after_close_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("x.txt");
        std::fs::write(&f, "data").unwrap();
        let src = format!(
            "import open from std.io\nfn main():\n    r := open(\"{f}\")?\n    r.close()?\n    match r.read_bytes(4):\n        ?b: print(\"got \" + str(b.len()))\n        !e: print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (should be a clean Err, not a fault): {r:?}"
        );
        assert!(
            out.contains("closed reader"),
            "want a clean closed-reader Err, got: {out:?}"
        );
    }
}

/// R2b — `open` on a nonexistent file is a clean `Result::Err`, not a panic. Both engines.
#[test]
fn reader_open_missing_file_clean_err_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("no_such_dir").join("x.txt");
        let src = format!(
            "import open from std.io\nfn main():\n    match open(\"{f}\"):\n        ?r: print(\"opened\")\n        !e: print(\"ERR:\" + e.message())\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(r.is_ok(), "run faulted (should be a clean Err): {r:?}");
        assert!(
            out.starts_with("ERR:"),
            "want a clean open Err, got: {out:?}"
        );
    }
}

/// R2b — `import Reader from std.io` (a pure TYPE with no runtime member value) + send the handle
/// across a `spawn` boundary and read in the task: no runtime fault (the `bind_import` skip + airlock
/// sites). Both engines.
#[test]
fn import_reader_type_and_send_across_spawn_parity() {
    for run in [run_file as fn(&std::path::Path) -> RunOutput, run_file] {
        let t = TmpDir::new();
        let f = t.0.join("in.txt");
        std::fs::write(&f, "hello\n").unwrap();
        let src = format!(
            "import Reader, open from std.io\nfn first(r: Reader) -> str?:\n    return r.read_line()\nfn main():\n    r := open(\"{f}\")?\n    parallel:\n        spawn:\n            match first(r):\n                ?ln: print(ln)\n                None: print(\"empty\")\nmain()\n",
            f = f.display()
        );
        let entry = t.write("main.chz", &src);
        let (out, _e, r, _c) = run(&entry);
        assert!(
            r.is_ok(),
            "run faulted (bind_import/airlock missing?): {r:?}"
        );
        assert_eq!(out, "hello\n");
    }
}

/// R2b — a full `Reader` program TYPE-CHECKS clean through `check_graph` (the CLI path; `run_file`
/// skips the checker). Guards that `Ty::Reader`'s method arm + the harvested method-table seed resolve
/// `r.read_line()`/`r.read_bytes(..)`/`r.close()` at check time.
#[test]
fn reader_program_type_checks_clean() {
    // `main` must return Result to use `?` (no `fn main` exception — see the try-in-nil-fn soundness fix).
    let src = "import open, Reader from std.io\n\
               fn tag(r: Reader) -> Reader:\n    return r\n\
               fn main() -> int!:\n    r := tag(open(\"/tmp/x\")?)\n    match r.read_line():\n        ?ln: print(ln)\n        None: print(\"eof\")\n    b := r.read_bytes(8)?\n    print(str(b.len()))\n    r.close()?\n    return ?0\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let r = crate::checker::check_graph(&graph);
    assert!(
        r.is_ok(),
        "a well-typed Reader program must check clean, got: {r:?}"
    );
}

/// B3 — a fn-LOCAL aggregate mutated inside a spawn: task deep-copies per task (the airlock), so the
/// parent's list is untouched by the isolated task copy (prints `3`). Task 1
/// gave the MODULE-GLOBAL form the same isolation (module globals are snapshotted per task now —
/// see `serial_module_global_direct_mutation_forms_isolate_parity`), so this fn-local case is just the
/// sibling boundary: a captured local was already deep-copied and still is.
#[test]
fn module_global_aggregate_mutation_in_task_parity() {
    // TICKET-169 (D4 layer C): the task's copy of the captured `xs` is marked, so `xs.push(99)`
    // now faults instead of mutating it — the parent's list was always going to stay `3` long.
    let src = "fn main():\n    xs := [1, 2, 3]\n    parallel:\n        spawn:\n            recover: xs.push(99)\n    print(xs.len())\nmain()\n";
    assert_golden_out(src, "3\n");
}

/// Task 1 — a module-global mutated via a cross-module FN CALL from a task isolates on BOTH engines.
/// `counter.bump()` reassigns `count` inside the helper module; each spawned task snapshots the module
/// globals (serial now deep-copies at the spawn boundary exactly as M:N does), so the parent's post-join
/// read sees the PRE-task value. Before the fix serial printed 2 (shared) / M:N printed 0 (snapshot);
/// after it both print 0. This program COMPILES today (a cross-module fn-call reassign escaped every
/// old checker gate) and used to DIVERGE at runtime — the exact gaps.md §B3 (A) residual.
#[test]
fn serial_module_global_method_call_mutation_isolates_parity() {
    // TICKET-169 (D4 layer C): `count = count + 1` inside `bump()` now faults (the task's own
    // copy of the imported module's global), so the parent's read was always going to stay `0`.
    let out = golden_file_entry(
        &[
            (
                "counter.chz",
                "count := 0\nfn bump():\n    recover:\n        count = count + 1\nfn get() -> int:\n    return count\n",
            ),
            (
                "main.chz",
                "import counter\nfn main():\n    parallel:\n        spawn:\n            counter.bump()\n            counter.bump()\n    print(counter.get())\nmain()\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "0\n");
}

/// Task 1 — gaps.md §B3 (D): aliasing a module-global aggregate into a task-local (`local := xs`) then
/// mutating the alias. The receiver root resolves to `local`, so the old flow-blind gate never caught it
/// and the program silently diverged (serial 4 / M:N 3). With module globals snapshotted per
/// task, the alias points at the task's OWN copy — reads 3.
#[test]
fn serial_module_global_task_local_alias_isolates_parity() {
    // TICKET-169 (D4 layer C): `local.push(99)` now faults (`local` is the adopted copy of the
    // module global's), so the parent's read was always going to stay `3`.
    let src = "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            local := xs\n            recover: local.push(99)\n    print(xs.len())\nmain()\n";
    let out = golden_entry(src);
    assert_eq!(out, "3\n");
}

/// Task 1 — the ESCAPE HATCH must survive the serial snapshot change: `Atomic`/`Shared`/`Channel` cross
/// the spawn boundary by shared `Arc` core (via `to_snap`), NOT by deep copy, so a task-side mutation IS
/// visible to the parent. Two tasks each `add(1)` to a module-global `Atomic`; the parent reads 2 on both
/// engines. This is the regression guard for trap #1 (a hand-rolled copier would clone the Arc away → 0).
#[test]
fn atomic_incremented_in_task_visible_to_parent_parity() {
    let src = "import Atomic from std.concurrency\na := Atomic(0)\nfn main():\n    parallel:\n        spawn:\n            a.add(1)\n        spawn:\n            a.add(1)\n    print(a.load())\nmain()\n";
    let out = golden_entry(src);
    assert_eq!(out, "2\n");
}

/// Task 1 — direct in-block mutation forms (previously REJECTED by the frozen-module-global gates, now
/// AtomicInt — monomorphic lock-free int atomic. Basic load/store/exchange/cas roundtrip.
#[test]
fn atomic_int_roundtrip_parity() {
    let src = "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(5)\n    a.store(10)\n    print(a.load())\n    print(a.exchange(20))\n    print(a.cas(20, 99))\n    print(a.load())\nmain()\n";
    assert_eq!(golden_entry(src), "10\n10\ntrue\n99\n");
}

/// AtomicInt.add MUST keep the i64-overflow FAULT (checked CAS-loop, NOT raw fetch_add that wraps).
#[test]
fn atomic_int_add_overflow_parity() {
    let src = "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(9223372036854775807)\n    print(a.add(1))\nmain()\n";
    let msg = golden_entry_fault(src);
    assert!(msg.contains("integer overflow in Add"), "{msg}");
}

/// AtomicInt.sub MUST keep the i64-overflow FAULT symmetrically (i64::MIN - 1).
#[test]
fn atomic_int_sub_overflow_parity() {
    let src = "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(-9223372036854775808)\n    print(a.sub(1))\nmain()\n";
    let msg = golden_entry_fault(src);
    assert!(msg.contains("integer overflow in Sub"), "{msg}");
}

/// High-contention: 8 tasks × 10000 `add(1)` on one shared AtomicInt == 80000 exactly (lock-free
/// correctness — a broken non-atomic RMW would lose updates under M:N).
#[test]
fn atomic_int_contention_parity() {
    let src = "import AtomicInt from std.concurrency\na := AtomicInt(0)\nfn main():\n    parallel:\n        for _ in 0..8:\n            spawn:\n                for _ in 0..10000:\n                    a.add(1)\n    print(a.load())\nmain()\n";
    assert_eq!(golden_entry(src), "80000\n");
}

/// Reserved-name hole guard: `import AtomicInt from std.concurrency` must RUN (bind_import skip),
/// not check-pass-then-runtime-trap. Both engines.
#[test]
fn atomic_int_from_import_runs_parity() {
    let src = "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(0)\n    a.add(3)\n    print(a.load())\nmain()\n";
    assert_eq!(golden_entry(src), "3\n");
}

/// Wide-int returns: a value outside ±2^62 (a legal Chezzi int, boxed as Obj::BigInt) must round-trip
/// through load/exchange/add — the value-producing arms MUST box via make_int, not Value::int (inline-
/// only: debug-asserts + release-truncates for |n| >= 2^62). 2^62 = 4611686018427387904; INT_MAX_INLINE
/// = 2^62-1. Mirrors Mutex-backed Atomic, which is correct via from_wire→make_int.
#[test]
fn atomic_int_wide_value_returns_parity() {
    // load a wide stored value
    assert_eq!(
        golden_entry(
            "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387904)\n    print(a.load())\nmain()\n"
        ),
        "4611686018427387904\n",
    );
    // exchange returns the wide OLD value
    assert_eq!(
        golden_entry(
            "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387904)\n    print(a.exchange(0))\n    print(a.load())\nmain()\n"
        ),
        "4611686018427387904\n0\n",
    );
    // add carries the counter across the inline boundary (no i64 overflow → must return boxed)
    assert_eq!(
        golden_entry(
            "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(4611686018427387903)\n    print(a.add(1))\nmain()\n"
        ),
        "4611686018427387904\n",
    );
    // sub carries below the negative inline boundary
    assert_eq!(
        golden_entry(
            "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(-4611686018427387904)\n    print(a.sub(1))\nmain()\n"
        ),
        "-4611686018427387905\n",
    );
}

/// deleted): list `.push`, map index-assign, struct field-assign, set `.add`, bytearray `.extend`, and a
/// bare reassign. Each mutates the task's OWN module-global copy → invisible to the parent.
#[test]
fn serial_module_global_direct_mutation_forms_isolate_parity() {
    // TICKET-169 (D4 layer C): every write below now faults (a spawned task's own module-global
    // copy) instead of mutating in place — every expected value below was already the pre-task
    // one, so isolation holds for a stronger reason now.
    // list .push
    assert_eq!(
        golden_entry(
            "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            recover: xs.push(99)\n    print(xs.len())\nmain()\n"
        ),
        "3\n",
    );
    // map index-assign
    assert_eq!(
        golden_entry(
            "m := {1: 2}\nfn main():\n    parallel:\n        spawn:\n            recover:\n                m[1] = 9\n    print(m[1])\nmain()\n"
        ),
        "2\n",
    );
    // struct field-assign
    assert_eq!(
        golden_entry(
            "struct Box:\n    n: int\ns := Box(0)\nfn main():\n    parallel:\n        spawn:\n            recover:\n                s.n = 9\n    print(s.n)\nmain()\n"
        ),
        "0\n",
    );
    // set .add
    assert_eq!(
        golden_entry(
            "st := {1, 2}\nfn main():\n    parallel:\n        spawn:\n            recover: st.insert(9)\n    print(st.len())\nmain()\n"
        ),
        "2\n",
    );
    // bytearray .extend
    assert_eq!(
        golden_entry(
            "ba := bytearray()\nfn main():\n    parallel:\n        spawn:\n            recover: ba.extend([1, 2, 3])\n    print(ba.len())\nmain()\n"
        ),
        "0\n",
    );
    // bare reassign
    assert_eq!(
        golden_entry(
            "g := 0\nfn main():\n    parallel:\n        spawn:\n            recover:\n                g = g + 1\n    print(g)\nmain()\n"
        ),
        "0\n",
    );
}

/// Task 1 — a module global mutated through a directly-spawned free-fn callee (`spawn worker()` where
/// `worker` does `m[1] = 9`) — the old transitive-scan gate's job. Now it just isolates: the callee runs
/// against the task's module-global copy, so the parent's map is untouched.
#[test]
fn serial_module_global_spawned_callee_mutation_isolates_parity() {
    // TICKET-169 (D4 layer C): `m[1] = 9` now faults (the callee's own module-global copy).
    let src = "m := {1: 2}\nfn worker():\n    recover:\n        m[1] = 9\nfn main():\n    parallel:\n        spawn worker()\n    print(m[1])\nmain()\n";
    assert_eq!(golden_entry(src), "2\n");
}

/// Task 1 — a NESTED serial spawn (a task that itself opens a `parallel:` nursery and spawns) still
/// isolates the module global at every level: the inner task copies the (already-copied) outer view, so
/// its mutation is invisible even to the intermediate task. Guards the recursion in the snapshot path.
#[test]
fn nested_serial_spawn_module_global_isolates_parity() {
    // TICKET-169 (D4 layer C): both writes now fault (each is a spawned task's own module-global
    // copy: `outer` is itself spawned from `main`, and `inner` from `outer`).
    let src = "g := 0\nfn inner():\n    recover:\n        g = g + 100\nfn outer():\n    parallel:\n        spawn inner()\n    recover:\n        g = g + 1\nfn main():\n    parallel:\n        spawn outer()\n    print(g)\nmain()\n";
    assert_eq!(golden_entry(src), "0\n");
}

/// Task 1 — a task that mutates its module-global copy, PARKS on a channel recv, resumes, then reads the
/// global keeps its per-fiber snapshot across the park (the copy lives in `FiberCtx`, travels with the
/// fiber). The task sees its own mutation (1) and the parent sees the untouched global
/// (0). Guards trap #3 (snapshot survives a park).
#[test]
fn channel_park_keeps_module_snapshot_parity() {
    // TICKET-169 (D4 layer C): `g = g + 1` now faults (the task's own module-global copy), so the
    // task's snapshot stays `0` across the park — the parent's read was always going to be `0`.
    let src = "\
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency
g := 0
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            recover:
                g = g + 1
            v := ch.recv()
            print(\"task sees {g} got {v}\")
        spawn:
            ch.send(7)
    print(\"parent sees {g}\")
main()
";
    // task-order buffered output (decision F): the task's line flushes before the parent's read.
    assert_eq!(golden_entry(src), "task sees 0 got 7\nparent sees 0\n");
}

/// W6-2 (was: …`reads_frozen_parity`, expecting `0`) — a task that mutates a module global and THEN
/// opens a NESTED `parallel:` gives its grandchild the TASK's CURRENT view (1), at every depth.
/// The old `0` was a MEMOIZATION ARTIFACT, not design: `ensure_snapshot` memoized the first
/// nursery's snapshot forever and every later worker/nested nursery replayed that frozen `Arc` (decision
/// G1), so the grandchild read the pre-mutation value. A task now snapshots FRESH at its own `spawn`,
/// from the view of whoever spawned it — so the nested rule is uniform and Go-like. Per-task
/// ISOLATION is unchanged (`nested_serial_spawn_module_global_isolates_parity` still reads `0`).
#[test]
fn nested_serial_spawn_mutation_before_nested_reads_fresh_parity() {
    // TICKET-169 (D4 layer C): `g = g + 1` now faults (`worker` is a spawned task's own copy of
    // `g`), so the grandchild's fresh-snapshot read sees the task's UNCHANGED view (`0`) — the
    // property this test guards (a nested nursery sees its parent TASK's current view, not the
    // module's declaration-time one) no longer has a write to demonstrate it with.
    let src = "g := 0\nfn worker():\n    recover:\n        g = g + 1\n    parallel:\n        spawn:\n            print(g)\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
    assert_eq!(golden_entry(src), "0\n");
}

/// W6-2 (was: …`reads_frozen_parity`, expecting `0\n0\n`) — TWO sequential top-level nurseries with
/// ordinary (non-task) parent code mutating an imported module's global BETWEEN them: the second
/// nursery's task sees the MUTATED value (2 * 100). The old frozen `0` came from the never-invalidated
/// `snapshot_memo`; a module-slot write now drops the cache, so each nursery snapshots fresh.
#[test]
fn sequential_mutation_between_nurseries_reads_fresh_parity() {
    let out = golden_file_entry(
        &[
            (
                "counter.chz",
                "count := 0\nfn bump():\n    count = count + 1\nfn get() -> int:\n    return count\n",
            ),
            (
                "main.chz",
                "import counter\nfn main():\n    parallel:\n        spawn:\n            print(counter.get())\n    counter.bump()\n    counter.bump()\n    parallel:\n        spawn:\n            print(counter.get() * 100)\nmain()\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "0\n200\n");
}

/// W6-19 — a spawned task whose FIRST module-global access is a WRITE. `Op::GetGlobalSlot` calls
/// `ensure_module_faulted`, but `DefineGlobalSlot`/`SetGlobalSlot` did not, so on M:N the write indexed
/// an unfaulted (empty-slots) worker module and PANICKED the pool thread
/// (`index out of bounds: the len is 0`) → `internal error: a parallel task panicked`, while the
/// since-removed cooperative engine
/// printed the right answer. Rooted at `set_global_slot` (one guard, all callers).
#[test]
fn spawn_task_first_global_access_is_write_parity() {
    // TICKET-169 (D4 layer C): the task's first access being a WRITE now faults cleanly (D4)
    // instead of the historical W6-19 panic — the worker's own read still sees its unfaulted `1`.
    let src = "g: int = 1\nfn worker():\n    res := recover:\n        g = 99\n    match res:\n        ?_: print(\"worker g =\", g)\n        !_: print(\"worker g fault, g =\", g)\nfn main():\n    parallel:\n        spawn worker()\n    print(\"parent g =\", g)\nmain()\n";
    assert_eq!(golden_entry(src), "worker g fault, g = 1\nparent g = 1\n");
}

/// W6-2 — the PIN INSTANT, and the reason it exists: a task's view is pinned at its own `spawn`, so an
/// OUTER nursery's queued task keeps that view even though the two (then-existing) engines used to
/// PREPARE it at different program points — the since-removed serial engine at the outer nursery's own
/// join (post-`g = 2`), M:N at the nested nursery's join via `early_enlist_outer` (pre-`g = 2`). Both
/// `A` (outer, spawned before the mutation) and the nested `B` therefore read `1` while the parent
/// reads `2`, byte-identically. Without the pin this shape diverges 2 vs 1. Observed through `Shared`
/// and printed once after the join, because the print ORDER of this shape is nondeterministic
/// (a bare-`print` variant is not byte-identical).
#[test]
fn task_snapshot_pins_at_its_own_spawn_parity() {
    let src = "\
import Shared from std.concurrency
g: int = 1
fn main():
    a := Shared(0)
    b := Shared(0)
    parallel:
        spawn: a.set(g)
        parallel:
            spawn: b.set(g)
        g = 2
    print(\"A={a.get()} B={b.get()} parent={g}\")
main()
";
    assert_eq!(golden_entry(src), "A=1 B=1 parent=2\n");
}

/// W6-2 — the pin is per-TASK, at its own `spawn`: two spawns into the SAME nursery straddling a global
/// mutation read the value live when EACH ran, identically. That is the Go rule (a
/// goroutine sees the globals current when `go` ran) and it is what makes the IMPLICIT nursery — which
/// spans a whole module/function body — behave, since a per-NURSERY pin would freeze the whole body at
/// its first bare `spawn` (see `bare_spawn_implicit_nursery_pins_per_spawn_parity`).
#[test]
fn two_spawns_one_nursery_pin_at_their_own_spawn_parity() {
    let src = "\
import Shared from std.concurrency
g: int = 1
fn main():
    a := Shared(0)
    b := Shared(0)
    parallel:
        spawn: a.set(g)
        g = 2
        spawn: b.set(g)
    print(\"A={a.get()} B={b.get()} parent={g}\")
main()
";
    assert_eq!(golden_entry(src), "A=1 B=2 parent=2\n");
}

/// W6-2 (regression) — a bare `spawn` binds to the IMPLICIT nursery, which the compiler opens at the top
/// of the module / function body (`Span{1,1}`) and joins at its end. So the snapshot instant must be the
/// SPAWN, never the nursery: pinning per-nursery would freeze the whole body at its first bare `spawn`,
/// replaying every global initialized later as `nil` — the very W6-2 class. Here the second task reads
/// `n`, declared AFTER the first bare `spawn`, and a `List` global's method (the `nil` shape that faults
/// rather than printing wrong).
#[test]
fn bare_spawn_implicit_nursery_pins_per_spawn_parity() {
    let src = "\
import AtomicInt from std.concurrency
tot := AtomicInt(0)
spawn: tot.add(1)
n: int = 42
q: List[int] = [1, 2, 3]
spawn: print(\"n = {n} len = {q.len()} sum = {n + 1}\")
";
    assert_eq!(golden_entry(src), "n = 42 len = 3 sum = 43\n");
}

/// W6-2 — the same, one level down: a bare `spawn` inside a FUNCTION body sees the globals live at the
/// spawn, and a later mutation in that body is NOT time-travelled back into it (pre-W6-2 the snapshot was
/// built at the body-end join, so the task read the POST-mutation value).
#[test]
fn bare_spawn_in_a_function_body_pins_per_spawn_parity() {
    let src = "\
import Shared from std.concurrency
g: int = 1
fn main() -> Shared[int]:
    seen := Shared(0)
    spawn: seen.set(g)
    g = 2
    return seen
print(main().get())
";
    assert_eq!(golden_entry(src), "1\n");
}

// ----- 8-byte `Value` (int-favoring pointer-tag): boxing must be invisible to programs -----

#[test]
fn parity_big_int_crosses_inline_box_boundary() {
    // 2^62 and above box as `Obj::BigInt`; 2^62-1 and below stay inline. Crossing the boundary must
    // be invisible: arithmetic, equality, and Display all agree, matching byte-for-byte.
    let src = "\
fn main():
    x := 4611686018427387904
    print(x)
    print(x - 1)
    print(x == x)
    print(x + 1)
    y := 1 << 62
    print(y)
    print(x == y)
    print(x + 100)
main()";
    assert_golden_out(
        src,
        "4611686018427387904\n4611686018427387903\ntrue\n4611686018427387905\n4611686018427387904\ntrue\n4611686018427388004\n",
    );
}

#[test]
fn parity_i64_overflow_still_faults_past_boxing() {
    // Boxing lifts the INLINE ceiling to ±2^62, NOT the i64 ceiling: an operation whose true result
    // exceeds i64 still faults (overflow semantics unchanged), it does not silently box a wider value.
    let src = "\
fn run() -> int!:
    x := 4611686018427387904
    r := recover:
        _ := x * 2
    match r:
        ?v: return ?v
        !e: print(e.message())
    return ?0
fn main():
    _ := run()
main()";
    assert_eq!(golden_entry(src), "integer overflow in Mul\n");
}

#[test]
fn parity_boxed_float_canonical_eq() {
    // Two independently-boxed equal floats compare `==` and hash equal (a Set dedups them to one);
    // cross-type `1 == 1.0` still holds. Boxing is per-alloc, so equality MUST compare the f64.
    let src = "\
fn main():
    a := 1.5
    b := 3.0 / 2.0
    print(a == b)
    print(a)
    print(1 == 1.0)
    s := {1.5, 3.0 / 2.0}
    print(s.len())
main()";
    assert_golden_out(src, "true\n1.5\ntrue\n1\n");
}

#[test]
fn parity_airlock_bigint_and_float_roundtrip() {
    // A boxed big-int and a boxed float cross the airlock (spawn + Channel) and round-trip on BOTH
    // engines: `from_wire` re-boxes via `make_int`/`box_float` on the destination heap identically.
    let src = "\
fn main():
    ci := Channel[int]()
    cf := Channel[float]()
    parallel:
        spawn:
            ci.send(4611686018427387905)
            cf.send(1.5)
    x := ci.recv()
    f := cf.recv()
    print(x)
    print(x == 4611686018427387905)
    print(f)
    print(f == 1.5)
main()";
    assert_golden_out(src, "4611686018427387905\ntrue\n1.5\ntrue\n");
}

#[test]
fn parity_bin_local_const_wide_literal_on_boxed_local() {
    // `BinLocalConst{slot,val}` fuses `[GetLocal, ConstInt(val), binop]` with an UNBOUNDED `val`.
    // When the local is a boxed BigInt the fast inline path misses and the else-branch must
    // reconstruct the folded constant via `make_int` (boxes wide vals), NOT the inline-only
    // `Value::int` (debug-panics / release-corrupts for |val| > 2^62-1).
    let src = "\
fn f() -> int:
    x := 5000000000000000000
    return x - 4999999999999999999
fn main():
    print(f())
main()";
    assert_golden_out(src, "1\n");
}

#[test]
fn parity_inc_local_wide_delta_on_boxed_local() {
    // `IncLocal{slot,delta}` (the `+=` superinstruction) carries an UNBOUNDED fused `delta`. On a
    // boxed BigInt local the else-branch must push the delta via `make_int`, not the inline-only
    // `Value::int` (which debug-panics / release-corrupts for |delta| > 2^62-1).
    let src = "\
fn f() -> int:
    x := -5000000000000000000
    x += 4999999999999999999
    return x
fn main():
    print(f())
main()";
    assert_golden_out(src, "-1\n");
}

// ===== F3 — checker over-rejection fixes, end-to-end RUN goldens =====

/// F3 — generic fns over the native reserved handles (`Shared`/`Atomic`) bind `T` from the argument
/// and run type-erased identically.
#[test]
fn generic_fn_over_native_handles_run_parity() {
    let src = "import Atomic, Shared, StructuralEq from std.concurrency\n\
               fn peek[T](s: Shared[T]) -> T:\n\
               \x20   return s.get()\n\
               fn look[T: StructuralEq](a: Atomic[T]) -> T:\n\
               \x20   return a.load()\n\
               print(peek(Shared(9)))\n\
               print(look(Atomic(3)))\n";
    assert_golden_out(src, "9\n3\n");
}

/// F3 (subst) — a generic wrapper struct holding a `Channel[T]` constructs, and its channel is
/// sent-to and recv'd-from after `Channel[T]→Channel[int]` substitution.
#[test]
fn generic_wrapper_struct_channel_run_parity() {
    let src = "import std.concurrency\nimport Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\n\
               struct Box[T]:\n\
               \x20   ch: Channel[T]\n\
               b := Box(Channel[int]())\n\
               b.ch.send(7)\n\
               print(b.ch.recv())\n";
    assert_golden_out(src, "7\n");
}

/// FIX 1a — member-level turbofish on a RESERVED built-in receiver whose harvested method declares
/// its own `[U]` (`List.map`) is accepted, not rejected "takes no type argument(s)". Runs on both
/// engines and prints the mapped list.
#[test]
fn reserved_receiver_method_turbofish_run_parity() {
    let src = "print([1, 2, 3].map[int](fn(x): x * 2))\n";
    assert_golden_out(src, "[2, 4, 6]\n");
}

/// TICKET-006: every FFI example must resolve its C library name per-platform (via a
/// `std.ffi` helper), not hardcode the Linux soname `libc.so.6`/`libm.so.6`. Hardcoding the
/// Linux name is exactly what makes each example `dlopen`-fail on macOS. This does not run
/// on macOS; it pins the source-text fact that reproduces the reported failure everywhere.
#[test]
fn ffi_examples_do_not_hardcode_linux_soname() {
    let examples = [
        "examples/ffi.chz",
        "examples/ffi_ptr.chz",
        "examples/ffi_int.chz",
        "examples/ffi_qsort.chz",
        "examples/ffi_qualified.chz",
        "examples/ffi_str.chz",
        "examples/ffi_struct.chz",
    ];
    let mut offenders = Vec::new();
    for rel in examples {
        let src = std::fs::read_to_string(fixture(rel)).unwrap();
        if src.contains("libc.so.6") || src.contains("libm.so.6") {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "FFI examples still hardcode a Linux soname instead of resolving one per platform: {offenders:?}"
    );
}

/// TICKET-006: the six FFI goldens must not be unconditionally `#[cfg(target_os = "linux")]`.
/// Gating them out on every other platform means `cargo test` reports green there while the
/// entire FFI surface goes unexercised — the gap that let the macOS `dlopen` failure through.
#[test]
fn ffi_goldens_are_not_unconditionally_linux_gated() {
    // Every file of the golden suite: the FFI goldens may live in any of them.
    let mut this_file = String::new();
    for entry in std::fs::read_dir(fixture("src/vm/golden_tests")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "rs") {
            this_file.push_str(&std::fs::read_to_string(&path).unwrap());
            this_file.push('\n');
        }
    }
    let ffi_golden_fns = [
        "golden_ffi_chz_via_run_file",
        "golden_ffi_ptr_chz_via_run_file",
        "golden_ffi_struct_chz_via_run_file",
        "golden_ffi_qsort_chz_via_run_file",
        "golden_ffi_str_chz_via_run_file",
        "golden_ffi_int_chz_via_run_file",
    ];
    let mut unconditionally_gated = Vec::new();
    for fn_name in ffi_golden_fns {
        let marker = format!("fn {fn_name}(");
        let fn_pos = this_file
            .find(&marker)
            .unwrap_or_else(|| panic!("missing test fn {fn_name}"));
        let before = &this_file[..fn_pos];
        let last_lines: Vec<&str> = before.lines().rev().take(3).collect();
        if last_lines
            .iter()
            .any(|l| l.trim().starts_with("#[cfg(") && l.contains("target_os = \"linux\""))
        {
            unconditionally_gated.push(fn_name);
        }
    }
    // The suite is split into topic files: a gate on a whole file switches off every golden in
    // it. No file opens with an inner `#![cfg(...)]`, and no `mod` line in `golden_tests/mod.rs`
    // or `vm/mod.rs`'s `mod golden_tests;` carries a `target_os` gate.
    for entry in std::fs::read_dir(fixture("src/vm/golden_tests")).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        if text.lines().any(|l| l.trim_start().starts_with("#![cfg")) {
            unconditionally_gated.push("a whole golden_tests file (inner #![cfg])");
        }
    }
    for (file, text) in [
        (
            "golden_tests/mod.rs",
            std::fs::read_to_string(fixture("src/vm/golden_tests/mod.rs")).unwrap(),
        ),
        (
            "vm/mod.rs",
            std::fs::read_to_string(fixture("src/vm/mod.rs")).unwrap(),
        ),
    ] {
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let is_suite_mod =
                line.starts_with("mod ") && (file != "vm/mod.rs" || *line == "mod golden_tests;");
            let gated = is_suite_mod
                && lines[..i]
                    .iter()
                    .rev()
                    .take_while(|l| l.starts_with("#["))
                    .any(|l| l.contains("target_os"));
            if gated {
                unconditionally_gated.push("a golden_tests module declaration (target_os cfg)");
            }
        }
    }
    assert!(
        unconditionally_gated.is_empty(),
        "FFI goldens are unconditionally Linux-gated, so cargo test is green on other \
         platforms with the FFI surface unexercised: {unconditionally_gated:?}"
    );
}

/// TICKET-183: the verdict of one program through the real pipeline (`build_graph` resolves a
/// native std import; the single-module `check` does not): `type error: <every message, joined by
/// " | ">`, else `fault: <message>`, else the trimmed stdout.
fn module_scope_verdict(src: &str) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
        return format!("type error: {}", msgs.join(" | "));
    }
    let (out, _err, result, _code) = run_file(&entry);
    match result {
        Err(e) => format!("fault: {}", e.message),
        Ok(()) => out.trim().to_string(),
    }
}

/// TICKET-183 grid: module scope is order-free for function bodies, top-level statements stay
/// lexical, and a body that reads a let-only global before its let runs faults. Axes: the op (read,
/// call, field, match, write) x X's kind (`:=`, typed, `const`, fn, enum type, import alias) x X
/// above or below the fn x the printing statement above or below X's statement. Every value cell
/// prints `3` (`7` for a write). CPython prints the same value in every value cell (a write through
/// `global X`); a read-before-init cell is CPython's `NameError`.
#[test]
fn module_scope_order_grid() {
    const FAULT: &str = "fault: 'X' is read before its initialization";
    struct Kind {
        label: &'static str,
        prelude: &'static str,
        decl: &'static str,
        fns: &'static str,
        print: &'static str,
    }
    let read_fn = "fn f(): X\n";
    let write_fns = "fn w():\n    X = 7\nfn r() -> int:\n    return X\n";
    let write_print = "w()\nprint(r())\n";
    let match_e = "fn f() -> int:\n    match X:\n        E.A:\n            return 1\n        E.B:\n            return 3\n";
    let kinds = [
        Kind {
            label: "read :=",
            prelude: "",
            decl: "X := 3\n",
            fns: read_fn,
            print: "print(f())\n",
        },
        Kind {
            label: "read typed",
            prelude: "",
            decl: "X: int = 3\n",
            fns: read_fn,
            print: "print(f())\n",
        },
        Kind {
            label: "read const",
            prelude: "",
            decl: "X: const int = 3\n",
            fns: read_fn,
            print: "print(f())\n",
        },
        Kind {
            label: "call :=",
            prelude: "",
            decl: "X := fn(n: int) -> int: n + 2\n",
            fns: "fn f(): X(1)\n",
            print: "print(f())\n",
        },
        Kind {
            label: "field :=",
            prelude: "struct P:\n    v: int\n",
            decl: "X := P(3)\n",
            fns: "fn f(): X.v\n",
            print: "print(f())\n",
        },
        Kind {
            label: "field typed",
            prelude: "struct P:\n    v: int\n",
            decl: "X: P = P(3)\n",
            fns: "fn f(): X.v\n",
            print: "print(f())\n",
        },
        Kind {
            label: "match :=",
            prelude: "enum E:\n    A\n    B\n",
            decl: "X := E.B\n",
            fns: match_e,
            print: "print(f())\n",
        },
        Kind {
            label: "match typed",
            prelude: "enum E:\n    A\n    B\n",
            decl: "X: E = E.B\n",
            fns: match_e,
            print: "print(f())\n",
        },
        Kind {
            label: "call fn",
            prelude: "",
            decl: "fn X(n: int) -> int:\n    return n + 2\n",
            fns: "fn f(): X(1)\n",
            print: "print(f())\n",
        },
        Kind {
            label: "match type",
            prelude: "",
            decl: "enum X:\n    A\n    B\n",
            fns: "fn f() -> int:\n    match X.B:\n        X.A:\n            return 1\n        X.B:\n            return 3\n",
            print: "print(f())\n",
        },
        Kind {
            label: "field import",
            prelude: "",
            decl: "import std.math as X\n",
            fns: "fn f() -> int:\n    return int(X.floor(3.9))\n",
            print: "print(f())\n",
        },
        Kind {
            label: "write :=",
            prelude: "",
            decl: "X := 3\n",
            fns: write_fns,
            print: write_print,
        },
        Kind {
            label: "write typed",
            prelude: "",
            decl: "X: int = 3\n",
            fns: write_fns,
            print: write_print,
        },
        Kind {
            label: "write const",
            prelude: "",
            decl: "X: const int = 3\n",
            fns: write_fns,
            print: write_print,
        },
    ];
    let mut got = Vec::new();
    let mut bad = Vec::new();
    for k in &kinds {
        // (X above the fn, print after X's statement): the four layouts of {print, X, fns}.
        for (above, after) in [(true, false), (true, true), (false, false), (false, true)] {
            let (d, f, p) = (k.decl, k.fns, k.print);
            let body = match (above, after) {
                (true, false) => format!("{p}{d}{f}"),
                (true, true) => format!("{d}{f}{p}"),
                (false, false) => format!("{f}{p}{d}"),
                (false, true) => format!("{f}{d}{p}"),
            };
            let src = format!("{}{body}", k.prelude);
            let hoisted = matches!(k.label, "call fn" | "match type");
            let want = if k.label == "field import" {
                if above {
                    "3"
                } else {
                    "type error: 'X' is used before its `import`"
                }
            } else if k.label == "write const" {
                "type error: cannot reassign const binding 'X'"
            } else if k.label.starts_with("write") {
                "7"
            } else if hoisted || after {
                "3"
            } else {
                FAULT
            };
            let v = module_scope_verdict(&src);
            let cell = format!(
                "{:<13} X {:<5} print {:<6} -> {v}",
                k.label,
                if above { "above" } else { "below" },
                if after { "after" } else { "before" }
            );
            if !v.starts_with(want) {
                bad.push(format!("{cell}\n    want: {want}\n{src}"));
            }
            got.push(cell);
        }
    }
    // Extra rows: (label, program, expected verdict prefix).
    let extras = [
        (
            "swap a,b",
            "a := 1\nb := 2\nfn f(): a * 10 + b\nprint(f())\n",
            "12",
        ),
        (
            "swap b,a",
            "b := 2\na := 1\nfn f(): a * 10 + b\nprint(f())\n",
            "12",
        ),
        (
            "swap fn above",
            "fn f(): a * 10 + b\nb := 2\na := 1\nprint(f())\n",
            "12",
        ),
        (
            "type hole :=",
            "x := \"s\"\nfn f(): x\ny: int = f()\nprint(y)\n",
            "type error: cannot assign str",
        ),
        (
            "type hole typed",
            "x: int = 5\nfn f(): x\ny: str = f()\nprint(y)\n",
            "type error: cannot assign int",
        ),
        (
            "cycle",
            "x := f()\nfn f(): x\nprint(x)\n",
            "type error: initialization cycle: the type of 'x' comes from 'f()'",
        ),
        (
            "annotated cycle",
            "x: int = f()\nfn f(): x\nprint(x)\n",
            "fault: 'x' is read before its initialization at line 1",
        ),
        (
            "spawn",
            "fn g():\n    print(X)\nparallel:\n    spawn g()\nX := 3\n",
            FAULT,
        ),
        (
            "keyword call",
            "fn g() -> int:\n    return h(b=1, a=2)\nh := fn(a: int, b: int) -> int: a - b\nprint(g())\n",
            "1",
        ),
        (
            "decline",
            "fn f() -> List[str]:\n    return xs\nxs := []\nxs.push(1)\n",
            "type error: 'xs' is declared below",
        ),
        (
            "top-level read above",
            "print(X)\nX := 3\n",
            "type error: unknown name 'X'",
        ),
    ];
    for (label, src, want) in extras {
        let v = module_scope_verdict(src);
        let cell = format!("{label:<34} -> {v}");
        if !v.starts_with(want) {
            bad.push(format!("{cell}\n    want: {want}\n{src}"));
        }
        got.push(cell);
    }
    assert!(
        bad.is_empty(),
        "{} of {} cells differ:\n{}\n\nfull grid:\n{}",
        bad.len(),
        got.len(),
        bad.join("\n"),
        got.join("\n")
    );
}

/// TICKET-186 grid: every read path of a module slot faults on the TICKET-183 uninit marker.
/// `run_file` skips the checker, so a module-level `return` (a check error since TICKET-186)
/// still leaves a let slot uninitialized at run time. A cell passes only on `Err` with the quoted
/// message; `Ok`, a VM panic or another message fails it. Cell c21 is the checker verdict for a
/// later let of a name `import f from lib` binds: a second binding of the name.
#[test]
fn t186_uninit_read_paths_fault() {
    let ret = "return\nx := 5\n";
    let at2 = "'x' is read before its initialization at line 2";
    let at4 = "'x' is read before its initialization at line 4";
    let cells: &[(&str, &str, &str, &str)] = &[
        ("R1 m.x + 1", ret, "import m\nprint(m.x + 1)\n", at2),
        ("R2 print(m.x)", ret, "import m\nprint(m.x)\n", at2),
        (
            "R3 m.f()",
            "return\nf := fn() -> int: 1\n",
            "import m\nprint(m.f())\n",
            "'f' is read before its initialization at line 2",
        ),
        ("R4 from-import", ret, "import x from m\nprint(x)\n", at2),
        (
            "R6 body read",
            "fn g() -> int:\n    return x\nreturn\nx := 5\n",
            "import m\nprint(m.g())\n",
            at4,
        ),
        (
            "R7 spawn snapshot",
            "fn g():\n    print(x)\nfn h():\n    parallel:\n        spawn g()\nreturn\nx := 5\n",
            "import m\nm.h()\n",
            "'x' is read before its initialization",
        ),
        (
            "R8 generator frame",
            "fn gen() -> Iterator[int]:\n    yield x\nreturn\nx := 5\n",
            "import m\nfor v in m.gen():\n    print(v)\n",
            at4,
        ),
    ];
    let verdict = |r: std::thread::Result<RunOutput>, want: &str| -> Option<String> {
        match r {
            Err(_) => Some("VM panic".to_string()),
            Ok((out, _, Ok(()), _)) => Some(format!("Ok, stdout {out:?}")),
            Ok((_, _, Err(e), _)) if e.message.contains(want) => None,
            Ok((_, _, Err(e), _)) => Some(format!("fault {:?}", e.message)),
        }
    };
    let mut bad = Vec::new();
    for (label, m_src, main_src, want) in cells {
        let t = TmpDir::new();
        t.write("m.chz", m_src);
        let entry = t.write("main.chz", main_src);
        let r = std::panic::catch_unwind(|| run_file(&entry));
        if let Some(got) = verdict(r, want) {
            bad.push(format!("{label}: want {want:?}, got {got}"));
        }
    }
    // R5: the manifest entrypoint reads its fn from the module slot.
    {
        let want = "'main' is read before its initialization at line 2";
        let t = TmpDir::new();
        let entry = t.write("main.chz", "return\nmain := fn(): print(1)\n");
        let r = std::panic::catch_unwind(|| run_file_entry(&entry, "main"));
        if let Some(got) = verdict(r, want) {
            bad.push(format!("R5 entrypoint: want {want:?}, got {got}"));
        }
    }
    // c21: a let below the body of a name an import binds.
    {
        let want = "'f' is already imported";
        let t = TmpDir::new();
        t.write(
            "lib.chz",
            "fn f(a: int, b: int) -> int:\n    return a * 10 + b\n",
        );
        let entry = t.write(
            "main.chz",
            "import f from lib\nfn call() -> int:\n    return f(a=1, b=2)\nf := fn(b: int, a: int) -> int: a * 10 + b\nprint(call())\n",
        );
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        let errs = crate::checker::check_graph(&graph)
            .err()
            .unwrap_or_default();
        if !errs.iter().any(|e| e.message.contains(want)) {
            bad.push(format!("c21: want {want:?}, got {errs:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cells differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
