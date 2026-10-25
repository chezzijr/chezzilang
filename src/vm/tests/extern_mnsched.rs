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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on colliding qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on colliding qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on colliding qualified width-alias extern: {par_res:?}"
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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on chained qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on chained qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on chained qualified width-alias extern: {par_res:?}"
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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on depth-3 chained qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on depth-3 chained qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on depth-3 chained qualified width-alias extern: {par_res:?}"
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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on named-import-hop qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on named-import-hop qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on named-import-hop qualified width-alias extern: {par_res:?}"
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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on qualified return struct with aliased fields: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on qualified return struct with aliased fields: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on qualified return struct with aliased fields: {par_res:?}"
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
    let (_io, _ie, ir, _) = run_file(&entry);
    let (_par_out, _pe, par_res, _) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on mixed-chain qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on mixed-chain qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on mixed-chain qualified width-alias extern: {par_res:?}"
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
        total += out.recv()
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
    let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err(), 0);
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

/// D2b/U4: every not-done fiber parked, none running, run queue empty ⇒ deadlock. `take_runnable`
/// detects it, records a `Deadlocked` outcome (`err.message == DEADLOCK_MSG`) for every parked
/// fiber, and terminates.
#[test]
fn mnsched_deadlock_when_all_parked_runq_empty() {
    let sched = mk_sched(2);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.park(core_key(&c1), Arc::clone(&c1), a);
    sched.park(core_key(&c2), Arc::clone(&c2), b);
    assert!(matches!(
        sched.take_runnable_unattached(0, 1, 0),
        Take::Stop
    ));
    let slots = sched.take_slots();
    assert_eq!(slots.len(), 2);
    for s in slots {
        assert!(
            matches!(s, Some(TaskOutcome::Deadlocked { err, .. }) if err.message == DEADLOCK_MSG)
        );
    }
}

/// N4 (cancel-teardown veto): a scope whose `cancel` is tripped but whose tasks have not all
/// settled is MID-TEARDOWN, not deadlocked. Every cancel trip and its `cancel_drain` are two
/// separate core-lock acquisitions apart (`mn_worker_loop`'s `finish`→`cancel_drain`,
/// `abort_enlisted_scope`, `abort_eager_nursery`), and an idle worker's `take_runnable` can land in
/// that gap and see the pre-drain quiesce (`running == 0 && runnable == 0 && parked_n > 0`). Without
/// the veto it declares DEADLOCK and `flag_deadlock` DROPS the parked siblings without running
/// `unwind_deferred` — silently skipping their `defer`s. Pins the invariant itself (the scenario
/// repro is `parallel_defer_runs_on_cancelled_sibling`).
#[test]
fn mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock() {
    // Scope 0's `JoinScope::cancel` IS this Arc (see `mnsched_park_requeues_when_cancel_tripped`).
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Cancel still false → these really park (the park-gap re-check does not requeue them).
    sched.park(core_key(&c1), Arc::clone(&c1), a);
    sched.park(core_key(&c2), Arc::clone(&c2), b);
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 2, "both fibers parked (quiesced, pre-cancel)");
    }
    // A sibling faults: `classify_mn_outcome` trips the scope cancel BEFORE `finish`/`cancel_drain`.
    cancel.store(true, Ordering::Relaxed);
    let c = sched.lock();
    assert!(
        !sched.is_deadlocked(&c),
        "a cancel-tripped scope mid-teardown is not deadlocked (its parked fibers are about to be \
         drained by `cancel_drain` so they can unwind their defers)"
    );
}

/// N4 boundary (the other half): the veto covers the trip→`cancel_drain` window ONLY. A cancelled
/// scope whose last unsettled fiber is DEMOTED — blocked in place inside its own `defer` on a `recv`
/// nobody will ever answer — has no undrained park left, so nothing can ever wake it: that IS a
/// genuine deadlock and must be REPORTED, never left to hang silently (`demote_recv_block`'s own
/// `is_deadlocked` self-detect is what fires). Before the veto was bounded to the parked window the
/// scope stayed `done < total && cancel` forever (incomplete *because* that fiber is stuck), so the
/// veto never lifted → silent M:N hang (repro: a `defer` whose body does a `recv` nobody answers).
#[test]
fn mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Fiber b enters its `defer` and blocks in place on an empty channel (running → a registered waiter).
    // Inside a `defer` a cancel is SUPPRESSED (`cancel_requested()`'s `deferring == 0` term), so it
    // registers NO cancel watch — nothing can ever wake it. That is what makes it a real deadlock.
    {
        let mut c = sched.lock();
        c.running -= 1;
        c.register_waiter(recv_waiter(&core, vec![]));
    }
    // Sibling a faults: trips the scope cancel, then finishes. No fiber of the scope is parked.
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
            trace: Vec::new(),
        },
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no fiber of the cancelled scope is parked");
    assert!(
        sched.is_deadlocked(&c),
        "a cancelled scope whose remaining fiber is demoted-blocked forever inside its cleanup is a \
         REAL deadlock — report it, never hang"
    );
    let _ = b;
}

