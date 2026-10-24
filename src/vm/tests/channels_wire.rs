//! VM tests: WireValue airlock, bounded channels, `wait` send arms, pmap.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- B3.0: WireValue airlock (to_wire / from_wire) -----

/// A round-trip through the wire form, into the *same* heap, must be value-equal to the original
/// over a deeply-nested sendable mix (scalars, str, list, tuple, map, set, struct, enum). This is
/// the airlock's correctness invariant (B3.0): serialize then reconstruct loses nothing.
#[test]
fn wire_roundtrip_preserves_value_equality() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("s".into()));
    let tup = vm
        .heap
        .alloc(Obj::Tuple(vec![Value::bool(true), Value::nil()]));
    let st = vm.heap.alloc(Obj::Struct {
        tid: TID_NONE,
        fields: crate::vm::heap::Fields::from_vec(vec![Value::int(1), Value::obj(s)]),
    });
    let en = vm.heap.alloc(Obj::Enum {
        // `empty_program` has no variant table, so use the unregistered sentinel (the enum analogue
        // of the struct's `TID_NONE` above) — it round-trips ("?"→VID_NONE→"?") value-equal.
        variant_id: crate::base::op::VID_NONE,
        payload: vec![Value::int(9)],
    });
    let m = MapData::default();
    m.push(&vm.heap, 10, Value::int(1), Value::int(100));
    m.push(&vm.heap, 20, Value::obj(s), Value::int(200));
    let map = vm.heap.alloc(Obj::Map(m));
    let set = SetData::default();
    set.push(&vm.heap, 5, Value::int(1));
    set.push(&vm.heap, 6, Value::int(2));
    let setobj = vm.heap.alloc(Obj::Set(set));
    let list = vm.heap.alloc(Obj::List(
        vec![
            Value::int(1),
            Value::obj(s),
            Value::obj(tup),
            Value::obj(st),
            Value::obj(en),
            Value::obj(map),
            Value::obj(setobj),
        ]
        .into(),
    ));
    let v = Value::obj(list);

    let w = vm
        .to_wire(v)
        .expect("nested sendable value should serialize");
    let wired = vm.from_wire(w);
    assert!(
        vm.values_equal(v, wired),
        "wire round-trip changed the value"
    );
    // Data is reconstructed into a *fresh* handle (deep copy, not aliasing the original).
    assert_ne!(
        v, wired,
        "round-tripped data should be a distinct heap object"
    );
}

/// `Map`/`Set` cross the wire carrying their **cached hashes** and **insertion order** unchanged —
/// `from_wire` rebuilds via `push(hash, …)`, never re-hashing. Pins byte-identical reconstruction
/// (the iteration order + index a later `print`/lookup observes) even when two keys collide.
#[test]
fn wire_preserves_map_hashes_and_order() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let m = MapData::default();
    m.push(&vm.heap, 42, Value::int(1), Value::int(10)); // collides with the third entry on hash 42
    m.push(&vm.heap, 7, Value::int(2), Value::int(20));
    m.push(&vm.heap, 42, Value::int(3), Value::int(30));
    let map = Value::obj(vm.heap.alloc(Obj::Map(m)));

    let w = vm.to_wire(map).expect("map should serialize");
    let wired = vm.from_wire(w);
    let Some(h) = wired.as_obj() else {
        panic!("expected heap obj")
    };
    let Obj::Map(rebuilt) = vm.heap.get(h) else {
        panic!("expected map")
    };
    let hashes: Vec<u64> = rebuilt.entries.iter().map(|(hash, ..)| hash).collect();
    assert_eq!(
        hashes,
        vec![42, 7, 42],
        "cached hashes / order must survive the round-trip"
    );
    // The index must reflect the cached hashes (collision bucket points at positions 0 and 2).
    assert_eq!(rebuilt.candidates(42), &[0, 2]);
    assert_eq!(rebuilt.candidates(7), &[1]);
}

