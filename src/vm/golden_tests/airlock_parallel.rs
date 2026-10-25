//! Golden tests: airlock-crossed closures and module globals, `parallel:` programs, deadlock and
//! cancel trips.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

#[test]
fn airlock_crossed_closure_reads_module_global_live_same_task() {
    // TICKET-041(b), historical. Before the fix, a closure's snapshot of its module-global reads
    // was frozen at the FIRST crossing and never refreshed — even on a same-task round trip
    // through `Channel.send`/`.recv()` where no task boundary is ever crossed. Calling the SAME
    // closure value directly (no crossing) and via the channel round trip must answer identically:
    // both must observe the post-crossing write `n = 42`, since the write happens strictly before
    // either call.
    let src = r#"
n := 1
fn mk() -> fn(int) -> int:
    fn r(x: int) -> int:
        return x * n
    return r
ch := Channel[fn(int) -> int](2)
ch.send(mk())
k := ch.recv()
n = 42
print("direct : {k(3)}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "direct : 126\n",
        "a closure round-tripped through a same-task channel must read the module global LIVE, \
         not a value frozen at the first crossing: {out:?}"
    );
}

#[test]
fn airlock_nested_closure_created_after_the_tasks_own_write_reads_it() {
    // TICKET-051, Shape B: the receiving task writes `n = 100`, then builds the nested closure
    // `inner`. Both reads see the write (M3.1: one `n`). Go (go1.27.0, `m31go/rust`) and CPython:
    // `100` for both.
    let src = r#"
n := 1
res := Channel[int](4)
fn mk() -> fn() -> fn() -> int:
    fn outer() -> fn() -> int:
        fn inner() -> int:
            return n
        return inner
    return outer
ch := Channel[fn() -> fn() -> int](1)
parallel:
    spawn:
        o := ch.recv()
        _ := recover:
            n = 100
        d := o()
        res.send(d())
        res.send(n)
    ch.send(mk())
print("nested closure sees : {res.recv()}")
print("plain global read   : {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "nested closure sees : 100\nplain global read   : 100\n",
        "a closure created after the task's own write must read it \
         like any other read: {out:?}"
    );
}

#[test]
fn airlock_forwarded_closure_reads_the_forwarders_write() {
    // TICKET-137: a closure carries captures only. A -> B -> C: B writes `n = 999` and forwards the
    // closure; C runs it after the receive, so it reads the run's one `n` (M3.1). Go (go1.27.0,
    // `m31go/rust`) and CPython: `2997`.
    let src = r#"
n := 1
fn mk() -> fn(int) -> int:
    fn r(x: int) -> int:
        return x * n
    return r

ch1 := Channel[fn(int) -> int](1)
ch2 := Channel[fn(int) -> int](1)
res := Channel[int](1)

parallel:
    spawn: ch1.send(mk())
    spawn:
        k := ch1.recv()
        _ := recover:
            n = 999
        ch2.send(k)
    spawn:
        k2 := ch2.recv()
        res.send(k2(3))

print("result: {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "result: 2997\n",
        "a forwarded closure must read the run's one copy of the module global, with \
         the forwarder's write: {out:?}"
    );
}

#[test]
fn airlock_receiving_tasks_own_write_beats_an_arriving_closures_global() {
    // TICKET-051: a closure arriving from another task must not clobber the receiving task's own
    // write to the same global. Task A writes `n = 5` before it receives a closure over `n`. Go
    // (go1.27.0, `m31go/rust`) and CPython: `5` and `5`.
    let src = r#"
n := 1
ch := Channel[fn() -> int](1)
res := Channel[int](2)

fn mk() -> fn() -> int:
    fn r() -> int:
        return n
    return r

parallel:
    spawn:
        _ := recover:
            n = 5
        k := ch.recv()
        res.send(k())
        res.send(n)
    spawn:
        ch.send(mk())

print("closure sees : {res.recv()}")
print("task read    : {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 5\ntask read    : 5\n",
        "the receiving task's own write to a module global must beat an arriving \
         closure's stale value for that same global: {out:?}"
    );
}

#[test]
fn airlock_arriving_closure_loses_to_the_receiving_views_own_assignment() {
    // TICKET-051: same shape as
    // `airlock_receiving_tasks_own_write_beats_an_arriving_closures_global` with a top-level `n =
    // 7` before the nursery. The crossing carries no globals, so the receiver's own later write
    // wins. Go (go1.27.0, `m31go/rust`) and CPython: `5` and `5`.
    let src = r#"
n := 1
n = 7
ch := Channel[fn() -> int](1)
res := Channel[int](2)

fn mk() -> fn() -> int:
    fn r() -> int:
        return n
    return r

parallel:
    spawn:
        _ := recover:
            n = 5
        k := ch.recv()
        res.send(k())
        res.send(n)
    spawn:
        ch.send(mk())

print("closure sees : {res.recv()}")
print("task read    : {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 5\ntask read    : 5\n",
        "the receiving task's own write must win even when the sender inherited a \
         different value for the same global: {out:?}"
    );
}

