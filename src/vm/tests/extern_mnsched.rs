//! VM tests: qualified extern widths and structs, mnsched park and cancel, W7-58 owner verdict.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// Regression (blocker, the adversarial-panel find): a module-QUALIFIED width alias must resolve
/// to its DEFINING module's body even when the CALLING module declares a colliding bare alias of
/// the SAME name but a DIFFERENT width. `w3.Len` (= int64 in core/w3) must marshal as int64 — NOT
/// collapse to the calling module's local `type Len = int8`. With the bug, `abs(-300)` rounds
/// through int8 to 44; correctly resolved (int64) it stays 300. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_param_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qcollide_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype Len = int64\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\nimport int8 from std.ffi\n\ntype Len = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on colliding qualified width-alias extern: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "qualified `w3.Len` must marshal as the DEFINING module's int64 (300), not the local int8 (44)"
    );
}

/// Regression (ROOT, the deeper adversarial find): a module-QUALIFIED width alias whose body is
/// itself ANOTHER alias (a CHAIN, `type Len = Inner; type Inner = int64`) must resolve EVERY hop
/// in its DEFINING module's scope — NOT re-enter the flat last-write-wins bare `aliases` map on
/// the inner hop. The calling module declares a colliding bare `type Inner = int8`; with the bug
/// the inner hop collapses to int8 and `abs(-300)` rounds to 44. Correctly resolved (int64) it
/// stays 300 against a literal golden. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_chain_depth2_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qchain2_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype Inner = int64\ntype Len = Inner\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\nimport int8 from std.ffi\n\ntype Inner = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on chained qualified width-alias extern: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "chained `w3.Len -> Inner -> int64` must marshal as int64 (300), not the inner hop's colliding local int8 (44)"
    );
}

/// Regression (ROOT, depth-3): proves arbitrary chain depth, not just one inner hop. `w3.Len ->
/// A -> B -> int64`, with the calling module colliding BOTH inner names (`type A = int8`,
/// `type B = int8`). Every hop must resolve in w3's scope; correctly resolved it stays 300, the
/// bug rounds to 44. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_chain_depth3_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qchain3_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype B = int64\ntype A = B\ntype Len = A\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\nimport int8 from std.ffi\n\ntype A = int8\ntype B = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on depth-3 chained qualified width-alias extern: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "depth-3 `w3.Len -> A -> B -> int64` must marshal as int64 (300), not the colliding local int8 (44)"
    );
}

/// A CYCLIC qualified alias chain (`type A = B; type B = A`, no scalar leaf) used at an extern
/// boundary must surface a clean "not C-marshallable" error and — critically — must NOT hang or
/// overflow the stack while following the chain. The recursive module-scoped resolver is bounded
/// by a visited set: a repeated hop yields `None`, which propagates to the marshal backstop's
/// clean error. No libc needed (the checker/marshal gate rejects before any dlopen). The test
/// merely COMPLETING proves no infinite loop.
#[test]
fn extern_qualified_alias_cycle_is_clean_error() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qcycle_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("w3.chz"), "type A = B\ntype B = A\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\n\nextern \"libc.so.6\":\n    fn abs(n: w3.A) -> w3.A\n\nprint(0)\n",
    )
    .unwrap();
    let (_vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_err(),
        "expected a clean error for a cyclic qualified alias extern type, got Ok"
    );
    let msg = format!("{:?}", vm_res.unwrap_err());
    assert!(
        msg.contains("not C-marshallable") || msg.contains("C-marshallable"),
        "error should mention marshallability, got: {msg}"
    );
}

/// A genuinely non-marshallable QUALIFIED type at an extern boundary must surface a clean
/// compile/runtime error (the checker is the real gate), and — even if a path slipped past the
/// checker — the marshal loop's backstop must NEVER panic the VM. No libc needed (the checker
/// rejects before any dlopen). Asserts `is_err`, the "not C-marshallable" wording, and that the
/// process did not abort (the test simply completing proves no panic).
#[test]
fn extern_non_marshallable_qualified_is_clean_error() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qbad_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("bag.chz"), "struct Bag:\n    items: List[int]\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.bag\n\nextern \"libc.so.6\":\n    fn use_it(b: bag.Bag) -> int\n\nprint(0)\n",
    )
    .unwrap();
    let (_vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_err(),
        "expected a clean error for a non-marshallable qualified extern type, got Ok"
    );
    let msg = format!("{:?}", vm_res.unwrap_err());
    assert!(
        msg.contains("not C-marshallable") || msg.contains("C-marshallable"),
        "error should mention marshallability, got: {msg}"
    );
}