/// N4 (the DEMOTED half of the cancel-teardown veto): a demoted fiber whose cancel flag is TRIPPED is
/// NOT stuck — `demote_recv_block` ranks `cancel_requested()` ABOVE `terminate` and its own deadlock
/// self-detect (sched.rs), so it resumes within one `DEMOTE_POLL_BACKOFF`, unwinds and runs its
/// `defer`s (which can `send`, waking parked siblings). CANCEL is a wakeup source the park/inflight/
/// `runnable` counters do not model, so without a veto an idle worker's `take_runnable` sees
/// `running == 0 && runnable == 0 && inflight == 0 && any_fiber_waiter()` and declares a SPURIOUS
/// deadlock: `flag_deadlock` then reaps every parked fiber of EVERY scope with no `unwind_deferred`
/// (their `defer`s silently skipped — the exact N4 harm) and LATCHES `terminate`, which truncates the
/// cleanup of any sibling demoted inside its own `defer`. The parked-only veto
/// (`any_cancelled_scope_awaiting_drain`) cannot see this fiber — it is a registered waiter, not
/// `parked` — hence the explicit watch. Boundary vs the test above: a fiber demoted INSIDE a `defer`
/// registers no watch and stays a genuine deadlock, so the hang fix is preserved.
#[test]
fn mnsched_demoted_fiber_with_a_tripped_cancel_is_not_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Fiber b blocks in place on an empty channel from inside a native callback, in its BODY
    // (`deferring == 0`) — a cancel would be honoured, so it watches its scope's flag.
    let tok = {
        let mut c = sched.lock();
        c.running -= 1;
        c.register_waiter(recv_waiter(&core, vec![Arc::clone(&cancel)]))
    };
    // Sibling a faults: trips the scope cancel, then finishes. No fiber of the scope is parked, so
    // the parked-window veto is silent — only the demoted watch can save b.
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
            trace: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "no fiber of the cancelled scope is parked");
        assert!(
            !sched.is_deadlocked(&c),
            "a demoted fiber with a tripped cancel is about to resume and unwind — that is progress, \
             not a deadlock (declaring one latches `terminate` and skips every parked fiber's defers)"
        );
    }
    // It resumes, unwinds and blocks again inside its own `defer`, where a cancel is no longer
    // honoured: its waiter carries no cancel flag, the veto lifts, and a genuine post-teardown
    // quiesce still fires (no stale cancel entry vetoing forever).
    {
        let mut c = sched.lock();
        c.unregister_waiter(tok);
        c.register_waiter(recv_waiter(&core, vec![]));
        assert!(
            sched.is_deadlocked(&c),
            "once the demoted fiber has settled the veto must lift"
        );
    }
    let _ = b;
}

/// N4 boundary (pins the veto's remaining half): a cancelled scope with BOTH an undrained parked
/// fiber and a demoted one is still mid-teardown — the parked one is about to be requeued by
/// `cancel_drain` to unwind its `defer`s, so the deadlock must NOT fire (`flag_deadlock` would drop it
/// without `unwind_deferred`). If this flips, the parked-scan is keyed wrong.
#[test]
fn mnsched_cancelled_scope_with_a_parked_and_a_demoted_fiber_is_not_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(3, 4, Arc::clone(&cancel), dl_err(), 0);
    let park_core = empty_core();
    let demote_core = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    let d = take_run(&sched);
    sched.park(core_key(&park_core), Arc::clone(&park_core), b); // cancel still false → really parks
    {
        let mut c = sched.lock();
        c.running -= 1;
        c.register_waiter(recv_waiter(&demote_core, vec![]));
    }
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
            trace: Vec::new(),
        },
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 1);
    assert!(
        !sched.is_deadlocked(&c),
        "an undrained parked fiber of the cancelled scope still vetoes: it is about to be requeued \
         by `cancel_drain` to unwind its defers"
    );
    let _ = d;
}

// ----- gaps.md W7-58 — the nursery OWNER is a counted party of the process-wide verdict -----

/// W7-58, the REFACTOR FENCE. `is_deadlocked` was split into "the W7-56 outstanding-job veto" plus
/// `is_deadlocked_ignoring_jobs` (the old body).
///
/// **It asserts the VERDICT, not the agreement.** With an empty registry `is_deadlocked` literally
/// delegates, so `f(x) == f(x)` is a tautology and would stay green if the move had dropped a gate on
/// the way. So each state below pins the concrete expected answer: drop `any_body_open`,
/// `all_incomplete_awaiting_builder` or `any_cancelled_scope_awaiting_drain` from the moved body and
/// exactly one of these flips. The delegation check rides along, but it is not the fence.
///
/// Walks the states the predicate's own gates discriminate, in order.
#[test]
fn w758_is_deadlocked_ignoring_jobs_matches_is_deadlocked_with_no_executors() {
    let check = |sched: &MnSched, want: bool, what: &str| {
        let c = sched.lock();
        assert_eq!(
            sched.is_deadlocked_ignoring_jobs(&c),
            want,
            "wrong verdict for: {what}"
        );
        assert_eq!(
            sched.is_deadlocked(&c),
            sched.is_deadlocked_ignoring_jobs(&c),
            "with an empty registry the veto is a no-op, so the two must agree: {what}"
        );
    };
    // (a) fresh, nothing seeded — no incomplete scope, so nothing to be stuck about.
    check(&mk_sched(0), false, "no tasks at all");
    // (b) seeded and runnable — work is queued.
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    check(&sched, false, "one runnable fiber");
    // (c) running.
    let f = take_run(&sched);
    check(&sched, false, "one running fiber");
    // (d) all parked, nothing runnable/inflight → the genuine deadlock state.
    let chan = empty_core();
    sched.park(core_key(&chan), Arc::clone(&chan), f);
    check(&sched, true, "all parked with no possible feeder");
    // (e) `body_open` veto — an eager nursery body may still `inject` a feeder.
    sched.open_body(0);
    check(&sched, false, "body open");
    sched.close_body(0);
    check(&sched, true, "body closed again");
    // (f) `awaiting_builder` veto — needs a SECOND scope (`all_incomplete_awaiting_builder` returns
    // false at `scopes.count() == 1`), i.e. exactly the early-enlisted shape it exists for.
    let two = mk_sched(1);
    let s1 = two.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    two.seed(vec![mk_fiber(0)]);
    let g = take_run(&two);
    two.park(core_key(&chan), Arc::clone(&chan), g);
    check(&two, true, "two incomplete scopes, all parked");
    {
        let mut c = two.lock();
        c.scopes[0].awaiting_builder = true;
        c.scopes[s1].awaiting_builder = true;
    }
    check(&two, false, "every incomplete scope awaits its builder");
    // (g) cancelled-scope-awaiting-drain veto — its parked fibers are about to be requeued to unwind.
    sched.trip_scope_cancel(0);
    check(&sched, false, "cancelled scope awaiting drain");
    sched.cancel_drain(0);
}