#[test]
fn airlock_arriving_closure_does_not_clobber_the_receivers_in_place_push() {
    // TICKET-051: an arriving closure must not replace the receiving task's global, which it has
    // mutated in place (`xs.push(9)`). M3.1: the push lands in the run's one `xs`. Go (go1.27.0,
    // `m31go/rust`): `[1, 9] len 2`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

xs := [1]
ch := Channel[fn() -> int](1)
res := Channel[str](1)

fn mk() -> fn() -> int:
    fn r() -> int:
        return xs.len()
    return r

parallel:
    spawn:
        _ := recover: xs.push(9)
        k := ch.recv()
        res.send("{xs} len {k()}")
    spawn:
        ch.send(mk())

print(res.recv())
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[1, 9] len 2\n",
        "an arriving closure's global that no ancestor ever assigned must not replace the \
         receiver's in-place push: {out:?}"
    );
}

#[test]
fn airlock_closure_from_a_sender_that_only_inherited_a_global_reads_the_live_global() {
    // TICKET-051: main writes `n = 7` between the two spawns; the received closure runs after the
    // receive, so it reads the write (M3.1: one `n`). Go (go1.27.0, `m31go/rust`) and CPython: `7`.
    let src = r#"
import std.concurrency

n := 1
ch := Channel[fn() -> int](1)
res := Channel[int](1)

fn mk() -> fn() -> int:
    fn r() -> int:
        return n
    return r

parallel:
    spawn: res.send(ch.recv()())
    n = 7
    spawn: ch.send(mk())

print("closure sees : {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 7\n",
        "a received closure must read the run's one copy of the global, with the \
         root's write: {out:?}"
    );
}

#[test]
fn airlock_forwarded_closure_without_its_own_write_reads_the_first_senders_write() {
    // TICKET-137: task A writes `n = 999` and sends a closure over `n`; B forwards it; C reads the
    // run's one `n` (M3.1). Go (go1.27.0, `m31go/rust`) and CPython: `2997`.
    let src = r#"
n := 1
fn mk() -> fn(int) -> int:
    fn r(x: int) -> int:
        return x * n
    return r

ch1 := Channel[fn(int) -> int](1)
ch2 := Channel[fn(int) -> int](1)
res := Channel[int](1)

parallel:
    spawn:
        _ := recover:
            n = 999
        ch1.send(mk())
    spawn:
        k := ch1.recv()
        ch2.send(k)
    spawn:
        k2 := ch2.recv()
        res.send(k2(3))

print("result: {res.recv()}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "result: 2997\n",
        "a forwarded closure must read the run's one copy of the global, with the \
         first sender's write: {out:?}"
    );
}

#[test]
fn airlock_same_task_closure_round_trip_mutates_module_list_in_place() {
    // TICKET-041(b), data-loss variant. An in-place write to a module-global container from a
    // same-task channel-round-tripped closure must land, matching CPython's measured
    // `module xs: [1, 9]` (2026-09-03). Before the fix the frozen global snapshot made the write
    // vanish (`module xs: [1]`), because the closure mutated its OWN snapshot copy of `xs` instead
    // of the live module slot.
    let src = r#"
xs := [1]
fn mk() -> fn() -> None:
    fn f():
        xs.push(9)
    return f
ch := Channel[fn() -> None](2)
ch.send(mk())
ch.recv()()
print("module xs: {xs}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "module xs: [1, 9]\n",
        "a same-task channel round trip must mutate the module global in place: {out:?}"
    );
}

#[test]
fn airlock_closure_calling_a_helper_that_reads_a_global_reads_the_live_global() {
    // TICKET-097 defect A: a received closure reads `n` live whether it names the global or calls a
    // helper that reads it. M3.1: one `n` for the run. Go (go1.27.0, `m31go/rust`) and CPython:
    // `100` on every line.
    let src = r#"
n := 1

fn helper() -> int:
    return n

c1 := Channel[fn() -> int](1)
c2 := Channel[fn() -> int](1)

fn producer():
    _ := recover:
        n = 100
    c1.send(fn() -> int: helper())
    c2.send(fn() -> int: n)

fn main():
    parallel:
        spawn producer()
    indirect := c1.recv()
    print("indirect before: {indirect()}")
    direct := c2.recv()
    print("indirect after : {indirect()}")
    print("direct         : {direct()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "indirect before: 100\nindirect after : 100\ndirect         : 100\n",
        "a received closure, direct or through a helper call, must read the run's \
         one copy of the module global: {out:?}"
    );
}