/// Regression (ROOT, the named-import chain hop — fix4): a module-QUALIFIED width alias whose
/// resolution hops through a NAMED-IMPORTED alias (`import W from core.widths` in the defining
/// module) must resolve EVERY hop in its own module's import/alias scope — NOT fall back to the
/// flat bare `aliases` map on the imported hop. `main` declares a colliding `type W = int8`; the
/// true chain is `w3.Len -> W(from widths) -> int64`. With the bug the imported hop collapses to
/// main's int8 and `abs(-300)` rounds to 44; correctly resolved (int64) it stays 300.
/// Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_named_import_hop_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qnamedhop_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("widths.chz"),
        "import int64 from std.ffi\n\ntype W = int64\n",
    )
    .unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import W from core.widths\n\ntype Len = W\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\nimport int8 from std.ffi\n\ntype W = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on named-import-hop qualified width-alias extern: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "named-import-hop `w3.Len -> W(from widths) -> int64` must marshal as int64 (300), not main's colliding int8 (44)"
    );
}

/// Regression (ROOT, the struct-field scope — fix5): a qualified extern RETURN STRUCT whose FIELDS
/// are typed via the DEFINING module's LOCAL alias (`type Half = int32`; `struct DivT{quot:Half;
/// rem:Half}`). fix4 resolved the struct's fields in the IMPORTER's scope (where `Half` is
/// invisible) → field None → struct CType None → void return → `cannot read field 'quot' of nil`.
/// The single-resolver fix computes the struct's CType in ITS defining module's scope, so the
/// return marshals as a real two-int32 struct and `div(17,5)` reads quot 3 / rem 2.
/// Linux-only (needs libc.so.6 `div`).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_return_struct_aliased_field_runs() {
    let dir =
        std::env::temp_dir().join(format!("chezzi_vm_ffi_structalias_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
            core.join("cdefs.chz"),
            "import int32 from std.ffi\n\ntype Half = int32\n\nstruct DivT:\n    quot: Half\n    rem: Half\n",
        )
        .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
             fn div(numer: int32, denom: int32) -> cdefs.DivT\n\nr := div(17, 5)\nprint(r.quot)\nprint(r.rem)\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on qualified return struct with aliased fields: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "3\n2\n",
        "qualified return struct whose fields use the defining module's local alias must marshal as two int32 (quot 3, rem 2)"
    );
}

/// Regression (ROOT, the MIXED chain — fix4): a single qualified alias whose resolution hops
/// through a LOCAL alias, then a NAMED-IMPORTED alias, then a QUALIFIED alias — with a colliding
/// `type W = int8` shadow at each module along the way — must resolve to the true width (int64)
/// at every hop in that hop's own module scope. The chain: main's extern names `mid.Outer`;
/// `mid` declares `type Outer = ImpW` (local hop) where `ImpW` is `import ImpW from core.widths`
/// (named-import hop) and `core.widths` declares `type ImpW = base.Base` (qualified hop into
/// `core.base` `type Base = int64`). Every module ALSO declares a colliding `type W = int8` and
/// some declare colliding `Outer`/`ImpW`/`Base` bare. Correctly resolved it stays 300; any single
/// mis-resolved hop rounds to 44. Must stay 300. Linux-only (libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_mixed_chain_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qmixed_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    // core/base.chz: the true leaf width (`Base = int64`). It ALSO declares `Outer = int8` and
    // `ImpW = int8` — the names the OTHER modules' hops use — so a stray flat-map fallback there
    // would round to 44.
    std::fs::write(
            core.join("base.chz"),
            "import int64 from std.ffi\nimport int8 from std.ffi\n\ntype Base = int64\ntype Outer = int8\ntype ImpW = int8\n",
        )
        .unwrap();
    // core/widths.chz: a QUALIFIED hop into base (`ImpW = base.Base`). It collides `Base` (its
    // own qualified body-name spelled bare here would be int8) and `Outer`.
    std::fs::write(
            core.join("widths.chz"),
            "import core.base\nimport int8 from std.ffi\n\ntype ImpW = base.Base\ntype Base = int8\ntype Outer = int8\n",
        )
        .unwrap();
    // mid.chz: a NAMED-IMPORT hop (`ImpW from core.widths`), then a LOCAL hop (`Outer = ImpW`).
    // It does NOT locally redefine `ImpW` (that would legitimately shadow the import); the
    // collisions live in the OTHER modules. It collides `Base`.
    std::fs::write(
            dir.join("mid.chz"),
            "import ImpW from core.widths\nimport int8 from std.ffi\n\ntype Outer = ImpW\ntype Base = int8\n",
        )
        .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import mid\nimport int8 from std.ffi\n\ntype Outer = int8\ntype ImpW = int8\ntype Base = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: mid.Outer) -> mid.Outer\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on mixed-chain qualified width-alias extern: {vm_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "mixed chain (local -> named-import -> qualified) must marshal as int64 (300), not any colliding int8 hop (44)"
    );
}