/// TICKET-099 — a live PEER sched (still has a running/runnable fiber) vetoes the deadlock verdict of
/// a sched whose own fibers are all parked. `local_quiesced` stays `true` (the sched genuinely cannot
/// move on its own); only `is_deadlocked_ignoring_jobs`, which adds the peer veto, must flip.
#[test]
fn mnsched_a_live_peer_sched_vetoes_the_deadlock() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    a.seed(vec![mk_fiber(0)]);
    let f = take_run(&a);
    let chan = empty_core();
    a.park(core_key(&chan), Arc::clone(&chan), f);

    // B keeps its seeded fiber runnable — never taken, so B's own `running`/`runnable` shows work.
    b.seed(vec![mk_fiber(0)]);

    let c = a.lock();
    assert!(
        a.local_quiesced(&c),
        "A's own fibers are all parked, so its LOCAL predicate must still say quiesced"
    );
    assert!(
        !a.is_deadlocked_ignoring_jobs(&c),
        "B still has runnable work, so the peer veto must decline A's fault"
    );
}

/// TICKET-099 — the peer veto lifts once EVERY sched is genuinely quiesced: two private scheds that
/// are BOTH fully parked must still fire the deadlock, or a real deadlock hangs forever instead of
/// faulting.
#[test]
fn mnsched_two_quiesced_scheds_still_fire_the_deadlock() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let chan = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&chan), Arc::clone(&chan), fa);
    b.seed(vec![mk_fiber(0)]);
    let fb = take_run(&b);
    b.park(core_key(&chan), Arc::clone(&chan), fb);

    let c = a.lock();
    assert!(
        a.is_deadlocked_ignoring_jobs(&c),
        "B is ALSO fully parked with no runnable work, so the veto must not apply and the genuine \
         deadlock must still fire"
    );
}

/// TICKET-099 — a peer whose only non-parked fiber is an OWNER blocked at a nested join must NOT veto:
/// that fiber can feed nobody, whichever sched its child nursery runs on (the cross-sched widening of
/// `blocked_owner_guard`). Before that widening, B's join-blocked fiber counted only in `running`, so
/// `local_quiesced` read `false` on B and this veto declined forever — the exact hang measured on
/// `parity_nested_deadlock_cancels_the_outer_parked_siblings_defer` at `fdc71d70`.
#[test]
fn mnsched_a_peer_blocked_only_in_a_nested_join_does_not_veto() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(2);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let chan = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&chan), Arc::clone(&chan), fa);

    // B seeds two fibers: park the first normally, take-but-don't-park the second — simulating a
    // fiber blocked inline in a nested nursery join (counted via `blocked_owners`, not `parked`).
    b.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&b);
    let _f1 = take_run(&b);
    b.park(core_key(&chan), Arc::clone(&chan), f0);
    {
        let mut cb = b.lock();
        cb.running = 1;
        cb.blocked_owners = 1;
    }

    let c = a.lock();
    assert!(
        a.is_deadlocked_ignoring_jobs(&c),
        "B's only non-parked fiber is an owner blocked at a nested join, which can feed nobody, so \
         the peer veto must not apply"
    );
}

/// TICKET-101 — same property as the fixture above, but B's ONLY fiber (not a second one alongside a
/// parked first) is the join-blocked owner, so `parked_n` is 0 on B. The predecessor fixture's peer
/// parks a first fiber before taking the second, which satisfies the D5 Path-C parked-or-demoted
/// clause by accident and passes even when the peer question still wrongly demands a parked victim.
/// This fixture removes that accident.
#[test]
fn mnsched_a_peer_whose_only_fiber_is_a_join_blocked_owner_does_not_veto() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let chan = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&chan), Arc::clone(&chan), fa);

    // B seeds exactly one fiber, taken but never parked — an owner blocked inline at a nested join,
    // counted only via `blocked_owners`. Nothing on B is parked.
    b.seed(vec![mk_fiber(0)]);
    let _fb = take_run(&b);
    {
        let mut cb = b.lock();
        cb.running = 1;
        cb.blocked_owners = 1;
        assert_eq!(
            cb.parked_n, 0,
            "the fixture's whole point is an unparked peer"
        );
    }

    let c = a.lock();
    assert!(
        a.is_deadlocked_ignoring_jobs(&c),
        "B's only fiber is an owner blocked at a nested join and nothing on B is parked, so B can \
         feed nobody and the peer veto must not apply"
    );
}