/// By-reference callables (`Func`/`Closure`/`Module`/`Native`) cross the airlock **by handle** —
/// `to_wire`→`from_wire` returns the *same* `GcRef` (matching the old `deep_clone` by-handle arm).
/// (`Str` no longer qualifies — it crosses by value as of B3.3a; see `wire_crosses_str_by_value`.)
#[test]
fn wire_passes_by_reference_objects_as_same_handle() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let m = vm.heap.alloc(Obj::Module(Box::new(ModuleData {
        name: "m".into(),
        slots: Vec::new(),
        index: Default::default(),
    })));
    let v = Value::obj(m);
    let w = vm.to_wire(v).expect("by-ref object should serialize");
    assert_eq!(
        vm.from_wire(w),
        v,
        "by-reference object must round-trip to the same handle"
    );
}

/// B3.3a: a `str` crosses the airlock **by value** (owned bytes), not as a by-reference
/// `Handle(GcRef)`: `from_wire` allocates a *fresh* heap `str` that is value-equal but a distinct
/// handle. This is what lets a `str` cross a real OS-thread heap boundary at B3.3 (a `GcRef` would
/// be a meaningless slot index there). Parity-safe: `str` is immutable + value-compared and Chezzi
/// has no identity operator, so a fresh handle is observationally identical to the shared one.
#[test]
fn wire_crosses_str_by_value() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("imm".into()));
    let v = Value::obj(s);
    let w = vm.to_wire(v).expect("str should serialize");
    let wired = vm.from_wire(w);
    assert_ne!(
        wired, v,
        "a crossed str gets a fresh handle (by value, not by handle)"
    );
    assert!(
        vm.values_equal(v, wired),
        "the fresh str must be value-equal to the original"
    );
}

/// B3.3a: a `str` used as a **map key** crosses by value and stays findable — the cached hash is
/// carried through and `from_wire` rebuilds the key as a fresh handle whose content hashes
/// identically (hashing keys on bytes, not `GcRef`), so the reconstructed map's bucket index is
/// preserved. Guards against a future change that hashed/compared str keys by handle identity.
#[test]
fn wire_str_map_key_survives_roundtrip() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let key = vm.heap.alloc(Obj::Str("k".into()));
    let m = MapData::default();
    let h = vm.scalar_hash(Value::obj(key));
    m.push(&vm.heap, h, Value::obj(key), Value::int(42));
    let map = Value::obj(vm.heap.alloc(Obj::Map(m)));

    let w = vm
        .to_wire(map)
        .expect("map with a str key should serialize");
    let wired = vm.from_wire(w);
    let Some(mh) = wired.as_obj() else {
        panic!("expected map handle")
    };
    let Obj::Map(rebuilt) = vm.heap.get(mh) else {
        panic!("expected map")
    };
    // Same single entry: a fresh str key, value-equal, same cached hash → same bucket.
    assert_eq!(rebuilt.entries.len(), 1);
    let (rh, rk, rv) = rebuilt.entries.get(0);
    assert_eq!(rh, h, "cached hash preserved");
    assert_eq!(rv, Value::int(42));
    assert_eq!(
        rebuilt.candidates(h),
        &[0],
        "index bucket points at the rebuilt key"
    );
    assert!(
        vm.values_equal(rk, Value::obj(key)),
        "rebuilt str key is value-equal"
    );
}