/// D3/the discriminating fairness test. 64 CPU "hog" fibers (≫ the core-sized worker pool) each
/// busy-wait on a `Shared[int]` until it reaches 50, spawned FIRST; then 50 "short" fibers that
/// each `update(+1)` and exit. WITHOUT preemption every worker grabs a hog (FIFO seed order), all
/// spin forever on a counter the never-scheduled shorts can't advance → permanent hang. WITH
/// reduction-counting preemption the hogs yield, the shorts run, the counter reaches 50, the hogs
/// observe it and exit. A watchdog turns the no-preemption hang into a test FAILURE (not an
/// infinite hang) and stands as the regression guard if preemption ever regresses.
#[test]
fn d3_preemption_prevents_cpu_hog_starvation() {
    let src = "import Shared from std.concurrency\n\
fn hog(s: Shared[int], k: int):
    while s.get() < k:
        continue

fn short(s: Shared[int]):
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..64:
            spawn hog(s, 50)
        for _ in 0..50:
            spawn short(s)
    print(s.get())

main()
";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("hog/short nursery completed"), "50\n"),
        Err(_) => {
            panic!("starved — D3 preemption regressed (CPU hogs never yielded their workers)")
        }
    }
}

// `d3_thousands_of_cpu_fibers_all_complete` moved to tests/d3_thousands_of_cpu_fibers_cli.rs (TICKET-114):
// at the lib target's default pool the debug build hung to its 60 s bound under load (W12-23).

/// D3/regression: a reduction yield must unwind cleanly through **nested function calls**. A yield
/// is detected at the safepoint of the innermost `run_until`; every enclosing `run_proto`/call site
/// must propagate it up WITHOUT popping a result (the frames replay on resume) — the same contract
/// as a `recv`-park. The first cut only guarded `suspend`, so a yield deep in a call chain
/// (main → work → middle → inner, the shape of `primes_parallel`) let `run_proto` pop a live stack
/// temp as a bogus return value → "expected bool, found int". This computes a known sum across two
/// workers, each looping 50 k times through a 3-deep call chain (millions of ops ≫ CONTEXT_REDS, so
/// many yields fire mid-chain), and also crosses a channel `recv` — exercising yield + park together.
#[test]
fn d3_yield_unwinds_through_nested_calls() {
    let src = "\
fn inner(n: int) -> int:
    return n * 2

fn middle(n: int) -> int:
    return inner(n) + 1

fn work(lo: int, hi: int, out: Channel[int]):
    acc := 0
    i := lo
    while i < hi:
        acc += middle(i)
        i += 1
    out.send(acc)

fn main():
    out := Channel[int]()
    parallel:
        spawn work(0, 50000, out)
        spawn work(50000, 100000, out)
    total := 0
    for _ in 0..2:
        got := out.recv() else: panic(\"out closed\")
        total += got
    print(total)

main()
";
    // sum_{i=0}^{99999} (2*i + 1) = 100000 * 100000.
    assert_eq!(run_capture(src).unwrap(), "10000000000\n");
}

/// D2b park-gap guard (the lost-wakeup fix): if a message is already queued when `park` runs (a
/// `send` landed in the gap between `recv`'s empty-check and the park), the fiber must NOT park —
/// it is requeued `Ready` so it re-runs `recv` and pops the message. Without this the fiber would
/// park forever behind a delivered-but-unconsumed message → a false deadlock.
#[test]
fn mnsched_park_requeues_when_message_already_waiting() {
    let sched = mk_sched(1);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    // Simulate a send that landed in the gap (message queued, but this fiber wasn't parked yet).
    core.q.lock().unwrap().push(Value::int(7));
    sched.park(key, Arc::clone(&core), f);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "must not park behind a waiting message");
    assert_eq!(c.global.len(), 1, "fiber requeued to re-run recv");
}