/// TICKET-112 — a private nested sched whose body is run by a fiber of ANOTHER sched does not veto a
/// peer's verdict on that open body: the fiber is counted on its own sched, which answers for it.
#[test]
fn mnsched_a_fiber_body_peer_whose_children_are_parked_does_not_veto() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    inner_b.body_is_fiber = true;
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let ch_a = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&ch_a), Arc::clone(&ch_a), fa);

    let ch_b = empty_core();
    b.open_body(0);
    b.seed(vec![mk_fiber(0)]);
    let fb = take_run(&b);
    b.park(core_key(&ch_b), Arc::clone(&ch_b), fb);

    let c = a.lock();
    assert!(
        a.is_deadlocked_ignoring_jobs(&c),
        "B's only child is parked and B's open body is a fiber counted on another sched, so B \
         must not veto A"
    );
}

/// TICKET-112 — the same sched never faults its own fibers while that body is open: the body's
/// fiber may still feed them. Once the body reaches its join, the sched judges itself again.
#[test]
fn mnsched_a_fiber_body_sched_declines_its_own_verdict_until_its_body_closes() {
    let mut inner = mk_sched(1);
    inner.body_is_fiber = true;
    let s = Arc::new(inner);
    let ch = empty_core();
    s.open_body(0);
    s.seed(vec![mk_fiber(0)]);
    let f = take_run(&s);
    s.park(core_key(&ch), Arc::clone(&ch), f);
    assert!(
        s.local_quiesced(&s.lock()),
        "every fiber of this sched is parked, and its open body is not this sched's to count"
    );
    assert!(
        !s.is_deadlocked_ignoring_jobs(&s.lock()),
        "the fiber running the open body may still feed the parked child"
    );
    s.close_body(0);
    assert!(
        s.is_deadlocked_ignoring_jobs(&s.lock()),
        "with the body at its join, nothing can feed the parked child"
    );
}

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

/// TICKET-103 (`cousin_fed`) — owner A (scope 0) is blocked joining `s1`, whose child C is stuck.
/// F (scope 0) and D (scope `s2`) are parked too, but A may feed either once its `recover:`
/// returns, so only C is faulted and the sched keeps running.
#[test]
fn flag_deadlock_leaves_faults_only_the_family_a_blocked_owner_joins() {
    let s = mk_sched(2); // slot 0 = owner A, slot 1 = F
    let s1 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // slot 2 = C
    let s2 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // slot 3 = D
    let _chans = [
        park_in_scope(&s, 1, 0),
        park_in_scope(&s, 2, s1),
        park_in_scope(&s, 3, s2),
    ];
    let mut c = s.lock();
    c.running = 1;
    c.blocked_owners = 1;
    c.scopes[0].owners_blocked = 1;
    c.scopes[s1].joins_blocked = 1;
    let r = c
        .flag_deadlock_leaves(&s.deadlock_err, true)
        .expect("unproven_ok never declines");
    assert!(!r, "a leaf flag must not report a sched-wide stop");
    assert!(!c.terminate, "a leaf flag must not terminate the sched");
    assert!(
        matches!(c.slots[2], Some(TaskOutcome::Deadlocked { .. })),
        "C is the stuck leaf"
    );
    assert!(c.slots[1].is_none(), "F stays parked for A to feed");
    assert!(
        c.slots[3].is_none(),
        "D stays parked: A feeds by channel, not by family"
    );
    assert_eq!(c.parked_n, 2);
}

/// TICKET-103 — with no joined family the flag stays sched-wide, exactly as `flag_deadlock`.
#[test]
fn flag_deadlock_leaves_is_sched_wide_without_a_joined_family() {
    let s = mk_sched(2);
    let s1 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    let s2 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    let _chans = [
        park_in_scope(&s, 1, 0),
        park_in_scope(&s, 2, s1),
        park_in_scope(&s, 3, s2),
    ];
    let mut c = s.lock();
    c.running = 1;
    c.blocked_owners = 1;
    c.scopes[0].owners_blocked = 1;
    let r = c
        .flag_deadlock_leaves(&s.deadlock_err, true)
        .expect("unproven_ok never declines");
    assert!(r);
    assert!(c.terminate);
    for i in 1..=3 {
        assert!(
            matches!(c.slots[i], Some(TaskOutcome::Deadlocked { .. })),
            "slot {i} must be flagged"
        );
    }
}

/// TICKET-129 (W13-5 residual) — an unproven sched-wide flag must DECLINE (`None`) when
/// `unproven_ok` is false, and only fault (`Some(true)`) when licensed. A `mk_fiber` has no heap,
/// so its leaf is never provable.
#[test]
fn flag_deadlock_leaves_declines_an_unproven_sched_wide_flag() {
    let s = mk_sched(1);
    let _ch = park_in_scope(&s, 0, 0);
    let mut c = s.lock();
    assert_eq!(c.flag_deadlock_leaves(&s.deadlock_err, false), None);
    assert_eq!(c.parked_n, 1);
    assert!(!c.terminate);
    assert_eq!(c.flag_deadlock_leaves(&s.deadlock_err, true), Some(true));
    assert!(c.terminate);
}

/// M3.2 fix 1 — a channel slot can be shared: a value received by handle or a module global is
/// the SAME `Obj::Channel` slot in two tasks, and one slot holds one `Arc`. Here the parked fiber's
/// stack reaches the slot, and `held` stands for a running task that holds the same slot and can
/// still send. No count of `Arc` holders can see that second holder, so an unlicensed verdict must
/// decline; only `may_fault_unproven` (every peer parked) may license it.
#[test]
fn a_shared_channel_slot_never_proves_a_deadlock() {
    let s = mk_sched(1);
    let mut heap = Heap::new();
    let ch = empty_core();
    let key = core_key(&ch);
    let held = heap.alloc(Obj::Channel(Arc::clone(&ch)));
    let mut f = mk_fiber(0);
    f.ctx = CtxBox::unlisted(FiberCtx {
        stack: vec![Value::obj(held)],
        ..FiberCtx::default()
    });
    s.seed(vec![f]);
    let f = take_run(&s);
    s.park(key, ch, f);
    let mut c = s.lock();
    assert_eq!(
        c.flag_deadlock_leaves(&s.deadlock_err, false),
        None,
        "a slot another task may hold proves nothing"
    );
    assert_eq!(c.parked_n, 1);
    assert!(!c.terminate);
    let _ = held;
}

