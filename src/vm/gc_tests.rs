// Extracted from vm/mod.rs (test module). `super::` == the `vm` module.
use super::*;

/// A value reachable only via the operand stack (mid-expression temporary) must survive a
/// collection — the headline use-after-collect trap. Each list is built, left on the stack,
/// then indexed; a GC fires (stress) between build and index.
#[test]
fn value_only_on_operand_stack_survives() {
    assert_eq!(
        run_capture_stress("print([str(1), str(2), str(3)][0] + [str(4), str(5)][1])"),
        "15\n"
    );
}

/// A value held only in a call-frame local slot survives collections triggered by later
/// allocations in the same frame.
#[test]
fn value_in_frame_slot_survives() {
    let src = "\
fn main():
    x := [str(1), str(2)]
    junk := str(3)
    more := [str(4), str(5), str(6)]
    print(x)
main()";
    assert_eq!(run_capture_stress(src), "['1', '2']\n");
}

/// A value reachable only through a module's globals (the namespace cache root) survives.
#[test]
fn value_in_module_global_survives() {
    let src = "\
K := [str(7), str(8)]
fn main():
    a := str(1)
    b := [str(2), str(3)]
    print(K)
main()";
    assert_eq!(run_capture_stress(src), "['7', '8']\n");
}

/// A value reachable only through a closure's captured environment survives — after the
/// defining frame is gone, only the closure object holds it.
#[test]
fn value_in_closure_capture_survives() {
    let src = "\
fn make():
    secret := str(42)
    return fn(): secret
fn main():
    g := make()
    junk := [str(1), str(2), str(3)]
    print(g())
main()";
    assert_eq!(run_capture_stress(src), "42\n");
}

/// Set algebra with heap-allocated (string) elements under GC stress: the source set, the
/// argument set, and the freshly-built result must all survive a collection mid-operation.
#[test]
fn set_algebra_survives_gc_stress() {
    let src = "\
a := Set([\"al\" + \"pha\", \"be\" + \"ta\", \"gam\" + \"ma\"])
b := Set([\"be\" + \"ta\", \"de\" + \"lta\"])
print(a.union(b).len())
print(a.intersection(b).len())
print(a.difference(b).len())
total := 0
for w in a:
    total += w.len()
print(total)";
    // alpha+beta+gamma = 5+4+5 = 14
    assert_eq!(run_capture_stress(src), "4\n1\n2\n14\n");
}

/// `list.sort()` over Comparable structs whose `compare` allocates (triggering GC mid-sort) must
/// not collect the in-flight elements OR the source list — even when the receiver is an inline
/// temporary (popped before dispatch, so otherwise unrooted). Regression for the M7-G3 review.
#[test]
fn struct_sort_survives_gc_stress() {
    let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c), str(o.c)]
        return self.c - o.c
    fn eq(self, o: M) -> bool:
        return self.c == o.c
fn make() -> List[M]:
    xs := []
    i := 0
    while i < 8:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    xs := make()
    xs.sort()
    out := \"\"
    for m in xs:
        out = out + str(m.c)
    print(out)
    make().sort()              # inline temporary receiver
    print(\"ok\")
main()";
    assert_eq!(run_capture_stress(src), "00123456\nok\n");
}

/// M23 — `==` dispatching to a user `eq` that ALLOCATES (triggering GC mid-compare). `eq_operator`
/// POPS both operands before `run_proto`, so an inline-temporary operand (`M(1) == M(2)`) is reachable
/// only through the argument vec while the callee runs — the same rooting question
/// `struct_sort_survives_gc_stress` asks of `compare`. Not expressible in `tests/chz/` (no way to force
/// a collection from `assert`), which is why it lives here.
#[test]
fn struct_eq_dispatch_survives_gc_stress() {
    let src = "\
struct M:
    c: int
    fn eq(self, o: M) -> bool:
        junk := [str(self.c), str(o.c)]
        return junk[0] == junk[1]
fn make(n: int) -> M:
    return M(n)
fn main():
    a := make(3)
    out := \"\"
    i := 0
    while i < 8:
        # both an inline-temporary operand and a rooted local, plus the negated spelling
        if a == make(i % 4):
            out = out + \"y\"
        elif make(i) != make(i):
            out = out + \"!\"
        else:
            out = out + \"n\"
        i = i + 1
    print(out)
main()";
    assert_eq!(run_capture_stress(src), "nnnynnny\n");
}

/// A struct key's `hash()` allocates (triggering GC mid-operation). The map/set obj and the
/// in-flight key/value — popped off the operand stack before dispatch — must stay rooted across
/// every hash, including with an INLINE-TEMPORARY receiver (`make_map().get(k)`). Regression for
/// the hash-table struct-key rooting.
#[test]
fn map_struct_key_survives_gc_stress() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n), str(self.n + 1)]
        return self.n