/// B3.1: `Channel`/`Shared`/`Executor` cross the airlock as their shared `Arc<…Core>`. The
/// round-trip yields a *fresh* `GcRef` (a new handle obj) wrapping the **same** core — identity is
/// at the `Arc`, not the handle, so two tasks still reach one mailbox/box/queue.
#[test]
fn wire_shares_core_across_a_fresh_handle() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let ch = vm
        .heap
        .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
    let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore::default())));
    let rw = vm
        .heap
        .alloc(Obj::RwShared(Arc::new(RwSharedCore::default())));
    let ex = vm
        .heap
        .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
    for h in [ch, sh, rw, ex] {
        let v = Value::obj(h);
        let w = vm.to_wire(v).expect("core handle should serialize");
        let wired = vm.from_wire(w);
        assert_ne!(wired, v, "a crossed core gets a fresh handle (new GcRef)");
        // Same underlying core: both handles carry the same `Obj::identity` core key.
        let (a, b) = (
            vm.heap.get(h).identity(),
            vm.heap.get(wired.as_obj().unwrap()).identity(),
        );
        let same = matches!(a, Identity::Core(_)) && a == b;
        assert!(same, "the fresh handle must point at the SAME shared core");
    }
}

/// B3.1: two handles produced from one core (the `from_wire` airlock copy) reach the SAME mailbox
/// — `send` on one handle is `recv`-able through the other. Proves the `Arc` core is shared, not
/// duplicated, across the wire.
#[test]
fn channel_core_shared_across_handles() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h1 = vm
        .heap
        .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
    // Cross the airlock → a second handle onto the same core.
    let w = vm.to_wire(Value::obj(h1)).unwrap();
    let Some(h2) = vm.from_wire(w).as_obj() else {
        panic!("expected handle")
    };
    let sp = Span::RUNTIME;
    vm.channel_method(h1, "send", &[Value::int(7)], sp).unwrap();
    // recv through the OTHER handle sees the message.
    assert_eq!(
        vm.channel_method(h2, "recv", &[], sp).unwrap(),
        Value::int(7)
    );
}

/// Cross-thread channel delivery through one shared `ChannelCore`: a value `send`-queued on the
/// main thread is `recv`-popped on a **separate OS thread** running its own `Vm` — proving both
/// the `Arc<ChannelCore>` airlock is genuinely shared across threads AND `Vm: Send` (it moves into
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
    let handle = std::thread::spawn(move || worker.channel_method(wh, "recv", &[], sp).unwrap());
    assert_eq!(handle.join().unwrap(), Value::int(42));
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
    assert!(e.contains("send on a full channel"), "serial: {e}");
    let ep = run_capture(src).expect_err("M:N should fault").message;
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
        e.contains("full send()"),
        "serial diagnostic must name the send stall: {e}"
    );
    let ep = run_capture(src).expect_err("M:N should fault").message;
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

/// A send-arm on a CLOSED channel is SELECTED (ready) and FAULTS `send on a closed channel` — NOT
/// skipped like a closed recv arm — even with an `else` present (a closed send arm is always ready, so
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
                e.message.contains("send on a full channel"),
                "unexpected fault: {}",
                e.message
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