/// D2b park-gap guard (cancel half): if cancel was tripped in the gap before the park, the fiber
/// must be requeued (to unwind on the back-edge) rather than parked (where it would be stranded).
#[test]
fn mnsched_park_requeues_when_cancel_tripped() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    cancel.store(true, Ordering::Relaxed);
    sched.park(core_key(&core), Arc::clone(&core), f);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "must not park a cancelled fiber");
    assert_eq!(c.global.len(), 1);
}

/// TICKET-118 (W13-8) — the park-gap guard must also requeue on an ANCESTOR's cancel, not just this
/// scope's own flag: an `Executor`'s `cancel` trips a flag a job's nursery scope carries in
/// `JoinScope::ancestors`, not `scopes[sid].cancel` itself.
#[test]
fn mnsched_park_requeues_when_an_ancestor_cancel_is_tripped() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    sched.lock().scopes[0]
        .ancestors
        .push(Arc::new(AtomicBool::new(true)));
    sched.park(core_key(&core), Arc::clone(&core), f);
    let c = sched.lock();
    assert_eq!(
        c.parked_n, 0,
        "must not park a fiber whose ancestor is cancelled"
    );
    assert_eq!(c.global.len(), 1);
}

/// TICKET-118 (W13-8) — `take_runnable`'s drain trigger must requeue a park whose ancestor's cancel
/// tripped AFTER the fiber parked (not just before, as the park-gap guard above covers), so a
/// `cancel` racing a job's nursery `recv` still ends it instead of getting judged deadlocked.
#[test]
fn mnsched_take_runnable_drains_a_park_whose_ancestor_cancel_tripped_after_it_parked() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = mk_sched_with(1, Arc::clone(&cancel));
    let ancestor = Arc::new(AtomicBool::new(false));
    sched.lock().scopes[0].ancestors.push(Arc::clone(&ancestor));
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    sched.park(core_key(&core), Arc::clone(&core), f);
    assert_eq!(
        sched.lock().parked_n,
        1,
        "fiber parks while the ancestor is not yet cancelled"
    );
    crate::vm::trip_cancel_flag(&ancestor);
    assert!(matches!(
        sched.take_runnable_unattached(0, 1, 0),
        Take::Run(..)
    ));
    assert_eq!(
        sched.lock().parked_n,
        0,
        "drain trigger must requeue the park before judging deadlock"
    );
}

/// TICKET-118 (W13-8) — `poke_live_scheds` must take each live sched's core lock before it notifies,
/// not just call `notify_all` bare: a `cv.notify_all()` with no lock held can race a worker about to
/// wait and be lost, which is exactly the lost-wakeup shape `cancel`'s cancel store needs
/// closed. Proven by observing the poke's completion (a channel send AFTER it returns) stay blocked
/// while this test holds the sched's core lock, then unblock the instant the lock is released.
#[test]
fn poke_live_scheds_takes_each_sched_core_lock_before_notifying() {
    let sched = Arc::new(mk_sched(1));
    let registry: Vec<std::sync::Weak<MnSched>> = vec![Arc::downgrade(&sched)];
    let registry: crate::vm::SchedRegistry = Arc::new(Mutex::new(registry));

    let guard = sched.lock();
    let (tx, rx) = std::sync::mpsc::channel();
    let poke_registry = Arc::clone(&registry);
    let handle = std::thread::spawn(move || {
        crate::vm::sched::poke_live_scheds(&poke_registry);
        tx.send(()).unwrap();
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(100))
            .is_err(),
        "poke_live_scheds must block on the held core lock before it can notify"
    );
    drop(guard);
    assert!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok(),
        "poke_live_scheds must complete once the core lock is released"
    );
    handle.join().unwrap();
}

// An idle worker no longer judges its sched (plan 2026-11-17: one global awake count). Every
// parked task of a run is the deadlock in `tests/awake_count_grid.rs` cell `recv::task`.

