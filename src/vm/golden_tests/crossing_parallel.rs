//! Golden tests: closures and module globals handed to tasks, `parallel:` programs, deadlock and
//! cancel trips.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

#[test]
fn crossing_crossed_closure_reads_module_global_live_same_task() {
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
k := ch.recv() else: panic("ch closed")
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
fn crossing_nested_closure_created_after_the_tasks_own_write_reads_it() {
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
        o := ch.recv() else: panic("ch closed")
        _ := recover:
            n = 100
        d := o()
        res.send(d())
        res.send(n)
    ch.send(mk())
got := res.recv() else: panic("res closed")
print("nested closure sees : {got}")
got2 := res.recv() else: panic("res closed")
print("plain global read   : {got2}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "nested closure sees : 100\nplain global read   : 100\n",
        "a closure created after the task's own write must read it \
         like any other read: {out:?}"
    );
}

#[test]
fn crossing_forwarded_closure_reads_the_forwarders_write() {
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
        k := ch1.recv() else: panic("ch1 closed")
        _ := recover:
            n = 999
        ch2.send(k)
    spawn:
        k2 := ch2.recv() else: panic("ch2 closed")
        res.send(k2(3))

got := res.recv() else: panic("res closed")
print("result: {got}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "result: 2997\n",
        "a forwarded closure must read the run's one copy of the module global, with \
         the forwarder's write: {out:?}"
    );
}

#[test]
fn crossing_receiving_tasks_own_write_beats_an_arriving_closures_global() {
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
        k := ch.recv() else: panic("ch closed")
        res.send(k())
        res.send(n)
    spawn:
        ch.send(mk())

got := res.recv() else: panic("res closed")
print("closure sees : {got}")
got2 := res.recv() else: panic("res closed")
print("task read    : {got2}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 5\ntask read    : 5\n",
        "the receiving task's own write to a module global must beat an arriving \
         closure's stale value for that same global: {out:?}"
    );
}

#[test]
fn crossing_arriving_closure_loses_to_the_receiving_views_own_assignment() {
    // TICKET-051: same shape as
    // `crossing_receiving_tasks_own_write_beats_an_arriving_closures_global` with a top-level `n =
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
        k := ch.recv() else: panic("ch closed")
        res.send(k())
        res.send(n)
    spawn:
        ch.send(mk())

got := res.recv() else: panic("res closed")
print("closure sees : {got}")
got2 := res.recv() else: panic("res closed")
print("task read    : {got2}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 5\ntask read    : 5\n",
        "the receiving task's own write must win even when the sender inherited a \
         different value for the same global: {out:?}"
    );
}

#[test]
fn crossing_arriving_closure_does_not_clobber_the_receivers_in_place_push() {
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
        k := ch.recv() else: panic("ch closed")
        res.send("{xs} len {k()}")
    spawn:
        ch.send(mk())

got := res.recv() else: panic("res closed")
print(got)
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[1, 9] len 2\n",
        "an arriving closure's global that no ancestor ever assigned must not replace the \
         receiver's in-place push: {out:?}"
    );
}

#[test]
fn crossing_closure_from_a_sender_that_only_inherited_a_global_reads_the_live_global() {
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
    spawn:
        f := ch.recv() else: panic("ch closed")
        res.send(f())
    n = 7
    spawn: ch.send(mk())

got := res.recv() else: panic("res closed")
print("closure sees : {got}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "closure sees : 7\n",
        "a received closure must read the run's one copy of the global, with the \
         root's write: {out:?}"
    );
}

#[test]
fn crossing_forwarded_closure_without_its_own_write_reads_the_first_senders_write() {
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
        k := ch1.recv() else: panic("ch1 closed")
        ch2.send(k)
    spawn:
        k2 := ch2.recv() else: panic("ch2 closed")
        res.send(k2(3))

got := res.recv() else: panic("res closed")
print("result: {got}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "result: 2997\n",
        "a forwarded closure must read the run's one copy of the global, with the \
         first sender's write: {out:?}"
    );
}