/// TICKET-211 (W1): a nursery opened inside a spawned task, after the outer body has closed, runs on
/// ~2 cores however many workers exist (measured: 8 tasks of 10M iterations, T=8, 199% CPU, 2.6 s).
/// An Atomic in-flight probe cannot see it (fibers interleave on 2 runners, peak still reads 8), so
/// the test compares wall time of the same 8 spawns flat vs nested in one program.
#[test]
fn nested_nursery_in_spawned_task_uses_all_workers() {
    if crate::vm::rerun_in_child(
        module_path!(),
        "nested_nursery_in_spawned_task_uses_all_workers",
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
    crate::vm::set_worker_count(8);
    let src = "
import std.time
fn burn():
    i := 0
    while i < 3000000:
        i += 1
fn fan():
    parallel:
        for _ in 0..8:
            spawn burn()
t0 := time.monotonic()
parallel:
    for _ in 0..8:
        spawn burn()
flat := time.monotonic() - t0
t1 := time.monotonic()
parallel:
    spawn fan()
nested := time.monotonic() - t1
print(nested < flat * 1.6 + 0.05)
";
    let entry = write_temp_chz("w1_nested_nursery", src);
    let (out, _err, res, _code) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    res.expect("program runs");
    assert_eq!(
        out, "true\n",
        "8 spawns nested in a spawned task must run within 1.6x of the same 8 spawns flat"
    );
}

/// TICKET-211 (W1) — the grid: every nesting shape {flat, nested in a spawned task, two deep,
/// nested in an Executor job} x outer body {open, closed} x T {2, 4, 8} must reach exactly T runner
/// threads. A cell may stay below T only when the `NestedDrainerSlot` budget refused a start (another
/// test holds slots), and never exceed T.
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

/// `pmap_limited`'s token bucket actually BOUNDS in-flight tasks: an Atomic max-in-flight probe never
/// exceeds `limit` — the semaphore caps concurrent f-execution.
#[test]
fn pmap_limited_bounds_in_flight_both_engines() {
    // A nested named fn (multi-statement) captures the two Atomics by-ref; both cross the airlock as
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
    a := ch.recv()
    b := ch.recv()
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
/// buffers) and the roots `module_objs`/`module_faulted`/`executors` (each a `GcRef` to the
/// fiber's own copy in the run's one heap). A task switch (`swap_task`) round-trips all of them
/// for a context with `owns_side`. A task owns its output stream, so `swap_task` moves
/// `out`/`stderr` for every task context, `owns_side` or not; the `owns_side: false` side state is
/// [`swap_ctx_leaves_side_state_on_the_shell_without_owns_side`].
#[test]
fn swap_task_round_trips_output_and_owns_side_state() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"host-out");
    vm.stderr.extend_from_slice(b"host-err");
    let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
    let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
    vm.module_objs = vec![host_mod];
    vm.module_faulted = vec![true];
    vm.executors = vec![host_exec];
    // M19 Phase 3 — the intern cache holds the context's own strings; it must round-trip.
    let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
    vm.str_intern.insert(0x10, host_str);
    // TICKET-215 — the fn-value memo the same way.
    let host_fn = vm.heap.alloc(Obj::Str("host-fn".into()));
    vm.fn_values.insert(FnKey::Proto(0), host_fn);

    let fib_mod = vm.heap.alloc(Obj::Str("fiber-mod".into()));
    let fib_exec = vm.heap.alloc(Obj::Str("fiber-exec".into()));
    let fib_str = vm.heap.alloc(Obj::Str("fiber-str".into()));
    let fib_fn = vm.heap.alloc(Obj::Str("fiber-fn".into()));
    let mut ctx = FiberCtx {
        owns_side: true,
        out: b"fiber-out".to_vec(),
        stderr: b"fiber-err".to_vec(),
        module_objs: vec![fib_mod],
        module_faulted: vec![false],
        executors: vec![fib_exec],
        str_intern: fxhash::FxHashMap::from_iter([(0x20usize, fib_str)]),
        fn_values: fxhash::FxHashMap::from_iter([(FnKey::Proto(1), fib_fn)]),
        ..FiberCtx::default()
    };

    // Schedule in: the fiber's side state becomes live; the shell's parks into the ctx.
    vm.swap_task(&mut ctx);
    assert_eq!(vm.out, b"fiber-out");
    assert_eq!(vm.stderr, b"fiber-err");
    assert_eq!(vm.module_objs, vec![fib_mod]);
    assert_eq!(vm.module_faulted, vec![false]);
    assert_eq!(vm.executors, vec![fib_exec]);
    assert_eq!(vm.str_intern.get(&0x20), Some(&fib_str));
    assert_eq!(vm.str_intern.get(&0x10), None);
    assert_eq!(ctx.out, b"host-out");
    assert_eq!(ctx.module_objs, vec![host_mod]);
    assert_eq!(ctx.str_intern.get(&0x10), Some(&host_str));
    assert_eq!(vm.fn_values.get(&FnKey::Proto(1)), Some(&fib_fn));
    assert_eq!(vm.fn_values.get(&FnKey::Proto(0)), None);
    assert_eq!(ctx.fn_values.get(&FnKey::Proto(0)), Some(&host_fn));

    // Park out: the shell's side state is restored; the fiber keeps its own.
    vm.swap_task(&mut ctx);
    assert_eq!(vm.out, b"host-out");
    assert_eq!(vm.stderr, b"host-err");
    assert_eq!(vm.module_objs, vec![host_mod]);
    assert_eq!(vm.module_faulted, vec![true]);
    assert_eq!(vm.executors, vec![host_exec]);
    assert_eq!(vm.str_intern.get(&0x10), Some(&host_str));
    assert_eq!(ctx.out, b"fiber-out");
    assert_eq!(ctx.module_objs, vec![fib_mod]);
    assert_eq!(ctx.str_intern.get(&0x20), Some(&fib_str));
    assert_eq!(vm.fn_values.get(&FnKey::Proto(0)), Some(&host_fn));
    assert_eq!(ctx.fn_values.get(&FnKey::Proto(1)), Some(&fib_fn));
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

/// D2b / Task 1 companion to [`swap_ctx_leaves_heap_untouched_for_cooperative_fiber`]: `swap_ctx`
/// never moves `out` (only a task switch, `swap_task`, does), for any context. But
/// `module_objs`/`module_faulted` swap UNCONDITIONALLY, `owns_side` or not (Task 1: a child carries
/// its own deep-copied module view). So swapping in a child with an empty view parks the host's REAL
/// modules into the ctx. The `owns_side: false` side state is
/// [`swap_ctx_leaves_side_state_on_the_shell_without_owns_side`].
#[test]
fn swap_ctx_moves_module_objs_but_not_out_without_owns_side() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"host-out");
    let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
    vm.module_objs = vec![host_mod];
    let mut ctx = FiberCtx::default();
    vm.swap_ctx(&mut ctx);
    // `out` stays on the shell: `swap_ctx` does not move the output stream.
    assert_eq!(vm.out, b"host-out");
    assert!(
        ctx.out.is_empty(),
        "swap_ctx must not move the output stream"
    );
    // Task 1 — module_objs swaps: the host's real modules parked into the ctx, the shell now holds the
    // child's (empty) view.
    assert!(vm.module_objs.is_empty());
    assert_eq!(ctx.module_objs, vec![host_mod]);
}