/// TICKET-129 (W13-5 residual) — `may_fault_unproven` must decline while a peer sched still has
/// runnable work: that peer may yet feed this sched's unprovable candidate.
#[test]
fn mnsched_may_fault_unproven_waits_for_a_peer_with_runnable_work() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    a.seed(vec![mk_fiber(0)]);
    let f = take_run(&a);
    let chan = empty_core();
    a.park(core_key(&chan), Arc::clone(&chan), f);

    // B keeps its seeded fiber runnable — never taken, so B's own `running`/`runnable` shows work.
    b.seed(vec![mk_fiber(0)]);

    assert!(!a.may_fault_unproven(), "B still has runnable work");
}

/// TICKET-129 (W13-5 residual) — when every sched in the run is parked and unproven, the fallback
/// must still license the fault, or a genuine deadlock hangs forever instead of faulting.
#[test]
fn mnsched_may_fault_unproven_when_every_peer_is_parked_and_unproven() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let chan = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&chan), Arc::clone(&chan), fa);
    b.seed(vec![mk_fiber(0)]);
    let fb = take_run(&b);
    b.park(core_key(&chan), Arc::clone(&chan), fb);

    assert!(a.may_fault_unproven(), "every sched is parked and unproven");
}

/// TICKET-103 — leaf-ness is a FAMILY property. `s1`'s continuation `c1` holds owner B, blocked
/// joining `s2`. So `s1`'s family is interior and its parked P stays; only `s2`'s C is faulted.
#[test]
fn flag_deadlock_leaves_reads_leafness_over_the_joined_family() {
    let s = mk_sched(1); // slot 0 = owner A
    let tok = Arc::new(AtomicBool::new(false));
    let s1 = s.register_scope(1, Arc::clone(&tok), Vec::new()); // slot 1 = P
    let c1 = s.register_scope(1, Arc::clone(&tok), Vec::new()); // slot 2 = owner B, never seeded
    let s2 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // slot 3 = C
    let _chans = [park_in_scope(&s, 1, s1), park_in_scope(&s, 3, s2)];
    let mut c = s.lock();
    c.running = 2;
    c.blocked_owners = 2;
    c.scopes[0].owners_blocked = 1;
    c.scopes[s1].joins_blocked = 1;
    c.scopes[c1].owners_blocked = 1;
    c.scopes[s2].joins_blocked = 1;
    let r = c
        .flag_deadlock_leaves(&s.deadlock_err, true)
        .expect("unproven_ok never declines");
    assert!(!r);
    assert!(
        matches!(c.slots[3], Some(TaskOutcome::Deadlocked { .. })),
        "C is the stuck leaf"
    );
    assert!(
        c.slots[1].is_none(),
        "P's family holds a blocked owner, so it is interior"
    );
    assert_eq!(c.parked_n, 1);
}

/// TICKET-103 — the joined target is a FAMILY: a parked fiber in a continuation of the joined
/// scope is faulted with the origin's.
#[test]
fn flag_deadlock_leaves_faults_every_scope_of_the_joined_family() {
    let s = mk_sched(2); // slot 0 = owner A, slot 1 = B
    let tok = Arc::new(AtomicBool::new(false));
    let s1 = s.register_scope(1, Arc::clone(&tok), Vec::new()); // slot 2 = C
    let c1 = s.register_scope(1, Arc::clone(&tok), Vec::new()); // slot 3 = D, continuation of s1
    let _chans = [
        park_in_scope(&s, 1, 0),
        park_in_scope(&s, 2, s1),
        park_in_scope(&s, 3, c1),
    ];
    let mut c = s.lock();
    c.running = 1;
    c.blocked_owners = 1;
    c.scopes[0].owners_blocked = 1;
    c.scopes[s1].joins_blocked = 1;
    let r = c
        .flag_deadlock_leaves(&s.deadlock_err, true)
        .expect("unproven_ok never declines");
    assert!(!r);
    assert!(matches!(c.slots[2], Some(TaskOutcome::Deadlocked { .. })));
    assert!(
        matches!(c.slots[3], Some(TaskOutcome::Deadlocked { .. })),
        "the continuation too"
    );
    assert!(c.slots[1].is_none(), "B is outside the joined family");
    assert_eq!(c.parked_n, 1);
}

/// TICKET-103 — once a joined family is complete its owner resumes on its next pass, so the sched
/// is not quiesced until that owner's guard drops. The `done = 0` half is the positive control.
#[test]
fn a_blocked_owner_whose_joined_family_is_done_is_not_quiesced() {
    let s = mk_sched(2); // slot 0 = owner A, slot 1 = B
    let s1 = s.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // slot 2, no fiber
    let _ch = park_in_scope(&s, 1, 0);
    let mut c = s.lock();
    c.running = 1;
    c.blocked_owners = 1;
    c.scopes[0].owners_blocked = 1;
    c.scopes[s1].joins_blocked = 1;
    c.scopes[s1].done = 1;
    assert!(
        !s.local_quiesced(&c),
        "the joined family is done and its owner is about to resume"
    );
    c.scopes[s1].done = 0;
    assert!(
        s.local_quiesced(&c),
        "positive control: the fixture reaches the clause"
    );
}

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