// The cancel-teardown vetoes of the per-sched predicate are gone (plan 2026-11-17): a cancelled
// task is woken by its cancel, so it is awake until its `defer`s end. Covered by
// `spawn_cancel::parallel_defer_runs_on_cancelled_sibling` and the integration grid
// `tests/cancel_reaches_every_parked_party.rs` `cancel_grid_every_source_park_owner_and_thread_count`.

// The awake count is run-wide (plan 2026-11-17), so a live peer sched or job keeps it above 0.
// Covered by `tests/cousin_fed_deadlock_detected_at_two_workers.rs`,
// `tests/sibling_nursery_send_wakes_deeper_receiver.rs` and `tests/awake_count_grid.rs` cells
// `exec_join::job`, `nursery::task`.

/// TICKET-103 — park a fresh fiber for slot `task_index` of `scope_id` on its own channel. Returns
/// the channel so the caller keeps its park key alive.
fn park_in_scope(s: &MnSched, task_index: usize, scope_id: usize) -> Arc<ChannelCore> {
    let ch = empty_core();
    let mut f = mk_fiber(task_index);
    f.scope_id = scope_id;
    s.seed(vec![f]);
    let f = take_run(s);
    s.park(core_key(&ch), Arc::clone(&ch), f);
    ch
}

// A deadlock is the whole run, never one family (plan 2026-11-17). Covered by
// `tests/nested_nursery_genuine_deadlock_still_faults.rs` and
// `tests/chezzi_nested_nursery_deadlock.rs`
// `nested_nursery_genuine_deadlocks_still_fault_at_every_worker_count`.

/// TICKET-103 — a continuation scope shares its nursery's cancel token, so draining the origin
/// must requeue the continuation's parked fibers too, and leave every other scope's parked.
#[test]
fn cancel_drain_requeues_every_scope_sharing_the_cancel_token() {
    let s = mk_sched(1);
    let shared = Arc::new(AtomicBool::new(false));
    let s1 = s.register_scope(1, Arc::clone(&shared), Vec::new());
    let s2 = s.register_scope(1, Arc::clone(&shared), Vec::new());
    let _chans = [
        park_in_scope(&s, 0, 0),
        park_in_scope(&s, 1, s1),
        park_in_scope(&s, 2, s2),
    ];
    let before = s.runnable.load(Ordering::Relaxed);
    s.cancel_drain(s1);
    assert_eq!(s.lock().parked_n, 1, "only scope 0's fiber stays parked");
    assert_eq!(s.runnable.load(Ordering::Relaxed), before + 2);
}

/// TICKET-103 — a spawn into a scope that is no longer the sched's last opens a continuation scope
/// sharing its cancel token; a spawn into the continuation (now last) grows it in place.
#[test]
fn inject_or_extend_opens_a_continuation_when_the_target_is_not_last() {
    let s = mk_sched(1);
    let tok = Arc::new(AtomicBool::new(false));
    let s1 = s.register_scope(0, Arc::clone(&tok), Vec::new());
    let _s2 = s.register_scope(0, Arc::new(AtomicBool::new(false)), Vec::new());
    assert_eq!(s.inject_or_extend(mk_pending_fiber(0), s1).0, Some(3));
    {
        let c = s.lock();
        assert_eq!(c.scopes[3].total, 1);
        assert!(Arc::ptr_eq(&c.scopes[3].cancel, &tok));
        assert_eq!(
            c.scopes[s1].total, 0,
            "the origin never grows past a later scope"
        );
    }
    assert_eq!(s.inject_or_extend(mk_pending_fiber(0), 3).0, None);
    assert_eq!(s.lock().scopes[3].total, 2);
}