fn make_map() -> Map[K, str]:
    m: Map[K, str] = {}
    i := 0
    while i < 8:
        m[K(i)] = str(i)
        i = i + 1
    return m
fn main():
    m := make_map()
    out := \"\"
    for k in m:
        out = out + m[k]
    print(out)
    print(m.has(K(3)))
    print(m.get(K(5)))
    print(make_map().get(K(2)))   # inline-temporary receiver
    print(m.remove(K(0)))
    print(m.len())
main()";
    assert_eq!(
        run_capture_stress(src),
        "01234567\ntrue\nSome('5')\nSome('2')\nSome('0')\n7\n"
    );
}

/// Set construction (`Set([..])`) + `add` over structs whose `hash()` allocates, including
/// algebra — none of the elements may be collected mid-hash.
#[test]
fn set_struct_hash_survives_gc_stress() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n)]
        return self.n
fn main():
    a := Set([K(1), K(2), K(2), K(3)])
    print(a.len())
    a.add(K(3))
    a.add(K(4))
    print(a.len())
    b := Set([K(3), K(4), K(5)])
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
main()";
    // a = {1,2,3,4}; b = {3,4,5}; |a∪b|=5, |a∩b|=2, |a\\b|=2
    assert_eq!(run_capture_stress(src), "3\n4\n5\n2\n2\n");
}

/// M23 Task 4 — the CONTAINER twin of [`set_struct_hash_survives_gc_stress`]: every probe now
/// dispatches a user `eq`, so a probe re-enters the VM (and can collect) where before it was pure
/// `&self`. The receiver, the needle, the wire of a half-built local `SetData`/`MapData`/`Vec` and a
/// freshly-snapshotted key are all off the operand stack at that moment. `hash` is deliberately
/// allocation-FREE here so the only re-entry under test is `eq`'s. Inline-temporary receivers
/// (`make_set().has(..)`) are the sharpest case: nothing but the rooting keeps them alive.
#[test]
fn set_struct_eq_survives_gc_stress() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        return self.n % 3
    fn eq(self, o: K) -> bool:
        junk := [str(self.n), str(o.n)]
        return junk[0] == junk[1]
fn make_set() -> Set[K]:
    return Set([K(1), K(2), K(2), K(3)])
fn make_list() -> List[K]:
    return [K(1), K(2), K(2), K(3)]
fn make_map() -> Map[K, str]:
    m: Map[K, str] = {}
    i := 0
    while i < 6:
        m[K(i)] = str(i)
        i = i + 1
    return m
fn main():
    a := make_set()
    print(a.len())
    a.add(K(3))
    a.add(K(4))
    print(a.len())
    b := Set([K(3), K(4), K(5)])
    print(a.union(b).len(), a.intersection(b).len(), a.difference(b).len())
    print((a | b).len(), (a ^ b).len())
    print(make_set().has(K(2)))              # inline-temporary receiver
    print(K(3) in make_set())
    print(make_list().index_of(K(3)), make_list().unique().len())
    print(K(2) in make_list())
    m := make_map()
    print(m.has(K(4)), m.get(K(5)), make_map().get(K(2)), m[K(1)])
    print(m.merge(make_map()).len(), m.remove(K(0)), m.len())
    m.update(make_map())
    print(m.len())
    print({K(7): str(7), K(7): str(8)}.len(), {K(9), K(9)}.len())   # map/set literals
    print(Map([(K(1), str(1)), (K(1), str(2))]).len())              # Map() ctor
    print(Set(make_list()).len())                                   # Set() ctor
main()";
    // a = {1,2,3,4}; b = {3,4,5}; |a∪b|=5, |a∩b|=2, |a\\b|=2, |a^b|=3
    assert_eq!(
        run_capture_stress(src),
        "3\n4\n5 2 2\n5 3\ntrue\ntrue\n3 3\ntrue\ntrue Some('5') Some('2') 1\n6 Some('0') 5\n6\n1 1\n1\n3\n"
    );
}