/// M1.6 — a context with `owns_side: false` owns no side state, so `swap_ctx`
/// leaves the shell's `executors`, `str_intern`, `fn_values`, `pending_connect` and `poll_*`
/// fields on the shell, and the context's own fields stay empty.
#[test]
fn swap_ctx_leaves_side_state_on_the_shell_without_owns_side() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).expect("connect");
    let mut vm = Vm::new(Arc::new(empty_program()));
    let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
    let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
    let host_fn = vm.heap.alloc(Obj::Str("host-fn".into()));
    vm.executors = vec![host_exec];
    vm.str_intern.insert(0x10, host_str);
    vm.fn_values.insert(FnKey::Proto(0), host_fn);
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
        assert_eq!(vm.fn_values.get(&FnKey::Proto(0)), Some(&host_fn));
        assert_eq!(vm.pending_connect.as_ref().map(|c| c.key), Some(7));
        assert!(vm.poll_timed_out);
        assert_eq!(vm.poll_deadline, Some(deadline));
        assert_eq!(vm.poll_partial, Some(2));
        assert_eq!(vm.poll_written, Some(3));
        assert!(ctx.executors.is_empty() && ctx.str_intern.is_empty() && ctx.fn_values.is_empty());
        assert!(ctx.pending_connect.is_none());
        assert!(!ctx.poll_timed_out.load(Ordering::Relaxed));
        assert!(ctx.poll_deadline.is_none() && ctx.poll_partial.is_none());
        assert!(ctx.poll_written.is_none());
    }
}