/// TICKET-211 — the one runner claim never claims more than the worker count, claims up to it
/// while tasks wait, and claims a freed bit again. On a sched whose runners are all claimed at
/// build (`MnSched::mark_all_runners_claimed`), the claim starts nothing.
#[test]
fn claim_runners_never_exceeds_the_worker_count() {
    let set_bits = |s: &MnSched| s.lock().runner_wids.iter().filter(|b| **b).count();
    for n in 1..=12usize {
        let s = MnSched::new(0, n + 1, Arc::new(AtomicBool::new(false)), dl_err(), 0);
        {
            let mut c = s.lock();
            c.runner_wids[0] = true;
            c.runner_wids[1] = true;
        }
        for _ in 0..2 * n {
            let _ = s.inject_or_extend(mk_pending_fiber(0), 0);
            assert!(set_bits(&s) <= n.max(2), "n={n}: {} bits", set_bits(&s));
        }
        assert_eq!(
            set_bits(&s),
            n.max(2),
            "n={n}: waiting tasks claim up to the count"
        );
        // A runner leaving clears its bit (`close_body` for bit 0); the next claim takes it again.
        s.lock().runner_wids[0] = false;
        let _ = s.inject_or_extend(mk_pending_fiber(0), 0);
        assert_eq!(set_bits(&s), if n >= 2 { n } else { 1 }, "n={n}");

        let lazy = MnSched::new(0, n, Arc::new(AtomicBool::new(false)), dl_err(), 0);
        lazy.mark_all_runners_claimed();
        let scope =
            lazy.register_scope_seeded(Arc::new(AtomicBool::new(false)), Vec::new(), Vec::new());
        for _ in 0..2 * n {
            let (_, wids) = lazy.inject_or_extend(mk_pending_fiber(0), scope);
            assert!(wids.is_empty(), "n={n}: the lazy sched claimed {wids:?}");
        }
    }
}

/// TICKET-128 — a scope-scoped owner stop must wait for every continuation scope of its family,
/// not just the origin scope.
#[test]
fn owner_stop_waits_for_every_continuation_scope_of_its_family() {
    let s = mk_sched(1);
    let tok = Arc::new(AtomicBool::new(false));
    let s1 = s.register_scope(1, Arc::clone(&tok), Vec::new());
    let _s2 = s.register_scope(0, Arc::new(AtomicBool::new(false)), Vec::new());
    assert_eq!(s.inject_or_extend(mk_pending_fiber(0), s1).0, Some(3));
    let mut c = s.lock();
    c.scopes[s1].done = 1;
    assert!(
        !c.owner_scope_done(s1),
        "continuation scope 3 is still running"
    );
    c.scopes[3].done = 1;
    assert!(c.owner_scope_done(s1));
}

/// TICKET-103 fixture `S1`: owner A (slot 0, scope 0) join-parked on scope `s1`, whose one task C
/// is slot 1. Returns `s1`.
fn park_join_owner(s: &MnSched) -> usize {
    let s1 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    s.seed(vec![mk_fiber(0)]);
    let a = take_run(s);
    s.park_join(a, s1);
    s1
}

/// TICKET-103 fixture `S1`, second half: child C runs in `s1` and parks on an empty channel.
fn park_join_child(s: &MnSched, s1: usize) -> Arc<ChannelCore> {
    let mut cf = mk_fiber(1);
    cf.scope_id = s1;
    s.seed(vec![cf]);
    let cf = take_run(s);
    let ch = empty_core();
    s.park(core_key(&ch), Arc::clone(&ch), cf);
    ch
}

/// TICKET-103 — an owner whose joined family is incomplete is filed out of `running` and
/// `parked_n` and requeued once its family completes.
#[test]
fn park_join_files_the_owner_and_wake_completed_joins_requeues_it_when_its_family_is_done() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let mut c = s.lock();
    assert_eq!(c.join_parked.len(), 1);
    assert_eq!(c.running, 0);
    assert_eq!(c.parked_n, 0);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 0);
    c.scopes[s1].done = 1;
    let woken = s.wake_completed_joins(&mut c);
    assert_eq!(woken, 1);
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 1);
}

/// TICKET-103 — the park gap: a family that completed between the owner's check and the filing
/// requeues the owner at once instead of stranding it.
#[test]
fn park_join_requeues_at_once_when_the_family_is_already_done() {
    let s = mk_sched(1);
    let s1 = s.register_scope(0, Arc::new(AtomicBool::new(false)), Vec::new());
    s.seed(vec![mk_fiber(0)]);
    let a = take_run(&s);
    s.park_join(a, s1);
    let c = s.lock();
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(c.running, 0);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 1);
}

/// TICKET-103 — the task that completes a joined family requeues the family's join-parked owner.
#[test]
fn finish_wakes_the_join_parked_owner_of_the_completed_family() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let mut cf = mk_fiber(1);
    cf.scope_id = s1;
    s.seed(vec![cf]);
    let cf = take_run(&s);
    s.finish(
        cf.task_index,
        s1,
        TaskOutcome::Deadlocked {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
            verdict: 1,
        },
    );
    let c = s.lock();
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 1);
    assert_eq!(c.running, 0);
    assert!(!c.terminate);
}