/// M23 Task 4 — the `RwShared` read-view probes (`contains`/`has`/`get_key`) reconstruct the stored
/// element/key FROM THE WIRE before comparing it, so that reconstruction is a fresh object reachable
/// only from a Rust local while a now-re-entrant `eq` runs. `Atomic.cas` is deliberately absent: it
/// compares under `core.v.lock()` and stays on the structural path (see `netio.rs`).
#[test]
fn rwshared_struct_eq_survives_gc_stress() {
    let src = "\
import std.concurrency
struct K:
    n: int
    fn hash(self) -> int:
        return self.n % 3
    fn eq(self, o: K) -> bool:
        junk := [str(self.n), str(o.n)]
        return junk[0] == junk[1]
fn main():
    # LIST values, not str: a freshly-rebuilt list can never alias an interned program constant,
    # so an unrooted `v` really is collectable while `eq` runs.
    m: Map[K, List[str]] = {K(1): [str(1)], K(2): [str(2), str(3)]}
    box := RwShared(m)
    print(box.has(K(1)), box.has(K(9)), box.get_key(K(2)), box.get_key(K(9)))
    s := RwShared(Set([K(1), K(2)]))
    print(s.contains(K(2)), s.contains(K(7)))
main()";
    assert_eq!(
        run_capture_stress(src),
        "true false Some(['2', '3']) None\ntrue false\n"
    );
}

/// M23 adversarial review, HIGH — a container-equality arm SNAPSHOTS the child handles into a Rust
/// local and then walks it. The M23 rooting design rooted the two source CONTAINERS, which does not
/// keep those children alive: an `eq` that empties the sources orphans every element still to be
/// compared, and the next `heap.get` on one is `dangling GcRef` (an uncatchable abort) — or, before
/// the collection catches up, a compare against a recycled object. The defence measured the same
/// `xs == ys` answering `true` at 0 allocations, `false` at 2000 and aborting at 40000: equality
/// whose ANSWER depends on GC timing. Under `run_capture_stress` (collect at every allocation) the
/// pre-fix binary aborts on the first arm below; the fix roots the snapshot itself
/// (`Vm::with_elem_roots`), inductively down the recursion.
///
/// Every arm the review named is exercised: list, tuple, set, map (keys AND values), struct fields,
/// enum payloads, plus `seq_slot`'s callers (`in` / `contains` / `index_of` / `unique`) and the
/// set-algebra worker.
#[test]
fn container_eq_snapshot_survives_an_eq_that_empties_the_sources() {
    let src = "\
xs: List[K] = []
ys: List[K] = []
armed: List[int] = []

struct K:
    n: int
    fn hash(self) -> int:
        return 0
    fn eq(self, o: K) -> bool:
        # ONE mutation per armed walk: EMPTY the two source lists in place, so the only remaining
        # reference to the elements the caller already snapshotted is that Rust-local snapshot —
        # then allocate, so the stress collector actually runs while the walk still holds them.
        if armed.len() > 0:
            armed.remove_at(0)
            while xs.len() > 0:
                xs.remove_at(0)
            while ys.len() > 0:
                ys.remove_at(0)
            junk := [str(self.n), str(o.n)]
        return self.n == o.n

fn reset():
    while armed.len() > 0:
        armed.remove_at(0)
    while xs.len() > 0:
        xs.remove_at(0)
    while ys.len() > 0:
        ys.remove_at(0)
    xs.push(K(1))
    xs.push(K(2))
    ys.push(K(1))
    ys.push(K(2))

fn arm():
    armed.push(1)

fn main():
    # The List arm: the compare of element 0 empties both sources; element 1 is compared from the
    # snapshot alone. Pre-fix this is `dangling GcRef` (abort) under stress.
    reset()
    arm()
    print(xs == ys)
    # A nested list — the recursion must root at EVERY level, not just the top.
    reset()
    arm()
    print([xs] == [ys])
    # `seq_slot`'s callers: `in`, `contains`, `index_of` (the needle is a FRESH K, so the identity
    # short-circuit cannot hide the dispatch).
    reset()
    arm()
    print(K(2) in xs)
    reset()
    arm()
    print(xs.contains(K(2)))
    reset()
    arm()
    print(xs.index_of(K(2)))
    # `unique` walks its own accumulator as well as the receiver snapshot.
    reset()
    arm()
    print(xs.unique().len())
    # Set algebra: `mine`/`other`/`out` are three Rust locals over the same orphaned elements.
    reset()
    sa := Set(xs)
    sb := Set(ys)
    reset()
    arm()
    print(sa.union(sb).len())
main()";
    assert_eq!(run_capture_stress(src), "true\ntrue\ntrue\ntrue\n1\n2\n2\n");
}