#[test]
fn crossing_same_task_closure_round_trip_mutates_module_list_in_place() {
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
got := ch.recv() else: panic("ch closed")
got()
print("module xs: {xs}")
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "module xs: [1, 9]\n",
        "a same-task channel round trip must mutate the module global in place: {out:?}"
    );
}

#[test]
fn crossing_closure_calling_a_helper_that_reads_a_global_reads_the_live_global() {
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
    indirect := c1.recv() else: panic("c1 closed")
    print("indirect before: {indirect()}")
    direct := c2.recv() else: panic("c2 closed")
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
fn crossing_a_called_helper_that_writes_the_global_keeps_it_a_late_load() {
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
    f := c.recv() else: panic("c closed")
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
fn crossing_a_closure_calling_a_nested_fn_reads_the_live_global() {
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
    got := c.recv() else: panic("c closed")
    print("nested-fn indirect: {got()}")
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
fn crossing_closure_over_a_global_the_sender_mutated_in_place_reads_the_live_global() {
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
    f := c.recv() else: panic("c closed")
    g := c2.recv() else: panic("c2 closed")
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
fn crossing_closure_over_a_global_the_sender_mutated_by_index_or_field_assignment_reads_the_live_global()
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
    f := c.recv() else: panic("c closed")
    g := c2.recv() else: panic("c2 closed")
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
fn crossing_closure_over_a_global_a_user_struct_method_mutated_reads_the_live_global() {
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
    got := c.recv() else: panic("c closed")
    print("user-method mutation via closure: {got()}")
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
fn crossing_closure_over_a_global_main_mutated_through_a_local_alias_reads_the_live_global() {
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
            f := c.recv() else: panic("c closed")
            done.send(f())
        xs := g
        xs.push(2)
        c.send(fn() -> str: "{g}")
    got := done.recv() else: panic("done closed")
    print("main-as-sender alias: {got}")
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
fn crossing_closure_over_a_global_the_sender_mutated_through_a_struct_alias_reads_the_live_global()
{
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
    got := c.recv() else: panic("c closed")
    print("struct alias: {got()}")
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
fn crossing_alias_write_reaches_a_grandchild_senders_closure() {
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
    got := c.recv() else: panic("c closed")
    print("grandchild alias: {got()}")
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
fn crossing_closure_over_an_aliased_element_reads_the_live_global() {
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
            f := c.recv() else: panic("c closed")
            done.send(f())
        inner.push(2)
        c.send(fn() -> str: "{inner} {gl[0]}")
    got := done.recv() else: panic("done closed")
    print("sender-push alias: {got}")
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
fn crossing_unchanged_aliased_global_does_not_clobber_the_receivers_push() {
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
            k := ch.recv() else: panic("ch closed")
            res.send("{xs} len {k()}")
        a := xs
        n := a.len()
        ch.send(fn() -> int: xs.len() + n)
    got := res.recv() else: panic("res closed")
    print(got)
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
fn crossing_closure_over_a_captured_alias_pushed_by_the_receiver_is_seen() {
    // TICKET-105, G6. The push through `inner` runs in the RECEIVER's captured closure body,
    // AFTER the send. M3.2: a channel moves the closure itself, so its capture of `inner` is the
    // sender's `gl[0]`, and both reads see the push. Go (`rewrites.go`, M3.2 report): a closure
    // sent over a channel writes its captured map, and both reads print `2 2`.
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
            f := c.recv() else: panic("c closed")
            done.send(f())
        c.send(work)
    got := done.recv() else: panic("done closed")
    print("channel capture alias: {got}")
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "channel capture alias: [1, 2] [1, 2]\n",
        "a receiver-side push through a captured alias lands on the sender's object: {out:?}"
    );
}

#[test]
fn crossing_task_local_alias_of_a_global_is_one_object() {
    // TICKET-105, G7. A spawn-arg alias (`spawn f(gl[0], r)`) and a whole-value alias (`a := gl`)
    // are the global's own object (M3.1 one global, M3.4 by handle), so a push through the alias is
    // visible through the global. CPython 3.14.7 prints `[1, 2] [1, 2]` and `[[1, 2]] [[1, 2]]`
    // (measured 2026-09-10).
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
    assert_eq!(
        out, "spawn arg alias: no-fault [1, 2] [1, 2]\n",
        "a spawn-arg alias of a global element is the global's own list: {out:?}"
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
        out2, "whole alias: no-fault [[1, 2]] [[1, 2]]\n",
        "a whole-value alias of a global is the global itself: {out2:?}"
    );
}

#[test]
fn crossing_adopted_alias_keeps_a_later_globals_backref_into_its_old_subtree() {
    // TICKET-111, gotcha 2 (M3.1 deleted the snapshot). `inner` (an alias of `gl[0]`) was adopted
    // as the global's object during the module snapshot's replay, but the memo cached `y := gl[0][0]`
    // EARLIER in the nursery, so `y`'s Backref had to resolve into the adopted node's OLD subtree. CPython 3.14.7 prints
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
    a := r.recv() else: panic("r closed")
    b := r.recv() else: panic("r closed")
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
fn crossing_channel_message_ids_stay_in_their_own_crossing() {
    // TICKET-111, gotcha 1. `c`'s received `[[7]]` must be the freshly-sent list, never another
    // crossing's node. `inner` is a parent-made alias the task captures: M3.4 hands it over by
    // handle, so it is `gl[0]` itself (M3.1: the run's one list) and its push lands. CPython 3.14.7
    // prints `[[7, 5]] [1, 2] [1, 2]`.
    let src = r#"
import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency

gl := [[1]]

fn main():
    inner := gl[0]
    c := Channel[List[List[int]]](1)
    r := Channel[str](1)
    parallel:
        spawn:
            m := c.recv() else: panic("c closed")
            m[0].push(5)
            _ := recover: inner.push(2)
            r.send("{m} {inner} {gl[0]}")
        c.send([[7]])
    got := r.recv() else: panic("r closed")
    print(got)
main()
"#;
    let out = golden_entry(src);
    assert_eq!(
        out, "[[7, 5]] [1, 2] [1, 2]\n",
        "a Channel message's ids must never resolve to another crossing's node \
         (TICKET-111 gotcha 1): {out:?}"
    );
}

#[test]
fn generator_guard_clears_on_every_unwind_path() {
    // The guard is the generator's own box: a resume takes it, and a yield, a return or a fault
    // unwind of the body frame (`RetAction::Gen`) puts one back — so it can never poison a
    // generator as permanently "running". One case per unwind path.
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
    let (vo, _ve, vr, _vc) = run_file(&entry);
    assert_eq!(
        vo, "before\ncleanup\n",
        "top-level defer runs on unhandled error"
    );
    assert!(vr.is_err(), "unhandled `?` is an error: {vr:?}");
}

#[test]
fn exit_is_not_caught_by_recover() {
    // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
    let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (vo, _ve, _vr, vc) = run_file(&entry);
    assert_eq!(vo, "", "nothing after the recover runs");
    assert_eq!(vc, Some(7), "exit code");
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
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert!(
        vo.contains('a'),
        "the exiting child's output flushes: got {vo:?}"
    );
    assert!(
        !vo.contains("after"),
        "the post-parallel statement never runs after os.exit: {vo:?}"
    );
    assert_eq!(vc, Some(3), "exit code");
    assert!(vr.is_ok(), "os.exit is a clean halt, not an error: {vr:?}");
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
/// run must match `.expected`. Pre-fix it false-faulted `deadlock` (the right output is
/// "receiver got 1"). Then 10 more rounds run under a watchdog (repeated on purpose) so a
/// lost-wakeup regression fails loud rather than hanging.
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
    // Repeated rounds under a watchdog, to shake a flaky lost wakeup.
    for _ in 0..10 {
        let (mo, _me, mr, _) = run_parallel_watchdog(src);
        assert!(
            mr.is_ok(),
            "faulted (spurious cross-nursery deadlock / lost wakeup): {mr:?}"
        );
        assert_eq!(mo, expected, "watchdog-round stdout != expected");
    }
}

/// B3.5 — two tasks each block on a distinct empty channel with no producer. Under threads
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

/// TICKET-299 (wave 24 F2): the callee-frame `recover:` shape of the owner's repro.
const CALLEE_RECOVER_REPRO: &str = r#"
fn feeder(c: Channel[int]):
    for i in 0..3:
        c.send(i * 100)
fn body(c: Channel[int]):
    parallel:
        spawn feeder(c)
        defer:
            for _ in 0..3:
                print("got {c.recv() ?? -1}")
        panic("body boom")
fn main():
    r2 := recover:
        body(Channel[int](0))
    print("callee -> {r2}")
main()"#;
const CALLEE_RECOVER_WANT: &str = "got 0\ngot 100\ngot 200\ncallee -> !body boom\n";

#[test]
fn parallel_body_fault_defers_run_before_join_in_callee_frame() {
    // TICKET-299 (wave 24 F2). A faulting `parallel:` body drains its `defer`s BEFORE the children
    // are cancelled and joined, in every frame shape. Same-frame `recover:` already did; a caller's
    // `recover:` drained the nursery first, so the deferred `recv` deadlocked.
    assert_golden_out(CALLEE_RECOVER_REPRO, CALLEE_RECOVER_WANT);
}

/// TICKET-299 (wave 24 F2, owner option B): the exit x `recover:`-placement x worker-count grid of
/// a two-level `parallel:`. Inner level: body defer `B`, child `C`; outer level: body defer `b`,
/// child `c`; recover-block defer `K`. A child's defer waits on a gate its level's body defer sends
/// on, so the order is causal: a level that cancels and joins before its body defers deadlocks or
/// hangs (5 s watchdog per cell), and no cell races. Every cell must print `B C b c`, then `K`,
/// except `Return` x `SameFrame|Nested`: `return` cannot leave a `recover:` block, so those two
/// programs must be type rejects.
#[derive(Clone, Copy, Debug)]
enum Exit {
    Panic,
    Index,
    ChildFault,
    Try,
    Return,
    Break,
    Continue,
    Cancel,
}
const EXITS: [Exit; 8] = [
    Exit::Panic,
    Exit::Index,
    Exit::ChildFault,
    Exit::Try,
    Exit::Return,
    Exit::Break,
    Exit::Continue,
    Exit::Cancel,
];
/// `HashHook` and `JsonDefault` make the `Caller` program's call from inside a native that calls
/// back with `native_reentry == 1`: a struct `hash` hook run by a `Map` insert (`vm/hash.rs`
/// `declared_hash`) and a field default run by `json.decode` (`vm/json_decode.rs`
/// `run_decode_default`). There every drain runs on the Rust stack (`drains_on_frames() == false`)
/// and every abort waits inline (parks are off under re-entry).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Place {
    NoRecover,
    SameFrame,
    Caller,
    Nested,
    HashHook,
    JsonDefault,
}
const PLACES: [Place; 6] = [
    Place::NoRecover,
    Place::SameFrame,
    Place::Caller,
    Place::Nested,
    Place::HashHook,
    Place::JsonDefault,
];
const LEAVE_HEAD: &str = "import std.time\nimport std.json as json\nup: Channel[int] = Channel[int](2)\ng1: Channel[int] = Channel[int](1)\ng2: Channel[int] = Channel[int](1)\nready: Channel[int] = Channel[int](1)\nfn kid(tag: str, gate: Channel[int]):\n    defer:\n        _ := gate.recv()\n        print(tag)\n    up.send(1)\n    while true:\n        time.sleep_ms(1)\nfn bad() -> int!str:\n    return !\"q\"\nfn boom():\n    panic(\"child boom\")\nfn faulty():\n    _ := ready.recv()\n    panic(\"sib\")\n";
const LEAVE_SPIN: &str = "while true:\n    time.sleep_ms(1)";
/// TICKET-299: a same-frame `recover:` catches a child fault (the owner's cut is
/// `Cut::Delivered`) while a level is open. The level leaves first (`B C`), then the recover block's
/// defer (`K`); after the catch the cut is cleared, so a later `recover:` catches and a later level joins.
const DELIVERED_CUT: &str = "import std.time\nup: Channel[int] = Channel[int](2)\ng1: Channel[int] = Channel[int](1)\nfn kid(tag: str, gate: Channel[int]):\n    defer:\n        _ := gate.recv()\n        print(tag)\n    up.send(1)\n    while true:\n        time.sleep_ms(1)\nfn boom():\n    panic(\"child boom\")\nfn noop():\n    pass\nfn main():\n    r := recover:\n        defer:\n            print(\"K\")\n        parallel:\n            spawn kid(\"C\", g1)\n            _ := up.recv()\n            defer:\n                print(\"B\")\n                g1.send(1)\n            spawn boom()\n            while true:\n                time.sleep_ms(1)\n    print(\"first -> {r}\")\n    r2 := recover:\n        panic(\"second\")\n    print(\"second -> {r2}\")\n    parallel:\n        spawn noop()\n    print(\"joined\")\nmain()\n";
const DELIVERED_CUT_WANT: &str = "B\nC\nK\nfirst -> !child boom\nsecond -> !second\njoined\n";
fn leave_indent(text: &str, n: usize) -> String {
    let pad = " ".repeat(n);
    text.lines().map(|l| format!("{pad}{l}\n")).collect()
}
/// The two levels, with `exit` as the inner level's last statement.
fn leave_levels(exit: &str) -> String {
    let head = "parallel:\n    spawn kid(\"c\", g2)\n    _ := up.recv()\n    defer:\n        print(\"b\")\n        g2.send(1)\n    parallel:\n        spawn kid(\"C\", g1)\n        _ := up.recv()\n        defer:\n            print(\"B\")\n            g1.send(1)\n";
    format!("{head}{}", leave_indent(exit, 8))
}
fn leave_program(exit: Exit, place: Place) -> String {
    let levels = match exit {
        Exit::Panic => leave_levels("panic(\"p\")"),
        Exit::Index => leave_levels("xs := [1]\nprint(xs[5])"),
        Exit::ChildFault => leave_levels(&format!("spawn boom()\n{LEAVE_SPIN}")),
        Exit::Try => leave_levels("x := bad()?\nprint(x)"),
        Exit::Return => leave_levels("return"),
        Exit::Break => format!(
            "for _ in 0..1:\n{}",
            leave_indent(&leave_levels("break"), 4)
        ),
        Exit::Continue => format!(
            "for _ in 0..1:\n{}",
            leave_indent(&leave_levels("continue"), 4)
        ),
        Exit::Cancel => leave_levels(&format!("ready.send(1)\n{LEAVE_SPIN}")),
    };
    // A function-scoped `?` needs a result type; a recover-scoped one (same frame, nested) does not.
    let (ret, tail, call) = match exit {
        Exit::Try => (" -> int!str", "return ?0\n", "_ := body()"),
        Exit::Panic
        | Exit::Index
        | Exit::ChildFault
        | Exit::Return
        | Exit::Break
        | Exit::Continue
        | Exit::Cancel => ("", "", "body()"),
    };
    let body = format!(
        "fn body(){ret}:\n{}{}",
        leave_indent(&levels, 4),
        leave_indent(tail, 4)
    );
    let k = leave_indent("r := recover:\n    defer:\n        print(\"K\")\n", 4);
    let run = match place {
        Place::NoRecover => format!("{body}fn run():\n    {call}\n"),
        Place::Caller => format!("{body}fn run():\n{k}        {call}\n"),
        Place::SameFrame => format!("fn run():\n{k}{}", leave_indent(&levels, 8)),
        Place::Nested => format!(
            "fn run():\n{k}        if true:\n{}",
            leave_indent(&levels, 12)
        ),
        Place::HashHook => format!(
            "{body}struct Key:\n    n: int\n    fn hash(self) -> int:\n        {call}\n        return self.n\n    fn eq(self, other: Key) -> bool:\n        return self.n == other.n\nfn run():\n{k}        m: Map[Key, int] = {{}}\n        m[Key(1)] = 1\n"
        ),
        Place::JsonDefault => format!(
            "{body}fn mk() -> int:\n    {call}\n    return 0\nstruct Cfg:\n    name: str\n    v: int = mk()\nfn run():\n{k}        _ := json.decode[Cfg](r'{{\"name\":\"x\"}}')\n"
        ),
    };
    // A cancel cell runs the placement inside `victim`, which a faulting sibling cancels.
    let main = match exit {
        Exit::Cancel => {
            "fn victim():\n    run()\nfn main():\n    r0 := recover:\n        parallel:\n            spawn victim()\n            spawn faulty()\n    print(\"end\")\n"
        }
        Exit::Panic
        | Exit::Index
        | Exit::ChildFault
        | Exit::Try
        | Exit::Return
        | Exit::Break
        | Exit::Continue => "fn main():\n    run()\n    print(\"end\")\n",
    };
    format!("{LEAVE_HEAD}{run}{main}main()\n")
}
type LeaveOut = (String, String, Result<(), crate::vm::RunError>, Option<i32>);
/// Runs `src` from a file with a 5 s watchdog: its output, or `None` when it hung.
fn leave_run(src: &str) -> Option<LeaveOut> {
    let t = crate::test_support::TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&entry, crate::native::HostConfig::default()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(r) => Some(r),
        Err(_) => {
            std::mem::forget(t); // the hung run still reads its file
            None
        }
    }
}
/// The checker's messages for `src` (this harness runs the compiler without the checker).
fn leave_check_errors(src: &str) -> String {
    let t = crate::test_support::TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    match crate::checker::check_graph(&graph) {
        Ok(_) => String::new(),
        Err(errs) => errs
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join("; "),
    }
}
#[test]
fn parallel_level_leave_order_at_every_exit_and_recover_site() {
    if crate::vm::rerun_in_child(
        module_path!(),
        "parallel_level_leave_order_at_every_exit_and_recover_site",
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
    let mut red = Vec::new();
    for t in [1usize, 2] {
        crate::vm::set_worker_count(t);
        for exit in EXITS {
            for place in PLACES {
                // `return` cannot leave a `recover:` block: the checker rejects these two programs.
                if let (Exit::Return, Place::SameFrame | Place::Nested) = (exit, place) {
                    let why = leave_check_errors(&leave_program(exit, place));
                    if !why.contains("is not allowed inside a recover block") {
                        red.push(format!("T={t} {exit:?}/{place:?}: not rejected: {why:?}"));
                    }
                    continue;
                }
                let Some((out, ..)) = leave_run(&leave_program(exit, place)) else {
                    red.push(format!("T={t} {exit:?}/{place:?}: hung"));
                    continue;
                };
                let got: String = out
                    .lines()
                    .filter(|l| *l != "end")
                    .map(|l| format!("{l}\n"))
                    .collect();
                let want = if place == Place::NoRecover {
                    "B\nC\nb\nc\n"
                } else {
                    "B\nC\nb\nc\nK\n"
                };
                if got != want {
                    red.push(format!("T={t} {exit:?}/{place:?}: {got:?}"));
                }
            }
        }
        for (name, src, want) in [
            ("repro", CALLEE_RECOVER_REPRO, CALLEE_RECOVER_WANT),
            ("delivered_cut", DELIVERED_CUT, DELIVERED_CUT_WANT),
        ] {
            match leave_run(src) {
                Some((out, ..)) if out == want => {}
                Some((out, ..)) => red.push(format!("T={t} {name}: {out:?}")),
                None => red.push(format!("T={t} {name}: hung")),
            }
        }
    }
    eprintln!("red count: {}", red.len());
    assert!(red.is_empty(), "red cells: {red:#?}");
}