/// TICKET-103 — a body's wait mark covers its whole family in one acquisition, and no other scope.
#[test]
fn set_body_wait_marks_every_scope_sharing_the_cancel_token() {
    let s = mk_sched(1);
    let other = s.register_scope(0, Arc::new(AtomicBool::new(false)), Vec::new());
    let tok = Arc::clone(&s.lock().scopes[0].cancel);
    let cont = s.register_scope(1, tok, Vec::new());
    s.set_body_wait(0, None, true, true);
    {
        let c = s.lock();
        assert!(c.scopes[cont].body_blocked && c.scopes[cont].awaiting_builder);
        assert!(!c.scopes[other].body_blocked && !c.scopes[other].awaiting_builder);
    }
    s.set_body_wait(0, None, false, true);
    let c = s.lock();
    assert!(
        c.scopes
            .values()
            .all(|x| !x.body_blocked && !x.awaiting_builder)
    );
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

/// TICKET-211 — the one runner claim never claims more than the worker count, a body block hands
/// the body's slot to a fiber runner, and the joiner is refused once the budget is full. On a lazy
/// sched (`run_mn_nursery_outermost`), whose runners are fixed at build, the claim starts nothing.
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
        s.open_body(0);
        for _ in 0..2 * n {
            let _ = s.inject_or_extend(mk_pending_fiber(0), 0);
            assert!(set_bits(&s) <= n.max(2), "n={n}: {} bits", set_bits(&s));
        }
        let _ = s.body_runner(true);
        assert!(
            !s.lock().runner_wids[0],
            "n={n}: a blocked body frees bit 0"
        );
        assert_eq!(set_bits(&s), if n >= 2 { n } else { 1 }, "n={n}");
        assert!(!s.claim_joiner(), "n={n}: the budget is full");

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
/// `parked_n`, counted on both join counters, and requeued once its family completes.
#[test]
fn park_join_files_the_owner_and_wake_completed_joins_requeues_it_when_its_family_is_done() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let mut c = s.lock();
    assert_eq!(c.join_parked.len(), 1);
    assert_eq!(c.running, 0);
    assert_eq!(c.parked_n, 0);
    assert_eq!(c.scopes[s1].joins_blocked, 1);
    assert_eq!(c.scopes[0].owners_blocked, 1);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 0);
    c.scopes[s1].done = 1;
    let woken = s.wake_completed_joins(&mut c);
    assert_eq!(woken, 1);
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 1);
    assert_eq!(c.scopes[s1].joins_blocked, 0);
    assert_eq!(c.scopes[0].owners_blocked, 0);
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
    assert_eq!(c.scopes[s1].joins_blocked, 0);
    assert_eq!(c.scopes[0].owners_blocked, 0);
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
        },
    );
    let c = s.lock();
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(s.runnable.load(Ordering::Relaxed), 1);
    assert_eq!(c.running, 0);
    assert!(!c.terminate);
}

/// TICKET-103 — the leaf flag completes the joined family, so it must requeue the family's
/// join-parked owner too, or the owner is stranded on a family nothing will bump again.
#[test]
fn flag_deadlock_leaves_wakes_the_join_parked_owner_of_the_faulted_leaf() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let _ch = park_join_child(&s, s1);
    let mut c = s.lock();
    let r = s
        .flag_leaves_and_wake(&mut c)
        .expect("an empty registry licenses the fault");
    assert!(!r);
    assert!(matches!(c.slots[1], Some(TaskOutcome::Deadlocked { .. })));
    assert!(c.slots[0].is_none());
    assert!(c.join_parked.is_empty());
    assert_eq!(c.global.len(), 1);
    assert_eq!(c.parked_n, 0);
    assert!(!c.terminate);
}

/// TICKET-103 — the sched-wide flag sets `terminate`, which stops every worker before its next
/// global pop, so a join-parked owner is recorded `Deadlocked` rather than requeued.
#[test]
fn flag_deadlock_records_a_join_parked_owner_as_deadlocked() {
    let s = mk_sched(1);
    let s1 = park_join_owner(&s);
    let _ch = park_join_child(&s, s1);
    let mut c = s.lock();
    c.flag_deadlock(&s.deadlock_err);
    assert!(matches!(c.slots[0], Some(TaskOutcome::Deadlocked { .. })));
    assert!(matches!(c.slots[1], Some(TaskOutcome::Deadlocked { .. })));
    assert!(c.join_parked.is_empty());
    assert!(c.terminate);
    assert_eq!(c.scopes[0].done, 1);
    assert_eq!(c.scopes[s1].done, 1);
    assert_eq!(c.scopes[s1].joins_blocked, 0);
    assert_eq!(c.scopes[0].owners_blocked, 0);
}

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