/// The Map and Set container arms of the same defect: a heap Map/Set is the one container a user
/// `eq` can empty IN PLACE while its entries are being walked out of a Rust-local snapshot.
#[test]
fn map_and_set_eq_snapshot_survives_an_eq_that_empties_the_sources() {
    let src = "\
sa: Set[K] = Set([])
sb: Set[K] = Set([])
ma: Map[K, List[str]] = {}
mb: Map[K, List[str]] = {}
armed: List[int] = []

struct K:
    n: int
    fn hash(self) -> int:
        return self.n
    fn eq(self, o: K) -> bool:
        if armed.len() > 0:
            armed.remove_at(0)
            sa.remove(K(1))
            sa.remove(K(2))
            sb.remove(K(1))
            sb.remove(K(2))
            ma.remove(K(1))
            ma.remove(K(2))
            mb.remove(K(1))
            mb.remove(K(2))
            junk := [str(self.n), str(o.n)]
        return self.n == o.n

fn reset():
    while armed.len() > 0:
        armed.remove_at(0)
    sa.add(K(1))
    sa.add(K(2))
    sb.add(K(1))
    sb.add(K(2))
    # LIST values, not str: a rebuilt list can never alias an interned program constant.
    ma[K(1)] = [str(1)]
    ma[K(2)] = [str(2)]
    mb[K(1)] = [str(1)]
    mb[K(2)] = [str(2)]

fn main():
    reset()
    armed.push(1)
    print(sa == sb)
    reset()
    armed.push(1)
    print(ma == mb)
main()";
    assert_eq!(run_capture_stress(src), "true\ntrue\n");
}

/// Same hazard via `sort_by` with an allocating comparator on an inline-temporary list.
#[test]
fn struct_sort_by_inline_temporary_survives_gc_stress() {
    let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c)]
        return self.c - o.c
    fn eq(self, o: M) -> bool:
        return self.c == o.c
fn make() -> List[M]:
    xs := []
    i := 0
    while i < 6:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    make().sort_by(fn(a: M, b: M) -> int: a.compare(b))
    print(\"ok\")
main()";
    assert_eq!(run_capture_stress(src), "ok\n");
}