// One `on_deadlock` lists every task (plan 2026-11-17). A join-parked
// owner is listed at its join in `tests/exit_status.rs`
// `every_deadlock_shape_lists_each_blocked_task_once_in_spawn_tree_order`; a stuck cleanup of a
// cancelled scope in `tests/callback_park_grid.rs`
// `stuck_cleanup_is_the_deadlock_on_every_former_verdict_path`.

/// TICKET-103 — a cancel drain requeues channel-parked fibers of the family only; a join-parked
/// owner stays filed until its children finish.
#[test]
fn cancel_drain_leaves_a_join_parked_owner_filed() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let _ch = park_join_child(&s, s1);
    s.cancel_drain(0);
    let c = s.lock();
    assert_eq!(c.join_parked.len(), 1);
    assert_eq!(c.parked_n, 1);
    assert!(c.global.is_empty());
}

// Two scheds blocked one after the other end in the deadlock in
// `tests/cousin_fed_deadlock_detected_at_two_workers.rs`.

// Main at a `parallel:` join is asleep in the awake count (plan 2026-11-17). Covered by
// `tests/awake_count_grid.rs` cells `nursery::main`, `nursery::task`.

/// D2b: `finish` records a task's outcome in its slot, drops it from `running`, and flips
/// `terminate` once every task is done.
#[test]
fn mnsched_finish_writes_slot_and_terminates_at_total() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.scopes[0].done, 1);
        assert!(!c.terminate);
    }
    sched.finish(
        b.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.scopes[0].done, 2);
        assert!(c.terminate);
    }
    assert!(matches!(
        sched.take_runnable_unattached(0, 1, 0),
        Take::Stop
    ));
}

/// D2b/U5 (mechanics half): `cancel_drain` moves every parked fiber back onto the run queue so a
/// worker resumes it and it observes the cancel flag on its next dispatch back-edge.
#[test]
fn mnsched_cancel_drain_requeues_parked() {
    let sched = mk_sched(2);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.park(core_key(&c1), Arc::clone(&c1), a);
    sched.park(core_key(&c2), Arc::clone(&c2), b);
    sched.cancel_drain(0);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0);
    assert_eq!(c.global.len(), 2);
}

/// D2b/G1 (headline): #fibers ≫ #threads. 64 consumer fibers each block on an empty channel while
/// 64 producer fibers send. On the legacy "one OS thread per task, block the thread on `recv`"
/// engine the blocked consumers pin every pool thread and the queued producers never run
/// (starvation/hang). Under the M:N engine the consumers PARK (freeing their workers), the
/// producers run and wake them, and the sum completes.
#[test]
fn mn_many_blocked_consumers_complete_without_starving() {
    let src = "import Shared from std.concurrency\n\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    v := ch.recv() else: panic(\"ch closed\")
    acc.update(fn(x): x + v)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        for i in 0..64:
            spawn consumer(ch, acc)
        for i in 0..64:
            spawn producer(ch, i)
    print(acc.get())
main()
";
    assert_eq!(run_capture(src).unwrap(), "2016\n"); // sum 0..64
}

/// D2b: 1000 producer fibers + 1 consumer recv-looping 1000 times, multiplexed over the
/// core-sized pool — 1001 fibers on ~N threads, no thread-per-fiber. The consumer parks between
/// sends and resumes via the rewound-ip `recv` replay.
#[test]
fn mn_thousand_fiber_pipeline_completes() {
    let src = "import Shared from std.concurrency\n\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    total := 0
    for _ in 0..1000:
        got := ch.recv() else: panic(\"ch closed\")
        total = total + got
    acc.update(fn(x): x + total)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        spawn consumer(ch, acc)
        for i in 0..1000:
            spawn producer(ch, i)
    print(acc.get())
main()
";
    assert_eq!(run_capture(src).unwrap(), "499500\n"); // sum 0..1000
}

/// M1.7: every context of a run shares one heap, so `swap_ctx` never moves the heap.
#[test]
fn swap_ctx_keeps_the_heap() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
    let core = vm.heap.id();
    let mut ctx = FiberCtx::default();
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.heap.id(), core);
    assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.heap.id(), core);
}