/// TICKET-099 — a vetoed sched must poll (`DEMOTE_POLL_BACKOFF`) rather than sleep on its OWN condvar
/// untimed: nothing notifies A's `cv` when peer B quiesces later, so an untimed wait here would hang
/// forever even though B eventually stops being a live peer.
#[test]
fn mnsched_a_vetoed_sched_polls_instead_of_parking_untimed() {
    let reg: crate::vm::SchedRegistry = Default::default();
    let mut inner_a = mk_sched(1);
    inner_a.sched_registry = Arc::clone(&reg);
    let a = Arc::new(inner_a);
    let mut inner_b = mk_sched(1);
    inner_b.sched_registry = Arc::clone(&reg);
    let b = Arc::new(inner_b);
    reg.lock().unwrap().push(Arc::downgrade(&a));
    reg.lock().unwrap().push(Arc::downgrade(&b));

    let chan = empty_core();
    a.seed(vec![mk_fiber(0)]);
    let fa = take_run(&a);
    a.park(core_key(&chan), Arc::clone(&chan), fa);
    // B keeps its seeded fiber runnable — A's fault decision is vetoed by it.
    b.seed(vec![mk_fiber(0)]);

    let (tx, rx) = std::sync::mpsc::channel();
    let a2 = Arc::clone(&a);
    std::thread::spawn(move || {
        tx.send(matches!(a2.take_runnable_unattached(0, 1, 0), Take::Stop))
            .ok();
    });

    // If A took the untimed `self.cv.wait(c)` path, nothing notifies A's cv here (parking B's fiber
    // below notifies B's condvar, never A's), so this MUST still be pending.
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_millis(200)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout),
        "A must not have stopped yet — its own fault decision is vetoed and it has nothing else to run"
    );

    // Park B's fiber too, so A's next `DEMOTE_POLL_BACKOFF` poll sees the genuine deadlock and fires.
    let fb = take_run(&b);
    b.park(core_key(&chan), Arc::clone(&chan), fb);

    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2)),
        Ok(true),
        "a vetoed sched must be polling (DEMOTE_POLL_BACKOFF), not parked untimed on its own cv — \
         otherwise it never re-examines B and hangs forever"
    );
}

/// W7-58 — `PartyWait::Nursery` answers the nursery's OWN predicate, live, on every evaluation.
///
/// This is the "a wait predicate that answers a CONSTANT is a bug waiting for a window" fence: the
/// arm must be unsatisfiable ONLY while the sched is genuinely stuck, and satisfiable again the
/// instant ANY of `is_deadlocked_ignoring_jobs`'s vetoes holds. Each veto is exercised in turn,
/// because a snapshot taken at registration would pass the first assertion and fail every later one.
#[test]
fn w758_nursery_party_is_satisfiable_whenever_the_sched_can_still_move() {
    let sched = Arc::new(mk_sched(1));
    let party = crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&sched));
    sched.seed(vec![mk_fiber(0)]);
    assert!(
        party.satisfiable(),
        "a runnable fiber → the nursery can move"
    );
    let f = take_run(&sched);
    assert!(
        party.satisfiable(),
        "a running fiber → the nursery can move"
    );
    let chan = empty_core();
    sched.park(core_key(&chan), Arc::clone(&chan), f);
    assert!(
        !party.satisfiable(),
        "the only fiber is parked with nothing to feed it — the owner's wait CANNOT end"
    );
    // …and now each veto in turn puts it back to satisfiable. `runnable`:
    sched.inject(mk_pending_fiber(1), 0);
    assert!(party.satisfiable(), "runnable > 0");
    let g = take_run(&sched);
    assert!(party.satisfiable(), "running > 0");
    sched.park(core_key(&chan), Arc::clone(&chan), g);
    assert!(!party.satisfiable(), "back to stuck");
    // `inflight` (a blocking-pool call WILL come back):
    sched.inflight.fetch_add(1, Ordering::Relaxed);
    assert!(party.satisfiable(), "inflight > 0");
    sched.inflight.fetch_sub(1, Ordering::Relaxed);
    // A demoted fiber waiter is deliberately NOT a veto, and that asymmetry with `inflight` is the point: an
    // `inflight` fiber WILL come back from the pool, a demoted one comes back only if a sibling sends,
    // so an all-parked-or-demoted quiesce IS a deadlock. Assert it rather than describe it.
    let tok = sched
        .lock()
        .register_waiter(recv_waiter(&empty_core(), vec![]));
    assert!(
        !party.satisfiable(),
        "a demoted fiber is not a feeder — an unsatisfiable waiter must NOT veto the way `inflight` does"
    );
    sched.lock().unregister_waiter(tok);
    // `body_open` (eager nursery still injecting):
    sched.open_body(0);
    assert!(party.satisfiable(), "body_open");
    sched.close_body(0);
    // a cancelled scope mid-teardown:
    sched.trip_scope_cancel(0);
    assert!(party.satisfiable(), "cancelled scope awaiting drain");
    sched.cancel_drain(0);
    // every scope complete → nothing to wait for at all:
    let done = Arc::new(mk_sched(0));
    assert!(
        crate::vm::quiesce::PartyWait::Nursery(done).satisfiable(),
        "a sched with no incomplete scope is not stuck"
    );
    // `awaiting_builder` — needs its own fixture: the veto is defined only for a MULTI-scope sched
    // (`all_incomplete_awaiting_builder` returns false at `scopes.count() == 1`), i.e. exactly the
    // early-enlisted shape it exists for.
    let two = Arc::new(mk_sched(1));
    let s1 = two.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    let two_party = crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&two));
    two.seed(vec![mk_fiber(0)]);
    let h = take_run(&two);
    two.park(core_key(&chan), Arc::clone(&chan), h);
    assert!(
        !two_party.satisfiable(),
        "both scopes incomplete, all parked"
    );
    {
        let mut c = two.lock();
        c.scopes[0].awaiting_builder = true;
        c.scopes[s1].awaiting_builder = true;
    }
    assert!(
        two_party.satisfiable(),
        "an enlisted scope awaiting its builder has a LIVE feeder the counters cannot see"
    );
}