#[test]
fn airlock_a_called_helper_that_writes_the_global_keeps_it_a_late_load() {
    // TICKET-097 guard: a global the closure tree WRITES is a live read, even behind a called
    // helper. M3.1: the run has one `n`, so the sender's `n = 100` is what `bump()` reads. Go
    // (go1.27.0, `m31go/rust`): `101`.
    let src = r#"
n := 1

fn bump() -> int:
    n = n + 1
    return n

c := Channel[fn() -> int](1)

fn producer():
    _ := recover:
        n = 100
    c.send(fn() -> int: bump())

fn main():
    parallel:
        spawn producer()
    f := c.recv()
    print("late load: {f()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "late load: 101\n",
        "a global the called helper WRITES must be read live from the run's one copy: \
         {out:?}"
    );
}

#[test]
fn airlock_a_closure_calling_a_nested_fn_reads_the_live_global() {
    // TICKET-137: a nested fn is a CAPTURE of the crossing closure; its global read is a live read
    // of the run's one `n` (M3.1), so it sees the sender's `n = 100`. Go (go1.27.0, `m31go/rust`)
    // and CPython: `100`.
    let src = r#"
n := 1
c := Channel[fn() -> int](1)

fn producer():
    fn helper() -> int:
        return n
    _ := recover:
        n = 100
    c.send(fn() -> int: helper())

fn main():
    parallel:
        spawn producer()
    print("nested-fn indirect: {c.recv()()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "nested-fn indirect: 100\n",
        "a closure calling a NESTED fn must read the run's one copy of the global: \
         {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_global_the_sender_mutated_in_place_reads_the_live_global() {
    // TICKET-097 defect B: the sender's `xs = [1, 2]` and `ys.push(2)` land in the run's one copy
    // (M3.1). Go (go1.27.0, `m31go/rust`) and CPython: `2` twice.
    let src = r#"
xs := [1]
ys := [1]
c := Channel[fn() -> int](1)
c2 := Channel[fn() -> int](1)

fn producer():
    _ := recover:
        xs = [1, 2]
    _ := recover: ys.push(2)
    c.send(fn() -> int: xs.len())
    c2.send(fn() -> int: ys.len())

fn main():
    parallel:
        spawn producer()
    f := c.recv()
    g := c2.recv()
    print("assigned global via closure : {f()}")
    print("in-place global via closure : {g()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "assigned global via closure : 2\nin-place global via closure : 2\n",
        "a received closure must read the run's one copy of an assigned or \
         in-place-mutated global: {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_global_the_sender_mutated_by_index_or_field_assignment_reads_the_live_global()
 {
    // TICKET-097 defect B: the sender's `zs[0] = 9` and `p.v = 9` land in the run's one copy
    // (M3.1). Go (go1.27.0, `m31go/rust`) and CPython: `9` and `9`.
    let src = r#"
zs := [1]
struct P:
    v: int
p := P(1)
c := Channel[fn() -> int](1)
c2 := Channel[fn() -> int](1)

fn producer():
    _ := recover:
        zs[0] = 9
    _ := recover:
        p.v = 9
    c.send(fn() -> int: zs[0])
    c2.send(fn() -> int: p.v)

fn main():
    parallel:
        spawn producer()
    f := c.recv()
    g := c2.recv()
    print("index-assigned global via closure : {f()}")
    print("field-assigned global via closure : {g()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "index-assigned global via closure : 9\nfield-assigned global via closure : 9\n",
        "a received closure must read the run's one copy of an index- or \
         field-assigned global: {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_global_a_user_struct_method_mutated_reads_the_live_global() {
    // TICKET-097/105: `g.bump()` twice in the sender changes the run's one `g` (M3.1). Go
    // (go1.27.0, `m31go/rust`) and CPython: `3`.
    let src = r#"
struct C:
    n: int
    fn bump(self):
        self.n = self.n + 1

g := C(1)
c := Channel[fn() -> int](1)

fn producer():
    _ := recover: g.bump()
    _ := recover: g.bump()
    c.send(fn() -> int: g.n)

fn main():
    parallel:
        spawn producer()
    print("user-method mutation via closure: {c.recv()()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "user-method mutation via closure: 3\n",
        "a received closure must read the run's one copy of a global the sender \
         mutated through a user method: {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_global_main_mutated_through_a_local_alias_reads_the_live_global() {
    // TICKET-105 (W12-6) G1: `main` pushes through a local alias of `g` before the send; one `g`
    // for the run (M3.1). Go (go1.27.0, `m31go/rust`) and CPython: `[1, 2]`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

g := [1]

fn main():
    c := Channel[fn() -> str](1)
    done := Channel[str](1)
    parallel:
        spawn:
            f := c.recv()
            done.send(f())
        xs := g
        xs.push(2)
        c.send(fn() -> str: "{g}")
    print("main-as-sender alias: {done.recv()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "main-as-sender alias: [1, 2]\n",
        "a received closure must read the run's one copy, with the root's alias \
         write: {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_global_the_sender_mutated_through_a_struct_alias_reads_the_live_global() {
    // TICKET-105 (W12-6) G2: `h := g; h.n = 5` writes the run's one `g` (M3.1). Go (go1.27.0,
    // `m31go/rust`) and CPython: `5`.
    let src = r#"
import std.concurrency

struct H:
    n: int

g := H(1)
c := Channel[fn() -> int](1)

fn producer():
    h := g
    _ := recover:
        h.n = 5
    c.send(fn() -> int: g.n)

fn main():
    parallel:
        spawn producer()
    print("struct alias: {c.recv()()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "struct alias: 5\n",
        "a received closure must read the run's one copy of a struct global the \
         sender wrote through an alias: {out:?}"
    );
}

#[test]
fn airlock_alias_write_reaches_a_grandchild_senders_closure() {
    // TICKET-105 (W12-6) G3: the alias write happens in `producer`, an ancestor of the closure's
    // sender `child`. M3.1: one `g` for the run, so `main` reads the write. Go (go1.27.0,
    // `m31go/rust`) and CPython: `[1, 2]`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

g := [1]
c := Channel[fn() -> str](1)

fn child():
    c.send(fn() -> str: "{g}")

fn producer():
    xs := g
    _ := recover: xs.push(2)
    parallel:
        spawn child()

fn main():
    parallel:
        spawn producer()
    print("grandchild alias: {c.recv()()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "grandchild alias: [1, 2]\n",
        "a received closure must read the run's one copy, with an ancestor's alias \
         write: {out:?}"
    );
}

#[test]
fn airlock_closure_over_an_aliased_element_reads_the_live_global() {
    // TICKET-105 (W12-6) G4: `inner` is a CAPTURE (it crosses as the sender's `[1, 2]`) and `gl[0]`
    // is the run's one global (M3.1), which the push changed too. Go (go1.27.0, `m31go/rust`) and
    // CPython: `[1, 2] [1, 2]`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn main():
    c := Channel[fn() -> str](1)
    done := Channel[str](1)
    inner := gl[0]
    parallel:
        spawn:
            f := c.recv()
            done.send(f())
        inner.push(2)
        c.send(fn() -> str: "{inner} {gl[0]}")
    print("sender-push alias: {done.recv()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "sender-push alias: [1, 2] [1, 2]\n",
        "a capture crosses as the sender's value; a global reads the run's one copy: \
         {out:?}"
    );
}

#[test]
fn airlock_unchanged_aliased_global_does_not_clobber_the_receivers_push() {
    // TICKET-105 (W12-6), G5 (negative control): `a := xs; n := a.len()` never mutates the alias,
    // so nothing clobbers the receiver's in-place push, which lands in the run's one `xs` (M3.1).
    // Go (go1.27.0, `m31go/rust`): `[[1, 9], [1, 9]] len 4`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

inner := [1]
xs := [inner, inner]
ch := Channel[fn() -> int](1)
res := Channel[str](1)

fn main():
    parallel:
        spawn:
            _ := recover: xs[0].push(9)
            k := ch.recv()
            res.send("{xs} len {k()}")
        a := xs
        n := a.len()
        ch.send(fn() -> int: xs.len() + n)
    print(res.recv())
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[[1, 9], [1, 9]] len 4\n",
        "an unread alias must not falsely register as changed and clobber the receiver's own \
         in-place push: {out:?}"
    );
}

#[test]
fn airlock_closure_over_a_captured_alias_pushed_by_the_receiver_faults() {
    // TICKET-105, G6. The push through `inner` runs in the RECEIVER's captured closure body,
    // AFTER the send — the sender's `gl` never changes, so this used to be a pure W12-5 identity
    // residual (TICKET-111): the receiver's push landed on the adopted alias and both reads
    // observed it. TICKET-169 (D4 layer C, Decisions item 3): `work`'s capture of `inner` crossed
    // the airlock with the closure and is marked as a copy, so the push now FAULTS instead.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn main():
    c := Channel[fn() -> str](1)
    done := Channel[str](1)
    inner := gl[0]
    fn work() -> str:
        res := recover:
            inner.push(2)
        match res:
            ?_: return "{inner} {gl[0]}"
            !_: return "fault"
    parallel:
        spawn:
            f := c.recv()
            done.send(f())
        c.send(work)
    print("channel capture alias: {done.recv()}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "channel capture alias: fault\n",
        "a receiver-side push through a captured alias now faults under D4 (TICKET-169): {out:?}"
    );
}

#[test]
fn airlock_task_local_alias_of_a_global_is_one_object() {
    // TICKET-105, G7. A spawn-arg alias (`spawn f(gl[0], r)`) and a whole-value alias (`a := gl`)
    // are both closed by TICKET-111 (W12-5): the task's copy of the alias is now ADOPTED as the
    // global's own object, so a push through the alias is visible through the global too. CPython
    // 3.14.7 prints `[1, 2] [1, 2]` and `[[1, 2]] [[1, 2]]` (measured 2026-09-10).
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn f(x: List[int], r: Channel[str]):
    res := recover:
        x.push(2)
    match res:
        ?_: r.send("no-fault {x} {gl[0]}")
        !_: r.send("fault {x} {gl[0]}")

fn main():
    r := Channel[str](1)
    parallel:
        spawn f(gl[0], r)
    print("spawn arg alias: {r.recv()}")
main()
"#;
    let out = golden_entry(src);
    // TICKET-169 (D4 layer C): TICKET-111's adoption still holds (`x` IS `gl[0]`'s copy, one
    // object), but D4 now faults the write through either name instead of letting it land.
    assert_eq!(
        out, "spawn arg alias: fault [1] [1]\n",
        "a spawn-arg alias of a global element must fault as the global's own (adopted) copy \
         (TICKET-111 identity, TICKET-169 write fault): {out:?}"
    );

    let src2 = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn main():
    a := gl
    r := Channel[str](1)
    parallel:
        spawn:
            res := recover:
                a[0].push(2)
            match res:
                ?_: r.send("no-fault {a} {gl}")
                !_: r.send("fault {a} {gl}")
    print("whole alias: {r.recv()}")
main()
"#;
    let out2 = golden_entry(src2);
    assert_eq!(
        out2, "whole alias: fault [[1]] [[1]]\n",
        "a whole-value alias of a global must fault as the global's own (adopted) copy \
         (TICKET-111 identity, TICKET-169 write fault): {out2:?}"
    );
}

#[test]
fn airlock_adopted_alias_keeps_a_later_globals_backref_into_its_old_subtree() {
    // TICKET-111, gotcha 2. `inner` (a spawn-crossed alias of `gl[0]`) is adopted as the global's
    // object during the module snapshot's replay, but the snapshot memo cached `y := gl[0][0]`
    // EARLIER in the nursery, so `y`'s Backref must still resolve into the adopted node's OLD
    // subtree even though that subtree is discarded rather than installed. CPython 3.14.7 prints
    // `[] [[]] [1]` (measured 2026-09-11). The two sibling sends race, so the program prints
    // whichever message is not "first" rather than the second one received.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[[1]]]
y := gl[0][0]

fn main():
    inner := gl[0]
    r := Channel[str](2)
    parallel:
        spawn:
            r.send("first")
        x := inner.pop()
        spawn:
            r.send("{inner} {gl} {y}")
    a := r.recv()
    b := r.recv()
    print(if a == "first": b else: a)
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[] [[]] [1]\n",
        "a later global's Backref into an adopted alias's discarded subtree must still resolve \
         (TICKET-111 gotcha 2): {out:?}"
    );
}

#[test]
fn airlock_channel_message_ids_stay_in_their_own_crossing() {
    // TICKET-111, gotcha 1. `c`'s received `[[7]]` mints ids in the CHANNEL's own id space, and
    // must resolve to the freshly-received list, never to another crossing's node. `inner` is a
    // parent-made alias the task captures, so it is a crossing copy until M3.4 and its push
    // faults; `gl[0]` is the run's one list (M3.1). CPython 3.14.7 prints `[[7, 5]] [1, 2] [1, 2]`
    // (one object; M3.4 removes the capture copy).
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn main():
    inner := gl[0]
    c := Channel[List[List[int]]](1)
    r := Channel[str](1)
    parallel:
        spawn:
            m := c.recv()
            m[0].push(5)
            _ := recover: inner.push(2)
            r.send("{m} {inner} {gl[0]}")
        c.send([[7]])
    print(r.recv())
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[[7, 5]] [1] [1]\n",
        "a Channel message's ids must never resolve to another crossing's node \
         (TICKET-111 gotcha 1): {out:?}"
    );
}

#[test]
fn generator_guard_clears_on_every_unwind_path() {
    // The guard is the VM's existing `active_generators` root list, pushed on resume and popped on
    // EVERY exit path — so it is self-clearing and can never poison a generator as permanently
    // "running". One case per unwind path.
    // (a) normal exhaustion, (b) an early `break` then a legitimate resume of the SAME generator,
    // (c) a generator whose body FAULTS inside a `recover:` — after recovery a fresh generator still
    //     runs to completion (and the faulted one is closed, like a Python generator: `.next()` → None),
    // (d) a generator driving a DIFFERENT generator (distinct GcRefs) — no over-rejection.
    let src = r#"
fn count(n: int) -> Iterator[int]:
    for i in 0..n:
        yield i

fn boom() -> Iterator[int]:
    yield 1
    print(1 / 0)
    yield 2

fn outer() -> Iterator[int]:
    for v in count(2):
        yield v * 10

fn main():
    # (a) fully consumed, then a second generator works
    for x in count(2):
        print("a {x}")
    for x in count(2):
        print("a2 {x}")

    # (b) break mid-`for`, then resume the SAME generator by hand — the guard is not stuck
    g := count(4)
    for x in g:
        print("b {x}")
        break
    print("b-resume {g.next()}")

    # (c) a faulting body, recovered — then a FRESH generator still runs; the faulted one is closed
    bad := boom()
    r := recover:
        for x in bad:
            print("c {x}")
        0
    match r:
        ?_: print("c no fault")
        !_: print("c caught")
    # a generator whose body faulted is CLOSED (like Python) — a later resume answers None
    print("c closed {bad.next()}")
    for x in count(2):
        print("c-after {x}")

    # (d) a generator driving another generator — distinct generators, both resume fine
    for x in outer():
        print("d {x}")
main()
"#;
    assert_golden_out(
        src,
        "a 0\na 1\na2 0\na2 1\nb 0\nb-resume 1\nc 1\nc caught\nc closed None\nc-after 0\nc-after 1\nd 0\nd 10\n",
    );
    let out = run_capture(src).expect("no path leaves a generator poisoned as running");
    let want = "a 0\na 1\na2 0\na2 1\nb 0\nb-resume 1\nc 1\nc caught\nc closed None\nc-after 0\nc-after 1\nd 0\nd 10\n";
    assert_eq!(out, want, "every unwind path clears the running guard");
}

#[test]
fn defer_top_level_runs_on_unhandled_error() {
    // An unhandled top-level `?` error still unwinds through the module body's defers (cleanup
    // runs before the program reports the error).
    let src = "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return !\"nope\"\ndefer log(\"cleanup\")\nprint(\"before\")\nx := boom()?\nprint(\"after\")\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, _ic) = run_file(&entry);
    let (vo, _ve, vr, _vc) = run_file(&entry);
    assert_eq!(
        io, "before\ncleanup\n",
        "interp: top-level defer runs on unhandled error"
    );
    assert_eq!(
        vo, "before\ncleanup\n",
        "vm: top-level defer runs on unhandled error"
    );
    assert!(
        ir.is_err() && vr.is_err(),
        "unhandled `?` is an error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn exit_is_not_caught_by_recover() {
    // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
    let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, _ir, ic) = run_file(&entry);
    let (vo, _ve, _vr, vc) = run_file(&entry);
    assert_eq!(io, "", "interp: nothing after the recover runs");
    assert_eq!(vo, "", "vm: nothing after the recover runs");
    assert_eq!(ic, Some(7), "interp exit code");
    assert_eq!(vc, Some(7), "vm exit code");
}

#[test]
fn exit_in_spawned_child_aborts_siblings() {
    // B1/B2: `std.os.exit` inside a child is a hard halt for the PROGRAM — the post-`parallel:`
    // statement never runs and the exit code propagates. Whether the sibling `b` prints depends on
    // the schedule: since TICKET-224 a run-wide halt lands at every function entry, and `print` is a
    // call, so `b` is cut before printing when the exit is already published (measured: T=1 100/100
    // cut, T=2 and the default count 0/100). Go's `os.Exit` gives the same either-way result for a
    // sibling goroutine. So `b` is not asserted; `a`, the missing "after" and the code are.
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert!(
        vo.contains('a') && io.contains('a'),
        "the exiting child's output flushes: got {vo:?} / {io:?}"
    );
    assert!(
        !io.contains("after") && !vo.contains("after"),
        "the post-parallel statement never runs after os.exit: mn={io:?} serial={vo:?}"
    );
    assert_eq!(vc, Some(3), "serial exit code");
    assert_eq!(ic, Some(3), "M:N exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "os.exit is a clean halt, not an error: mn={ir:?} serial={vr:?}"
    );
}

/// B3.4: a child `std.os.exit(code)` on the `--parallel` OS-thread pool is a clean hard-halt,
/// not a fault. Cross-thread: the worker's `pending_exit` propagates up the join to the parent
/// VM, the exiting child's buffered output is flushed, and the post-`parallel:` statement never
/// runs. The `--parallel` counterpart of `exit_in_spawned_child_aborts_siblings`.
#[test]
fn parallel_child_os_exit_halts_with_code() {
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, res, code) = run_file_with(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(3),
        "child os.exit code propagates cross-thread to the parent"
    );
    assert!(
        res.is_ok(),
        "os.exit is a clean halt, not an error: {res:?}"
    );
    assert!(
        out.contains('a'),
        "the exiting child's buffered output is flushed: got {out:?}"
    );
    assert!(
        !out.contains("after"),
        "the post-parallel statement never runs after os.exit: got {out:?}"
    );
}

/// B3.4: `os.exit` in one child aborts a `recv`-blocked sibling too (same machinery as a fault —
/// it trips the nursery cancel flag), so the join completes with the exit code instead of hanging.
/// `exiter` is spawned first → runs inline on the joining thread (the exit trips cancel without
/// depending on pool scheduling); the recv-blocked `consumer` runs on the pool and aborts.
#[test]
fn parallel_os_exit_aborts_recv_blocked_sibling() {
    let src = "import std.os\nfn exiter(ch: Channel[int]):\n    os.exit(5)\nfn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn exiter(ch)\n        spawn consumer(ch)\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, _res, code) = run_file_with(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(5),
        "os.exit code propagates; the recv-blocked consumer aborts, no hang"
    );
    assert!(
        !out.contains("consumed"),
        "the aborted consumer never ran past its blocked recv: got {out:?}"
    );
}

/// gaps.md B5 — cross-nursery child→parent wake parity: a `send` from a nested (eager) `parallel:`
/// nursery must wake a receiver parked in the OUTER nursery. Uncontended 1-send / 1-recv, so a plain
/// run must match `.expected`. Pre-fix M:N false-faulted `deadlock` while the since-removed serial
/// engine printed "receiver got 1". Runs the M:N leg under a watchdog (10 rounds) so a lost-wakeup
/// regression fails loud rather than hanging.
#[test]
fn parallel_cross_nursery_nested_send_to_outer_recv_parity() {
    let src =
        include_str!("../../../examples/parallel_cross_nursery_nested_send_to_outer_recv.chz");
    let expected =
        include_str!("../../../examples/parallel_cross_nursery_nested_send_to_outer_recv.expected");
    // Baseline run.
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (so, _se, sr, _) = run_file(&entry);
    assert!(sr.is_ok(), "baseline run faulted: {sr:?}");
    assert_eq!(so, expected, "baseline output != expected");
    // M:N leg under a watchdog, several rounds to shake a flaky lost wakeup.
    for _ in 0..10 {
        let (mo, _me, mr, _) = run_parallel_watchdog(src);
        assert!(
            mr.is_ok(),
            "M:N faulted (spurious cross-nursery deadlock / lost wakeup): {mr:?}"
        );
        assert_eq!(mo, expected, "M:N stdout != expected");
    }
}

/// B3.5 — the since-removed cooperative engine's `fibers_all_blocked_is_deadlock` golden, ported to
/// `--parallel`: two tasks each block on a distinct empty channel with no producer. Under threads
/// B3.5's nursery-local detector must fault it too rather than hang on the condvars.
#[test]
fn parallel_all_blocked_deadlock_faults() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
    let (_o, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("an all-blocked --parallel nursery must fault, not hang");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// gaps.md W13-3 — a genuine nested deadlock (main body AND a nested owner body both
/// channel-parked on `never.recv()`, leaf at depth 2) must fault `deadlock`, not hang. At the
/// default thread count the watchdog times out instead.
#[test]
fn parallel_nested_deadlock_at_channel_parked_owner_faults() {
    let src = "fn main():\n    never := Channel[int](0)\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    never.recv()\n                never.recv()\n        never.recv()\n    print(\"unreachable\")\nmain()\n";
    let (_o, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("a genuine nested deadlock must fault, not hang");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// A blocking `for v in ch:` (recv) with no producer and no runnable sibling deadlocks with an
/// ENGINE-AGNOSTIC message — the old text hardcoded "sequential executor", which
/// is misleading under the default M:N (real-thread) engine.
#[test]
fn deadlock_fault_message_is_engine_agnostic() {
    let msg = golden_entry_fault(
        "fn main():\n    ch := Channel[int]()\n    for v in ch:\n        print(v)\nmain()\n",
    );
    assert!(msg.contains("deadlock"), "got: {msg}");
    assert!(!msg.contains("sequential executor"), "got: {msg}");
}

/// `recover:` is transparent to the deadlock verdict (TICKET-135, D1): the program aborts with the
/// engine-agnostic text instead of binding `Err` (a scheduler verdict is fatal, like Go's
/// `all goroutines are asleep`).
#[test]
fn deadlock_fault_is_not_recoverable_new_message() {
    let src = "fn main():\n\
               \x20   ch := Channel[int]()\n\
               \x20   r := recover:\n\
               \x20       for v in ch:\n\
               \x20           print(v)\n\
               \x20   match r:\n\
               \x20       ?_: print(\"ok\")\n\
               \x20       !e: print(\"caught: {e.message()}\")\n\
               main()\n";
    let msg = golden_entry_fault(src);
    assert!(msg.contains("deadlock"), "got: {msg}");
    assert!(!msg.contains("sequential executor"), "got: {msg}");
}

/// `std.cancel` wakeup regression — a sibling's `cancel()` (which `trip()`s the token's `done()`
/// channel) must wake a fiber **parked** in `wait: tok.done().recv()` under the OS-thread engine.
/// The canceller sleeps first so the waiter reliably reaches `park_wait` and blocks; the `trip()`
/// then routes through `close_wake`, which must reach the parked `WaitPark` token and re-poll it to
/// observe the latch. (The *narrower* park-decision-vs-`trip` race is closed by adding `done_latch`
/// to `MnSched::park`/`park_wait`'s gap re-check — correct by construction, mirroring how `closed`
/// is handled; not deterministically forceable from source.) All rounds must complete.
#[test]
fn cancel_trip_wakes_parked_wait_under_parallel() {
    let src = "import std.cancel\n\
                   import std.time\n\
                   fn waiter(tok: cancel.Token, out: Channel[bool]):\n\
                   \x20   wait:\n\
                   \x20       _ := tok.done().recv(): out.send(true)\n\
                   fn canceller(tok: cancel.Token):\n\
                   \x20   time.sleep_ms(5)\n\
                   \x20   tok.cancel()\n\
                   fn main():\n\
                   \x20   out := Channel[bool]()\n\
                   \x20   n := 30\n\
                   \x20   for i in 0..n:\n\
                   \x20       tok := cancel.manual()\n\
                   \x20       parallel:\n\
                   \x20           spawn waiter(tok, out)\n\
                   \x20           spawn canceller(tok)\n\
                   \x20   c := 0\n\
                   \x20   for _ in 0..n:\n\
                   \x20       out.recv()\n\
                   \x20       c = c + 1\n\
                   \x20   print(c)\n\
                   main()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "cancel→wait must not fault/strand: {res:?}");
    assert_eq!(
        out, "30\n",
        "every parked waiter must be woken by the sibling's cancel"
    );
}

/// B3.5 — the named anti-false-positive case: one sibling `send`s the very channel the other
/// `recv`s, so the nursery genuinely progresses. The barrier-confirm detector must NOT report a
/// deadlock (a real send aborts any half-built all-blocked confirmation).
#[test]
fn parallel_near_miss_does_not_false_positive() {
    let src = "fn consumer(c: Channel[int]):\n    print(c.recv())\nfn producer(c: Channel[int]):\n    c.send(7)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn consumer(c)\n        spawn producer(c)\n    print(\"done\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "near-miss must not fault: {res:?}");
    assert!(
        out.contains('7'),
        "consumer received the sent value: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
}

/// B3.5 — a three-task relay (consumer ← relay ← producer) where `blocked == live` is reached
/// only momentarily while a message is in flight. A naive blocked-count detector false-positives
/// here; the per-epoch barrier (a worker holding a deliverable message pops it instead of
/// confirming empty) must not.
#[test]
fn parallel_chained_near_miss_no_false_positive() {
    let src = "fn relay(x: Channel[int], z: Channel[int]):\n    v := x.recv()\n    z.send(v)\nfn producer(x: Channel[int]):\n    x.send(1)\nfn consumer(z: Channel[int]):\n    print(z.recv())\nfn main():\n    x := Channel[int]()\n    z := Channel[int]()\n    parallel:\n        spawn consumer(z)\n        spawn relay(x, z)\n        spawn producer(x)\n    print(\"ok\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "chained relay must not false-positive: {res:?}"
    );
    assert!(
        out.contains('1'),
        "the relayed value reached the consumer: {out:?}"
    );
    assert!(out.contains("ok"), "the nursery joined: {out:?}");
}

/// D6 — single-connection loopback over `std.net` on the M:N engine: a `parallel:` runs a server
/// fiber (`listen`/`accept`/`read`/`write`) and a client fiber (`connect`/`write`/`read`) in one
/// program. A would-block `accept`/`read` parks on the netpoller and resumes on readiness, so the
/// round-trip completes without a thread per op. `Listener.addr()` surfaces the OS-assigned port so
/// the client can reach the `:0` bind. Watchdog-guarded.
#[test]
fn net_loopback_round_trip_over_parallel() {
    let src = r#"import std.net

fn serve(server: net.Listener) -> int!:
    conn := server.accept()?
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    server.close()
    return ?0

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("hello")?
    reply := sock.read(64)?
    print(reply)
    sock.close()
    return ?0

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return ?0

fn main():
    match run():
        ?_: print("done")
        !e: print("net error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "net round-trip must not fault: {res:?}");
    assert!(
        out.contains("echo:hello"),
        "the client received the server's echo: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
    assert!(
        !out.contains("net error"),
        "no I/O error on the happy path: {out:?}"
    );
}