/// M1.7 GC canary: a `collect` while a fiber is swapped in roots the swapped-in fiber through the
/// live `Vm` fields and the parked host context through the registry (the host's state parks in
/// the fiber's registered `CtxBox`). After the fiber parks back out, both objects are intact.
#[test]
fn collect_under_swapped_in_fiber_roots_the_parked_host() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
    vm.push(Value::obj(hv)); // keep the host object stack-rooted
    let hf = vm.heap.alloc(Obj::Str("fiber-obj".into()));
    let mut ctx = vm.root_registry.register(FiberCtx {
        owns_side: true,
        stack: vec![Value::obj(hf)], // the fiber's own stack roots its object
        ..FiberCtx::default()
    });

    // Schedule in: the host stack parks into the registered ctx; `self.stack` is the fiber's.
    vm.swap_ctx(&mut ctx);
    vm.collect();
    assert!(matches!(vm.heap.get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));
    assert!(vm.heap.holds(hv), "the parked host's object survives");

    // Park back out: the host stack and its object are restored.
    vm.swap_ctx(&mut ctx);
    assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
    assert_eq!(vm.pop(), Value::obj(hv));
}

/// M1.7: one heap, one sweep. An object that no context roots is garbage whichever context made
/// it, so a `collect` while a fiber is swapped in frees the host's unrooted object too.
#[test]
fn collect_under_swapped_in_fiber_sweeps_the_whole_heap() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let garbage = vm.heap.alloc(Obj::Str("host-garbage".into()));
    let hf = vm.heap.alloc(Obj::Str("fiber-obj".into()));
    let mut ctx = vm.root_registry.register(FiberCtx {
        owns_side: true,
        stack: vec![Value::obj(hf)],
        ..FiberCtx::default()
    });

    vm.swap_ctx(&mut ctx);
    vm.collect();
    vm.swap_ctx(&mut ctx);

    assert!(!vm.heap.holds(garbage), "the unrooted host object is freed");
    assert!(vm.heap.holds(hf), "the parked fiber's object survives");
}

/// B3.1: `shut` lives in the shared core, so a second handle on the core observes a shutdown done
/// through the first — `submit` on the alias then settles its Task with the byte-identical
/// refusal (TICKET-289: a refused submit is no longer a fault of the caller).
#[test]
fn executor_core_shut_is_shared_across_handles() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let core = Arc::new(ExecutorCore::default());
    let h1 = vm.heap.alloc(Obj::Executor(Arc::clone(&core)));
    let h2 = vm.heap.alloc(Obj::Executor(core));
    let sp = Span::RUNTIME;
    vm.executor_method(
        h1,
        crate::base::builtin_method::ExecutorMethod::Shutdown,
        &[],
        sp,
    )
    .unwrap();
    let ch = Arc::new(ChannelCore::default());
    let chv = vm.heap.alloc(Obj::Channel(Arc::clone(&ch)));
    vm.executor_method(
        h2,
        crate::base::builtin_method::ExecutorMethod::StdSubmitSettled,
        &[Value::nil(), Value::obj(chv)],
        sp,
    )
    .expect("a refused submit returns Ok with its Task settled");
    assert!(
        ch.q.lock().unwrap().is_sealed(),
        "the refusal must seal the Task's channel"
    );
    // M3.2: the refusal is sealed as text, and the receive builds it into the `Err` value.
    let got = vm
        .channel_method(
            chv,
            crate::base::builtin_method::ChannelMethod::Recv,
            &[],
            sp,
        )
        .unwrap();
    let sealed = vm.display(got);
    assert!(
        sealed.contains("submit on a shut-down Executor (it no longer accepts work)"),
        "sealed with: {sealed}"
    );
}

/// B3.1: `display` of a `Shared` box renders its contents. M3.3: the box holds the `str` itself,
/// by handle.
#[test]
fn display_shared_renders_contents() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("hi".into()));
    let core = Arc::new(SharedCore::new(Value::obj(s)));
    let sh = vm.heap.alloc(Obj::Shared(core));
    // The payload sits INSIDE `Shared(…)`, i.e. nested — so it renders as its `repr` (W7-25),
    // matching `print(s.get())` on the same box.
    assert_eq!(vm.display(Value::obj(sh)), "Shared('hi')");
}