/// W7-58, the COUNT. A nursery owner is counted against the very same `live` the verdict already
/// used — `1 (main) + Σ outstanding` — so a stuck job PLUS a stuck nursery owner is exactly `live`
/// parties and the verdict fires. Before the fix the owner never registered, so `parties.len() == 1 <
/// live == 2` vetoed forever: that is W7-58's whole mechanism, and this is the direct fence for it.
///
/// The last leg is the fence in the OTHER direction (the one that faults live programs): drop the
/// job's party while its slot is still reserved and the count must veto again.
#[test]
fn w758_quiesced_counts_a_nursery_owner_against_live() {
    let state: Arc<crate::vm::quiesce::QuiesceState> = Arc::default();
    // A stuck Executor job: a parked fiber of a second, registered sched (TICKET-208: a job is a
    // fiber, not a party). It cannot move, so it adds nothing to `live`.
    let jobs = Arc::new(mk_sched(1));
    jobs.seed(vec![mk_fiber(0)]);
    let jf = take_run(&jobs);
    let jchan = empty_core();
    jobs.park(core_key(&jchan), Arc::clone(&jchan), jf);
    state.register_eager_body(&jobs);

    // A stuck nursery: its only fiber is parked with nothing to feed it.
    let sched = Arc::new(mk_sched(1));
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    let chan = empty_core();
    sched.park(core_key(&chan), Arc::clone(&chan), f);

    assert!(
        !state.quiesced(),
        "the owner is not registered yet — 0 parties < live 1, so the verdict must decline (this IS \
         the W7-58 hang)"
    );
    let owner = state.block(
        crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&sched)),
        crate::vm::block::WakeSet::default(),
    );
    assert!(
        state.quiesced(),
        "owner == live, unsatisfiable, and the job cannot move → the run really is stuck"
    );
    // The nursery becomes able to move → the owner's wait is satisfiable → veto.
    sched.inject(mk_pending_fiber(1), 0);
    assert!(
        !state.quiesced(),
        "a runnable fiber makes the owner's wait satisfiable, which must veto the verdict"
    );
    let g = take_run(&sched);
    sched.park(core_key(&chan), Arc::clone(&chan), g);
    assert!(state.quiesced(), "stuck again");
    // The other direction: fewer parties than `live` must always veto.
    jobs.inject(mk_pending_fiber(1), 0);
    assert!(
        !state.quiesced(),
        "1 party < live 2 — a runnable job is a live sender, which may yet send"
    );
    drop(owner);
}

/// W7-58 — the GATE. A worker shell must never register a nursery party: it is not in `live`, so
/// registering it would let `parties.len()` exceed `live`, which is the one error direction that
/// faults a live program (`quiesce`'s error-direction table).
///
/// Also pins the deliberate WIDENING versus `BlockCtx::judged`: a builder holding an early-enlisted
/// sched (`mn_enlist_sched.is_some()`, `BlockCtx::Builder`) is NOT judged, yet it is exactly
/// `main`, i.e. the `1 +` in `live` — so it MUST register.
#[test]
fn w758_only_a_thread_with_no_scheduler_under_it_registers_a_nursery_party() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let sched = Arc::new(mk_sched(0));
    assert!(
        vm.nursery_party_guard(&sched).is_some(),
        "top-level main owns its OS thread — it is the `1 +` in `live`"
    );
    vm.mn_enlist_sched = Some(Arc::clone(&sched));
    assert!(
        vm.nursery_party_guard(&sched).is_some(),
        "an early-enlisted builder is still main; a `BlockCtx::judged` gate would wrongly exclude it"
    );
    vm.mn_enlist_sched = None;
    vm.mn = Some(Arc::clone(&sched));
    assert!(
        vm.nursery_party_guard(&sched).is_none(),
        "a worker SHELL is not in `live` — registering it would let parties exceed live"
    );
}

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
    v := ch.recv()
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
        total = total + ch.recv()
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

/// B3.1: `shut` lives in the shared core, so a `from_wire`'d alias observes a shutdown done through
/// the original handle — `submit` on the alias then settles its Task with the byte-identical
/// refusal (TICKET-289: a refused submit is no longer a fault of the caller).
#[test]
fn executor_core_shut_is_shared_across_handles() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h1 = vm
        .heap
        .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
    let w = vm.to_wire(Value::obj(h1)).unwrap();
    let Some(h2) = vm.from_wire(w).as_obj() else {
        panic!("expected handle")
    };
    let sp = Span::RUNTIME;
    vm.executor_method(h1, "shutdown", &[], sp).unwrap();
    let ch = Arc::new(ChannelCore::default());
    let chv = vm.heap.alloc(Obj::Channel(Arc::clone(&ch)));
    vm.executor_method(h2, "_submit_settled", &[Value::nil(), Value::obj(chv)], sp)
        .expect("a refused submit returns Ok with its Task settled");
    assert!(
        ch.q.lock().unwrap().is_sealed(),
        "the refusal must seal the Task's channel"
    );
    // M3.2: the refusal is sealed as text, and the receive builds it into the `Err` value.
    let got = vm.channel_method(chv, "recv", &[], sp).unwrap();
    let sealed = vm.display(got);
    assert!(
        sealed.contains("submit on a shut-down Executor (it no longer accepts work)"),
        "sealed with: {sealed}"
    );
}

/// B3.1: `display` of a `Shared` box renders its contents through `display_wire` (a boxed `str`
/// renders from its owned bytes — B3.3a), since `display` is `&self` and can't `from_wire`.
#[test]
fn display_shared_renders_contents() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("hi".into()));
    let boxed = vm.to_wire(Value::obj(s)).unwrap();
    let core = vm.new_shared_core(boxed);
    let sh = vm.heap.alloc(Obj::Shared(core));
    // The payload sits INSIDE `Shared(…)`, i.e. nested — so it renders as its `repr` (W7-25),
    // matching `print(s.get())` on the same box.
    assert_eq!(vm.display(Value::obj(sh)), "Shared('hi')");
}