/// An `Err` value propagated by `?` through a function boundary survives collection.
#[test]
fn value_propagated_by_try_survives() {
    let src = "\
fn d() -> Result[str]:
    return Err(str(99))
fn use() -> Result[str]:
    x := d()?
    return Ok(x)
fn main():
    match use():
        Ok(v): print(v)
        Err(e): print(\"got {e}\")
main()";
    assert_eq!(run_capture_stress(src), "got 99\n");
}

/// An allocation-heavy loop's garbage must be reclaimed: the live set stays bounded rather
/// than growing with the iteration count (threshold-driven GC, not stress mode).
#[test]
fn allocation_loop_is_bounded() {
    let src = "\
fn main():
    i := 0
    while i < 10000:
        x := [str(i)]
        i += 1
    print(i)
main()";
    let (out, live) = run_with(src, false);
    assert_eq!(out.unwrap(), "10000\n");
    // Without collection this would be ~20000+ live objects; the threshold GC keeps it small.
    assert!(
        live < 2000,
        "heap not bounded: {live} live objects after 10000 allocating iterations"
    );
}

/// Stress-mode collection must not change observable behavior on a feature-rich program.
#[test]
fn hello_chz_identical_under_gc_stress() {
    let expected = include_str!("../../examples/hello.expected");
    assert_eq!(
        run_capture_stress(include_str!("../../examples/hello.chz")),
        expected
    );
}

/// Stress vs. normal must agree on a program exercising structs, enums, closures, and match.
#[test]
fn stress_matches_normal_on_mixed_program() {
    let src = "\
struct Box:
    v: int
    fn get(self) -> int:
        return self.v
enum Opt:
    Has(int)
    Nope
fn pick(o: Opt) -> int:
    match o:
        Opt.Has(n): return n
        Opt.Nope: return -1
fn main():
    b := Box(7)
    add := fn(x: int) -> int: x + b.get()
    print(add(3))
    print(pick(Opt.Has(9)))
    print(pick(Opt.Nope))
    items := [str(1), str(2), str(3)]
    for s in items:
        print(s)
main()";
    let normal = run_capture(src).unwrap();
    assert_eq!(run_capture_stress(src), normal);
}

/// Concurrency C4 rooting: a `spawn`'s deep-copied args, a pending task's captured closure env,
/// and the values queued in a `Channel` / boxed in a `Shared` must all survive collections that
/// fire (under stress) between registration and the nursery's join. Each task allocates strings
/// so a missing root would corrupt the output (or panic on a dangling `GcRef`).
#[test]
fn spawn_pending_tasks_survive_gc_stress() {
    let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    base := str(100)
    parallel:
        spawn work(str(1), ch)
        spawn work(str(2), ch)
        spawn:
            ch.send(\"blk-{base}\")
    print(ch.len())
    for _ in 0..3:
        print(ch.recv())
main()";
    // Measured (20/20 identical on the M:N engine): `ch.len()` is deterministic (`parallel:`
    // joins all three spawns before the block continues), but which of the two racing `work`
    // sends and the block `spawn` lands first in `recv` order is a genuine race — compare
    // order-insensitively against the real measured line set, same shape as
    // `executor_tasks_survive_gc_stress`.
    let expected = "3\n1!\n2!\nblk-100\n";
    let normal = run_capture(src).unwrap();
    assert_same_lines(&normal, expected);
    assert_same_lines(&run_capture_stress(src), expected);
}

/// B3.1 regression: a core nested *inside* another core. The channel core is reachable ONLY
/// through the `Shared` box's wire value once `stash`'s local channel handle is gone — its queued
/// `"hello"` (a heap `Str` handle embedded in the channel core's wire queue) must still be traced
/// as a GC root, or `gc_stress` sweeps it and `recv` dangles. Pins that `collect_gcrefs` recurses
/// into nested cores.
#[test]
fn nested_core_contents_survive_gc_stress() {
    let src = "\
fn stash(s: Shared[Channel[str]]):
    ch := s.get()
    ch.send(\"hello\")
fn main():
    s := Shared(Channel[str]())
    stash(s)
    base := str(100)
    ch := s.get()
    print(ch.recv())
main()";
    let normal = run_capture(src).unwrap();
    assert_eq!(normal, "hello\n");
    assert_eq!(
        run_capture_stress(src),
        normal,
        "nested core contents must survive GC"
    );
}

/// `Shared` box + `update`'s re-entrant call survive GC stress (the box is re-rooted across the
/// nested user-fn call; the boxed list's elements stay reachable through collections).
#[test]
fn shared_box_survives_gc_stress() {
    let src = "\
fn appended(xs: List[str], v: str) -> List[str]:
    xs.push(v)
    return xs
fn push_one(s: Shared[List[str]], v: str):
    s.update(fn(xs): appended(xs, v))
fn main():
    s := Shared([str(0)])
    parallel:
        spawn push_one(s, str(1))
        spawn push_one(s, str(2))
    print(s.get())
main()";
    // The two spawns race for `s`'s lock under the pooled engine (the only engine now `--serial` is
    // gone), so which of "1"/"2" lands first is NOT guaranteed — measured: 300 stress-mode runs under
    // `RUST_TEST_THREADS=4` contention produced BOTH `['0','1','2']` and `['0','2','1']`, so an
    // order-exact comparison here is a real (if rare) flake, not a guaranteed invariant. What IS
    // guaranteed, and what this test is actually for, is that every pushed element survives GC — so
    // compare as a sorted multiset, order-independent.
    let out = run_capture_stress(src);
    let mut got: Vec<&str> = out
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .split(", ")
        .collect();
    got.sort_unstable();
    assert_eq!(
        got,
        vec!["'0'", "'1'", "'2'"],
        "all three pushes must survive GC (order not guaranteed)"
    );
}

/// Executor: a submitted task closure must survive GC firing between the `submit` and the job's
/// completion, and across each task's re-entrant call. Under eager `submit` the closure has already
/// crossed into its own worker heap by the time `submit` returns, so this covers the wire crossing as
/// well as the heap root. Each task allocates a string into a Channel — a missing root would corrupt
/// the output or dangle a `GcRef`.
/// Eager `Executor` + GC stress on the M:N engine, where the dispatch actually happens.
///
/// The specific hazard: `submit` rebuilds the closure into a fresh worker heap, and a closure that
/// CAPTURED the executor puts an `Obj::Executor` over the SAME core into that heap — whose GC mark arm
/// (`Heap`, the `Obj::Executor` case) takes `core.inner.lock()`. `std::sync::Mutex` is not reentrant,
/// so doing that rebuild while the submitting thread holds `core.inner` self-deadlocks. `submit`
/// therefore prepares the worker with NO executor lock held and takes the lock only for the
/// allocation-free reserve-and-dispatch. Under `gc_stress` every allocation collects, so this is the
/// shape that would hang if that ordering is ever reintroduced.
///
/// Watchdogged: the regression is a HANG, which would otherwise stall the whole test binary.
#[test]
fn eager_executor_self_capturing_closure_survives_gc_stress_parallel() {
    let src = "\
fn main():
    ch := Channel[str]()
    ex := Executor()
    ex.submit(fn(): ch.send(\"saw {ex}\"))
    ex.shutdown()
    print(ch.recv())
main()";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_stress(src));
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("submit must not hold the executor lock across the worker rebuild (deadlock)");
    // `pending=1` is the job counting ITSELF: it is outstanding while it runs, and `Display` now sums
    // the serial queue with the eager in-flight count instead of reading the (always-empty on M:N)
    // queue alone. Reading `0` here would mean the display is lying about running work.
    assert_eq!(got, "saw Executor(pending=1)\n");
}

#[test]
fn executor_tasks_survive_gc_stress() {
    let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    ex := Executor()
    ex.submit(fn(): work(str(1), ch))
    ex.submit(fn(): work(str(2), ch))
    ex.shutdown()
    for _ in 0..2:
        print(ch.recv())
main()";
    // Two Executor jobs race to send on a shared channel, so which one lands first is a genuine
    // M:N race (not a rooting bug) — compare order-insensitively.
    let normal = run_capture(src).unwrap();
    assert_same_lines(&run_capture_stress(src), &normal);
}

/// `snapshot_value` itself is pure alloc (no GC), but each insert path interleaves snapshots with
/// the ELEMENTS' re-entrant `hash()` (which allocates junk here → GC can fire). A snapshot made for
/// one element must be rooted (stack slot for NewMap/NewSet + map/set index-set; operand stack for
/// Map()/Set() ctors) before the NEXT element's `hash()` runs, else it is collected and reads/algebra
/// go wrong. Mutate the originals after insert to prove the collection holds SNAPSHOTS, not aliases.
#[test]
fn map_struct_key_snapshot_survives_gc_stress() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n), str(self.n + 1)]
        return self.n
fn main():
    a := K(1)
    b := K(2)
    m: Map[K, str] = {a: str(1), b: str(2)}   # NewMap literal
    s := {a, b, K(3)}                          # NewSet literal
    a.n = 90                                   # mutate originals AFTER insert
    b.n = 91
    mi := Map([(K(4), str(4)), (K(5), str(5))])  # Map(iterable)
    si := Set([K(6), K(7), K(6)])                # Set(iterable)
    c := K(8)
    s.add(c)
    c.n = 92
    ks := \"\"
    for k in m:
        ks = ks + str(k.n)
    print(ks)                                  # 12 (snapshots, not 90/91)
    print(s.difference({K(1), K(2), K(3), K(8)}).len())   # 0 (all present as snapshots)
    print(mi.keys()[0].n)                      # 4
    print(si.len())                            # 2
main()";
    assert_eq!(run_capture_stress(src), "12\n0\n4\n2\n");
}

/// A boxed float (its own Float tag — 8B-`Value` milestone) held ONLY inside a list must be traced
/// by `children()`/`gc_roots` (via `Value::child_gcref`, which must match the Float tag as well as
/// the Obj tag) or it is swept while live — a silent use-after-free surfacing only under GC pressure.
/// Build a list of floats, stress-collect between allocations, then read them back.
#[test]
fn boxed_float_in_list_survives_collect() {
    let src = "\
fn main():
    xs := [1.5, 2.5, 3.5]
    junk := [str(1), str(2), str(3), str(4)]
    more := [str(5), str(6)]
    print(xs[0] + xs[1] + xs[2])
main()";
    assert_eq!(run_capture_stress(src), "7.5\n");
}

/// A boxed big-int (`Obj::BigInt`, > 2^62) held only inside a list survives GC stress too.
#[test]
fn boxed_bigint_in_list_survives_collect() {
    let src = "\
fn main():
    xs := [4611686018427387905, 4611686018427387906]
    junk := [str(1), str(2), str(3)]
    print(xs[0])
    print(xs[1])
main()";
    assert_eq!(
        run_capture_stress(src),
        "4611686018427387905\n4611686018427387906\n"
    );
}

/// Regression: RwShared[Set]/[Map] `contains`/`has`/`get_key` on a TEMPORARY receiver whose
/// element/key is a struct with a user `hash`. `hash_value` dispatches that hash (re-enters the VM,
/// allocates → GC under stress); the receiver handle `h` was popped off the operand stack at
/// dispatch (`recv = self.pop()`), so unless it is rooted across the hash the collector frees it and
/// the following `rwshared_core(h)` hits a freed slot (use-after-free / `unreachable!()` panic).
/// Fixed by hashing via `hash_key_rooted(k, &[Value::obj(h), k], span)` (mirrors arith.rs:913/921).
/// Chained calls (`make_*().probe(...)`) keep the RwShared reachable ONLY through the popped `recv`.
#[test]
fn rwshared_probe_struct_key_rooted_across_hash() {
    let src = "\
import std.concurrency
struct P:
    x: int
    fn hash(self) -> int:
        pad := [self.x, self.x + 1]   # allocate inside hash → GC mid-probe under stress
        return pad[0] * 31

fn make_set() -> RwShared[Set[P]]:
    return RwShared(Set([P(1), P(2), P(3)]))

fn make_map() -> RwShared[Map[P, int]]:
    return RwShared({P(1): 10, P(2): 20})

fn main():
    print(make_set().contains(P(2)))
    print(make_map().has(P(1)))
    match make_map().get_key(P(2)):
        Some(v): print(v)
        None: print(\"none\")
main()";
    assert_eq!(run_capture_stress(src), "true\ntrue\n20\n");
}

/// W6-7 — the GC now SHORT-CIRCUITS a core whose cached summary says its wire payload holds no
/// `Handle` and no nested core. That memo is only sound if EVERY store refreshes it, and the
/// `Shared`/`RwShared`/`Atomic` payload is *replaced* (`set`/`update`/`write`/`store`/`exchange`/
/// `cas`/`add`), not mutated in place. Under GC stress a stale `CLEAN` would free a value that is
/// live only through the core. Handle-bearing payloads (closures) and nested cores are the arms
/// that must stay DIRTY.
#[test]
fn gc_stress_values_parked_in_cores() {
    let src = "\
import std.concurrency

struct Bag:
    xs: List[str]

fn main():
    # REPLACING stores on every single-value core, interleaved with allocation.
    s := Shared(Bag([str(1)]))
    for i in range(20):
        s.update(fn(b): Bag(b.xs + [str(i)]))
        junk := [str(i), str(i + 1)]
    print(s.get().xs.len())

    rw := RwShared([str(0)])
    for i in range(20):
        rw.write(fn(xs): xs + [str(i)])
        junk := [str(i)]
    print(rw.get().len())

    a := Atomic([str(9)])
    for i in range(10):
        a.store([str(i), str(i)])
        junk := [str(i)]
    print(a.load()[0])

    # A payload that DOES root the heap: a closure with captures, parked in a channel.
    ch := Channel[fn() -> str]()
    tag := str(42)
    ch.send(fn() -> str: tag)
    filler := []
    for i in range(30):
        filler.push([str(i)])
    f := ch.recv()
    print(f())

    # A NESTED core inside a core: the outer must stay DIRTY so the inner's contents stay rooted.
    inner := Channel[str]()
    inner.send(str(7))
    outer := Shared(inner)
    for i in range(30):
        junk := [str(i)]
    print(outer.get().recv())

    # An Executor queue holding submitted closures across a stressed run.
    ex := Executor(1)
    note := str(5)
    ex.submit(fn(): print(note))
    for i in range(20):
        junk := [str(i)]
    ex.shutdown()
main()";
    assert_eq!(run_capture_stress(src), "21\n21\n9\n42\n7\n5\n");
}

/// A container crossing the airlock must be rebuilt at **exact** capacity.
///
/// `from_wire`'s container arms used to be `items.into_iter().map(…).collect()`, which Rust
/// specializes into an **in-place** collect: the destination element (`Value`, 8 B) is smaller than
/// the source (`WireValue`, 176 B), so the source `Vec`'s allocation is reused and the rebuilt
/// `Vec<Value>` inherits its capacity — 22× the elements it holds. Measured before the fix: a
/// 200 000-int list arrived on the far heap as `len = 200 000, capacity = 4 400 000` — a 35.2 MB
/// `Obj::List` carrying 1.6 MB of data, and 50 such `spawn`s peaked at 3.45 GB (203 MB after).
///
/// `len` is IDENTICAL either way and every value-level assertion passes, so `capacity` is the only
/// thing that can see this — which is why the bug survived the whole behavioural suite.
#[test]
fn a_crossed_container_rebuilds_at_exact_capacity() {
    const N: usize = 4096;
    let span = Span::RUNTIME;
    let mut src = Vm::new(Arc::new(crate::vm::tests::empty_program()));

    // A list and a tuple — two of the eight arms now sharing one rebuild helper.
    let items: Vec<Value> = (0..N as i64).map(Value::int).collect();
    let list = Value::obj(src.heap.alloc(Obj::List(items.clone())));
    let tuple = Value::obj(src.heap.alloc(Obj::Tuple(items)));

    let iter = Value::obj(src.heap.alloc(Obj::Iter {
        items: (0..N as i64).map(Value::int).collect(),
        pos: 0,
    }));

    let mut dst = Vm::new(Arc::clone(&src.program));
    for (label, v) in [("List", list), ("Tuple", tuple), ("Iter", iter)] {
        let w = src.to_wire_at(v, span).expect("pure data crosses");
        let rebuilt = dst.from_wire(w);
        let h = rebuilt.as_obj().expect("rebuilds to a heap object");
        let (len, cap) = match dst.heap.get(h) {
            Obj::List(xs) | Obj::Tuple(xs) | Obj::Iter { items: xs, .. } => {
                (xs.len(), xs.capacity())
            }
            other => panic!("{label} rebuilt as the wrong object: {other:?}"),
        };
        assert_eq!(len, N, "{label}: every element must survive the crossing");
        assert_eq!(
            cap,
            len,
            "{label}: rebuilt at {cap} capacity for {len} elements — the wire buffer was retained \
             in place (expected exactly {len}; the in-place-collect regression gives {}×)",
            std::mem::size_of::<crate::vm::wire::WireValue>() / std::mem::size_of::<Value>()
        );
    }
}

/// `deep_clone_all` — the OTHER wire→`Value` list rebuild — must also land at exact capacity.
///
/// Sibling of [`a_crossed_container_rebuilds_at_exact_capacity`], and the reason it exists as its own
/// test: the first cut of that fix converted `from_wire_memo`'s eight container arms and left this
/// one — plus `rebuild_ready`'s `Lowered` arms — on the old `into_iter().map(…).collect()`. Both feed
/// a DURABLE `Obj::Closure { captured }` (`sched.rs`, the `parallel:`/block-spawn path), so `spawn`
/// with a capturing closure still retained the wire buffer at 22–24× while the suite stayed green.
/// One arm of an N-way set is not the set.
#[test]
fn deep_clone_all_rebuilds_at_exact_capacity() {
    const N: usize = 4096;
    let span = Span::RUNTIME;
    let mut vm = Vm::new(Arc::new(crate::vm::tests::empty_program()));
    let vs: Vec<Value> = (0..N as i64).map(Value::int).collect();

    let (out, _cells) = vm.deep_clone_all(vs, span).expect("scalars deep-clone");

    assert_eq!(out.len(), N, "every value must survive the clone");
    assert_eq!(
        out.capacity(),
        out.len(),
        "deep_clone_all returned {} capacity for {} values — the intermediate Vec<WireValue> was \
         retained in place by an in-place collect",
        out.capacity(),
        out.len()
    );
}

/// `min`/`max`/`min_by`/`max_by` now return `Some(<extremum>)`, so the winning element travels one
/// hop further than it used to. When a re-entrant `compare`/key extractor has REMOVED that element
/// from the live receiver, the GC-rooted snapshot is the ONLY thing keeping it alive across the
/// re-entry's collections, and the returned `Some` then carries an element the source no longer
/// contains. **Verified to discriminate**: swapping either helper's `push(<snapshot>)` for a
/// non-rooting push turns this into *dangling GcRef (object was collected while still reachable)*.
/// (The `Some` allocation itself cannot collect — `Heap::alloc` never GCs; collection happens only
/// at `run_until`'s instruction boundaries, which is why the wrap-before-unroot order in
/// `list_reduce_extreme`/`list_min_max_by` is belt-and-braces rather than load-bearing.)
#[test]
fn min_max_some_payload_survives_gc() {
    // `compare` shrinks the module-global receiver, so by the time the scan finishes the winner
    // (`P(str(1), 1)`) is reachable ONLY through the rooted snapshot.
    let min_src = "\
struct P:
    tag: str
    k: int
    fn compare(self, o: P) -> int:
        pts.remove_at(0)
        return self.k - o.k
    fn eq(self, o: P) -> bool:
        return self.k == o.k
pts: List[P] = [P(str(3), 3), P(str(1), 1), P(str(2), 2)]
fn main():
    match pts.min():
        Some(p): print(p.tag)
        None: print(\"none\")
main()";
    assert_eq!(run_capture_stress(min_src), "1\n");
    // …and the `min_by` path, whose key extractor does the shrinking.
    let by_src = "\
struct Q:
    tag: str
    k: int
qs: List[Q] = [Q(str(3), 3), Q(str(1), 1), Q(str(2), 2)]
fn key(q: Q) -> int:
    qs.remove_at(0)
    return q.k
fn main():
    match qs.min_by(key):
        Some(q): print(q.tag)
        None: print(\"none\")
main()";
    assert_eq!(run_capture_stress(by_src), "1\n");
}
