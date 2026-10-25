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
fn make() -> fn() -> str:
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
    fn compare(self, other: M) -> int:
        junk := [str(self.c), str(other.c)]
        return self.c - other.c
    fn eq(self, other: M) -> bool:
        return self.c == other.c
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

/// TICKET-146: a tuple ordering walk re-enters a user `compare` that allocates (GC mid-walk). The
/// list being sorted, each tuple, and an inline-temporary operand pair must all stay rooted.
#[test]
fn tuple_ordering_survives_gc_stress() {
    let src = "\
struct M:
    c: int
    fn compare(self, other: M) -> int:
        junk := [str(self.c), str(other.c)]
        return self.c - other.c
    fn eq(self, other: M) -> bool:
        return self.c == other.c
fn make() -> List[(M, int)]:
    xs := []
    i := 0
    while i < 8:
        xs.push((M((i * 5) % 7), i))
        i = i + 1
    return xs
fn main():
    xs := make()
    xs.sort()
    out := \"\"
    for t in xs:
        out = out + str(t.0.c)
    print(out)
    make().sort()
    print((M(1), 2) < (M(1), 3))
    print((M(2), 2) < (M(1), 3))
main()";
    assert_eq!(run_capture_stress(src), "00123456\ntrue\nfalse\n");
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
    fn eq(self, other: M) -> bool:
        junk := [str(self.c), str(other.c)]
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
    for k in m.keys():
        out = out + m[k]
    print(out)
    print(m.contains(K(3)))
    print(m.get(K(5)))
    print(make_map().get(K(2)))   # inline-temporary receiver
    print(m.remove(K(0)))
    print(m.len())
main()";
    assert_eq!(run_capture_stress(src), "01234567\ntrue\n5\n2\n0\n7\n");
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
    a.insert(K(3))
    a.insert(K(4))
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
/// (`make_set().contains(..)`) are the sharpest case: nothing but the rooting keeps them alive.
#[test]
fn set_struct_eq_survives_gc_stress() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        return self.n % 3
    fn eq(self, other: K) -> bool:
        junk := [str(self.n), str(other.n)]
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
    a.insert(K(3))
    a.insert(K(4))
    print(a.len())
    b := Set([K(3), K(4), K(5)])
    print(a.union(b).len(), a.intersection(b).len(), a.difference(b).len())
    print((a | b).len(), (a ^ b).len())
    print(make_set().contains(K(2)))              # inline-temporary receiver
    print(K(3) in make_set())
    print(make_list().index_of(K(3)), make_list().unique().len())
    print(K(2) in make_list())
    m := make_map()
    print(m.contains(K(4)), m.get(K(5)), make_map().get(K(2)), m[K(1)])
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
        "3\n4\n5 2 2\n5 3\ntrue\ntrue\n3 3\ntrue\ntrue 5 2 1\n6 0 5\n6\n1 1\n1\n3\n"
    );
}

/// M23 Task 4 — the `RwShared` read-view probes (`contains`/`has`/`get_key`) reconstruct the stored
/// element/key FROM THE WIRE before comparing it, so that reconstruction is a fresh object reachable
/// only from a Rust local while a now-re-entrant `eq` runs. `Atomic.cas` is deliberately absent: it
/// compares under `core.v.lock()` and stays on the structural path (see `netio.rs`).
#[test]
fn rwshared_struct_eq_survives_gc_stress() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
struct K:
    n: int
    fn hash(self) -> int:
        return self.n % 3
    fn eq(self, other: K) -> bool:
        junk := [str(self.n), str(other.n)]
        return junk[0] == junk[1]
fn main():
    # LIST values, not str: a freshly-rebuilt list can never alias an interned program constant,
    # so an unrooted `v` really is collectable while `eq` runs.
    m: Map[K, List[str]] = {K(1): [str(1)], K(2): [str(2), str(3)]}
    box := ConcurrentMap(RwShared(m))
    print(box.contains(K(1)), box.contains(K(9)), box.get(K(2)), box.get(K(9)))
    s := ConcurrentSet(RwShared(Set([K(1), K(2)])))
    print(s.contains(K(2)), s.contains(K(7)))
main()";
    assert_eq!(
        super::golden_tests::vm_run_file_stress(src, crate::native::HostConfig::default()),
        "true false ['2', '3'] None\ntrue false\n"
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
    fn eq(self, other: K) -> bool:
        # ONE mutation per armed walk: EMPTY the two source lists in place, so the only remaining
        # reference to the elements the caller already snapshotted is that Rust-local snapshot —
        # then allocate, so the stress collector actually runs while the walk still holds them.
        if armed.len() > 0:
            armed.remove(0)
            while xs.len() > 0:
                xs.remove(0)
            while ys.len() > 0:
                ys.remove(0)
            junk := [str(self.n), str(other.n)]
        return self.n == other.n

fn reset():
    while armed.len() > 0:
        armed.remove(0)
    while xs.len() > 0:
        xs.remove(0)
    while ys.len() > 0:
        ys.remove(0)
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
    fn eq(self, other: K) -> bool:
        if armed.len() > 0:
            armed.remove(0)
            sa.remove(K(1))
            sa.remove(K(2))
            sb.remove(K(1))
            sb.remove(K(2))
            ma.remove(K(1))
            ma.remove(K(2))
            mb.remove(K(1))
            mb.remove(K(2))
            junk := [str(self.n), str(other.n)]
        return self.n == other.n

fn reset():
    while armed.len() > 0:
        armed.remove(0)
    sa.insert(K(1))
    sa.insert(K(2))
    sb.insert(K(1))
    sb.insert(K(2))
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
    fn compare(self, other: M) -> int:
        junk := [str(self.c)]
        return self.c - other.c
    fn eq(self, other: M) -> bool:
        return self.c == other.c
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
fn d() -> str!:
    return !str(99)
fn use() -> str!:
    x := d()?
    return ?x
fn main():
    match use():
        ?v: print(v)
        !e: print(\"got {e}\")
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
    let src = "import Shared from std.concurrency\n\
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
    let src = "import Shared from std.concurrency\n\
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
    let src = "import Executor from std.concurrency\n\
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
    let src = "import Executor from std.concurrency\n\
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
    s.insert(c)
    c.n = 92
    ks := \"\"
    for k in m.keys():
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
import RwShared from std.concurrency
import ConcurrentMap, ConcurrentSet from std.concurrency.collection
struct P:
    x: int
    fn hash(self) -> int:
        pad := [self.x, self.x + 1]   # allocate inside hash → GC mid-probe under stress
        return pad[0] * 31

fn make_set() -> ConcurrentSet[P]:
    return ConcurrentSet(RwShared(Set([P(1), P(2), P(3)])))

fn make_map() -> ConcurrentMap[P, int]:
    return ConcurrentMap(RwShared({P(1): 10, P(2): 20}))

fn main():
    print(make_set().contains(P(2)))
    print(make_map().contains(P(1)))
    match make_map().get(P(2)):
        ?v: print(v)
        None: print(\"none\")
main()";
    assert_eq!(
        super::golden_tests::vm_run_file_stress(src, crate::native::HostConfig::default()),
        "true\ntrue\n20\n"
    );
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
import Atomic, Executor, RwShared, Shared from std.concurrency

struct Bag:
    xs: List[str]

fn main():
    # REPLACING stores on every single-value core, interleaved with allocation.
    s := Shared(Bag([str(1)]))
    for i in 0..20:
        s.update(fn(b): Bag(b.xs + [str(i)]))
        junk := [str(i), str(i + 1)]
    print(s.get().xs.len())

    rw := RwShared([str(0)])
    for i in 0..20:
        rw.write(fn(xs): xs + [str(i)])
        junk := [str(i)]
    print(rw.get().len())

    a := Atomic([str(9)])
    for i in 0..10:
        a.store([str(i), str(i)])
        junk := [str(i)]
    print(a.load()[0])

    # A payload that DOES root the heap: a closure with captures, parked in a channel.
    ch := Channel[fn() -> str]()
    tag := str(42)
    ch.send(fn() -> str: tag)
    filler := []
    for i in 0..30:
        filler.push([str(i)])
    f := ch.recv()
    print(f())

    # A NESTED core inside a core: the outer must stay DIRTY so the inner's contents stay rooted.
    inner := Channel[str]()
    inner.send(str(7))
    outer := Shared(inner)
    for i in 0..30:
        junk := [str(i)]
    print(outer.get().recv())

    # An Executor queue holding submitted closures across a stressed run.
    ex := Executor(1)
    note := str(5)
    ex.submit(fn(): print(note))
    for i in 0..20:
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
    let list = Value::obj(src.heap.alloc(Obj::List(items.clone().into())));
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
            Obj::List(xs) | Obj::Iter { items: xs, .. } => (xs.len(), xs.capacity()),
            Obj::Tuple(xs) => (xs.len(), xs.capacity()),
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
/// (The `Some` allocation itself cannot collect — `Heap::alloc` never GCs; the only collection
/// boundary THIS PATH can reach is `run_until`'s instruction boundary (the other collection site,
/// `Vm::sample_mem_cap`, runs once per task start — before a method call runs, not during it, so
/// it is unreachable from here), which is why the wrap-before-unroot order in
/// `list_reduce_extreme`/`list_min_max_by` is belt-and-braces rather than load-bearing.)
#[test]
fn min_max_some_payload_survives_gc() {
    // `compare` shrinks the module-global receiver, so by the time the scan finishes the winner
    // (`P(str(1), 1)`) is reachable ONLY through the rooted snapshot.
    let min_src = "\
struct P:
    tag: str
    k: int
    fn compare(self, other: P) -> int:
        pts.remove(0)
        return self.k - other.k
    fn eq(self, other: P) -> bool:
        return self.k == other.k
pts: List[P] = [P(str(3), 3), P(str(1), 1), P(str(2), 2)]
fn main():
    match pts.min():
        ?p: print(p.tag)
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
    qs.remove(0)
    return q.k
fn main():
    match qs.min_by(key):
        ?q: print(q.tag)
        None: print(\"none\")
main()";
    assert_eq!(run_capture_stress(by_src), "1\n");
}

/// TICKET-246 — a `Shared` state lives in the box's own heap, rooted at `stack[0]` of the box
/// context. Under gc-stress a collection runs at every instruction INSIDE the closure, with the
/// caller's heap parked: the state, the closure, its captures and the slot-door values must all
/// survive, and the caller's own values must be intact after the call. The state is built
/// across three updates, then read back three ways.
#[test]
fn shared_state_survives_collections_inside_its_closure() {
    let src = "\
import Shared from std.concurrency
struct Acc:
    log: List[str]
    n: int
fn tag(i: int) -> str:
    return \"t\" + str(i)
fn add(a: Acc, s: str) -> Acc:
    junk := [str(1), str(2), str(3)]
    a.log.push(s + str(junk.len()))
    a.n += 1
    return a
fn main():
    keep := [str(7), str(8)]
    s := Shared(Acc([str(0)], 0))
    for i in 0..3:
        pre := \"p\" + str(i)
        s.update(fn(a): add(a, pre + tag(i)))
    print(s.read(fn(a): a.log.len() + a.n))
    print(s.get().log)
    print(s)
    s.set(Acc([str(9)], 9))
    print(s.read(fn(a): a.log[0] + str(a.n)))
    print(keep)
main()";
    assert_eq!(
        run_capture_stress(src),
        "7\n['0', 'p0t03', 'p1t13', 'p2t23']\n\
         Shared(Acc(log=['0', 'p0t03', 'p1t13', 'p2t23'], n=3))\n99\n['7', '8']\n"
    );
}

/// TICKET-246 — the `RwShared` twin under gc-stress: a state built across three `write`s inside
/// the box heap, then every reader copy (`get`, `read`, display, the collection views and
/// writers) taken while the READER's heap collects at each safepoint. A copy decoded into the
/// reader's heap is rooted only by what the reader does next, so a missing root here is a freed
/// slot read back as a wrong value.
#[test]
fn rwshared_state_and_reader_copies_survive_collections() {
    let src = "\
import RwShared from std.concurrency
# The std-internal primitives are called directly: this harness compiles one module.
struct Acc:
    log: List[str]
    n: int
struct K:
    id: str
    fn hash(self) -> int:
        junk := [str(1), str(2)]
        return self.id.len() + junk.len()
    fn eq(self, other: K) -> bool:
        junk := [str(3)]
        return self.id == other.id and junk.len() == 1
fn add(a: Acc, s: str) -> Acc:
    junk := [str(1), str(2), str(3)]
    a.log.push(s + str(junk.len()))
    a.n += 1
    return a
fn main():
    keep := [str(7), str(8)]
    w := RwShared(Acc([str(0)], 0))
    for i in 0..3:
        pre := \"p\" + str(i)
        w.write(fn(a): add(a, pre))
    print(w.read(fn(a): a.log.len() + a.n))
    print(w.get().log)
    print(w)
    m: RwShared[Map[K, List[str]]] = RwShared({})
    xs: RwShared[List[List[str]]] = RwShared([])
    st: RwShared[Set[K]] = RwShared(Set[K]())
    for i in 0..4:
        m._set_key(K(\"k\" + str(i)), [str(i), str(i + 1)])
        xs._push([str(i)])
        st._add(K(\"k\" + str(i % 2)))
    print(m._get_key(K(\"k2\")), m._has(K(\"k9\")), m._len())
    print(m._get_or_insert(K(\"k1\"), [str(0)]), m._get_or_insert(K(\"k7\"), [str(7)]))
    _ := m._remove_key(K(\"k0\"))
    print(xs._at(1), xs._slice(1..3), xs._len())
    print(st._contains(K(\"k1\")), st._len(), m._len())
    st._remove(K(\"k1\"))
    print(st.get().len())
    print(keep)
main()";
    assert_eq!(
        run_capture_stress(src),
        "7\n['0', 'p03', 'p13', 'p23']\n\
         RwShared(Acc(log=['0', 'p03', 'p13', 'p23'], n=3))\n\
         ['2', '3'] false 4\n['1', '2'] ['7']\n\
         ['1'] [['1'], ['2']] 4\ntrue 2 4\n1\n['7', '8']\n"
    );
}

/// TICKET-246 — a global read through the box's slot door is cached for the call, and the cache
/// is a GC root of the box heap (`BoxFrame::roots`). The closure reads one heap-allocated `str`
/// global twice with nothing else holding it and allocations in between. Without the root the
/// first copy is swept, its slot is re-handed, and the second read returns another object.
#[test]
fn box_slot_door_cache_is_rooted_across_collections() {
    let src = "\
import Shared from std.concurrency
KS: str = \"abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz\"
fn probe(x: int) -> int:
    n := KS.len()
    junk := [str(x), str(x + 1), str(x + 2)]
    more := [str(n), str(n + 1)]
    m := KS.len()
    return n + m + junk.len() + more.len()
fn main():
    s := Shared(0)
    s.update(probe)
    print(s.get())
    print(s.read(probe))
main()";
    assert_eq!(run_capture_stress(src), "129\n129\n");
}

/// TICKET-246 — two boxes deep under gc-stress: the inner closure reads an aggregate module
/// global (the view is seated through the caller chain while every heap but the innermost is
/// parked) and allocates; both states and the root's values survive.
#[test]
fn nested_shared_calls_survive_collections_at_every_depth() {
    let src = "\
import Shared from std.concurrency
G: List[str] = [str(1), str(2)]
fn main():
    a: Shared[List[str]] = Shared([str(0)])
    b: Shared[List[str]] = Shared([])
    fn inner(ys: List[str]) -> List[str]:
        ys.push(G[1] + str(G.len()))
        return ys
    fn outer(xs: List[str]) -> List[str]:
        b.update(inner)
        xs.push(str(b.read(fn(ys): ys.len())) + G[0])
        return xs
    a.update(outer)
    a.update(outer)
    print(a.get())
    print(b.get())
    print(G)
main()";
    assert_eq!(
        run_capture_stress(src),
        "['0', '11', '21']\n['22', '22']\n['1', '2']\n"
    );
}

/// M2.2 — run `src` (a push loop past a container's capacity, then a print of it) and check:
/// the grows retired the old blocks on the run's retire lists, a collection frees them all, and
/// the container reads back intact (the print, and the same program under gc-stress).
fn growth_retires_until_collect(src: &str, want: &str) {
    growth_retires_index_until_collect(src, want, 0);
}

/// [`growth_retires_until_collect`], and at least `min_index` of the retired blocks are old
/// index tables of a map or a set.
fn growth_retires_index_until_collect(src: &str, want: &str, min_index: usize) {
    use super::rvec::{test_counts, test_serial, test_track};
    let _g = test_serial();
    let owned = src.to_string();
    let (out, retired, index, left, freed) = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            test_track();
            let mut vm = Vm::new(Arc::new(test_program(&owned).expect("compiles")));
            // The counts below are what the run's grows left retired: an env
            // `CHEZZI_GC_STRESS` collect inside the run would free them first. The stressed run
            // is the `run_capture_stress` check at the end.
            vm.gc_stress = false;
            let result = vm.run();
            vm.finish_run(result).expect("runs");
            let (own, core) = vm.heap.retired_len();
            let index = vm.heap.retired_index_blocks();
            let before = test_counts().1;
            vm.gc_collect();
            let freed = test_counts().1 - before;
            let (own2, core2) = vm.heap.retired_len();
            (captured(vm.out), own + core, index, own2 + core2, freed)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked");
    assert_eq!(out, want);
    assert!(
        retired >= 3,
        "each grow retired its old block, got {retired}"
    );
    assert!(
        retired - index >= 1,
        "a grow retired an old entry block, got {retired} blocks, {index} index blocks"
    );
    assert!(
        index >= min_index,
        "an index grow retired the old table, got {index} < {min_index} index blocks"
    );
    assert_eq!(left, 0, "the collection freed every retired block");
    assert!(
        freed >= retired,
        "the collection freed {freed} < {retired} blocks"
    );
    assert_eq!(run_capture_stress(src), want);
}

#[test]
fn list_growth_retires_until_collect() {
    let src = "\
fn main():
    xs: List[int] = [0]
    for i in 1..100:
        xs.push(i)
    print(xs.len(), xs[0], xs[50], xs[99])
main()";
    growth_retires_until_collect(src, "100 0 50 99\n");
}

#[test]
fn bytearray_growth_retires_until_collect() {
    let src = "\
fn main():
    b := bytearray(b\"a\")
    for i in 1..100:
        b.push(i)
    print(b.len(), b[0], b[50], b[99])
main()";
    growth_retires_until_collect(src, "100 97 50 99\n");
}

/// M2.3 — a map insert loop past the capacity retires the old entry blocks AND the old index
/// tables; a collection frees them, and the map reads back intact.
#[test]
fn map_insert_growth_retires_index_and_entries() {
    let src = "\
fn main():
    m: Map[int, int] = {0: 0}
    for i in 1..100:
        m[i] = i * 2
    print(m.len(), m[0], m[50], m[99])
main()";
    growth_retires_index_until_collect(src, "100 0 100 198\n", 3);
}

/// M2.9 — a grow loop on one map that allocates no other object retires about 5 MB of entry
/// blocks and index tables and charges about 10 MB in all, under the 16 MiB byte goal of an
/// empty heap. The retired bytes alone start collections (their goal is the live bytes of the
/// last sweep, at least 1 MiB), and those collections free retired blocks during the run.
#[test]
fn map_grow_loop_collects_on_retired_bytes() {
    use super::rvec::{test_counts, test_serial, test_track};
    let _g = test_serial();
    let src = "\
fn main():
    m: Map[int, int] = {}
    i := 0
    while i < 100000:
        m[i] = i
        i += 1
    print(m.len())
main()";
    let (out, stops, freed) = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            test_track();
            let mut vm = Vm::new(Arc::new(test_program(src).expect("compiles")));
            vm.gc_stress = false;
            let before = test_counts().1;
            let result = vm.run();
            let freed = test_counts().1 - before;
            vm.finish_run(result).expect("runs");
            (captured(vm.out), vm.gc_coord.stop_stats().0, freed)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked");
    assert_eq!(out, "100000\n");
    assert!(stops >= 1, "the retired bytes started no collection");
    assert!(freed >= 1, "no retired block was freed during the run");
}

/// M2.3 — the Set twin of [`map_insert_growth_retires_index_and_entries`].
#[test]
fn set_insert_growth_retires_index_and_entries() {
    let src = "\
fn main():
    s: Set[int] = Set([0])
    for i in 1..100:
        s.insert(i)
    print(s.len(), 0 in s, 50 in s, 99 in s, 100 in s)
main()";
    growth_retires_index_until_collect(src, "100 true true true false\n", 3);
}

/// M2.3 — keys whose `hash()` is one constant probe one cluster of the index table; inserts past
/// the table's capacity retire the old index block, and every key reads back.
#[test]
fn constant_hash_growth_retires_old_index() {
    let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        return 7
    fn eq(self, other: K) -> bool:
        return self.n == other.n

fn main():
    s: Set[K] = Set([K(0)])
    for i in 1..20:
        s.insert(K(i))
    print(s.len(), K(0) in s, K(19) in s, K(20) in s)
main()";
    growth_retires_index_until_collect(src, "20 true true false\n", 3);
}

/// M2.3 — a heap probe walks its candidates once per restart: inserting and finding `N` keys
/// whose `hash()` is one constant reads O(N^2) index slots in all, not O(N^3). The user `eq` makes
/// every compare re-entrant, so this is the rooted, re-validating probe path.
#[test]
fn constant_hash_probe_reads_each_candidate_once() {
    const N: usize = 300;
    let src = format!(
        "\
struct K:
    n: int
    fn hash(self) -> int:
        return 7
    fn eq(self, other: K) -> bool:
        return self.n == other.n

fn main():
    s: Set[K] = Set([K(0)])
    for i in 1..{N}:
        s.insert(K(i))
    found := 0
    for i in 0..{N}:
        if K(i) in s:
            found += 1
    print(s.len(), found)
main()"
    );
    let (out, reads) = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let before = super::map_data::slot_reads();
            let mut vm = Vm::new(Arc::new(test_program(&src).expect("compiles")));
            assert!(vm.eq_may_reenter(), "the user eq is the hook");
            let result = vm.run();
            vm.finish_run(result).expect("runs");
            (captured(vm.out), super::map_data::slot_reads() - before)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked");
    assert_eq!(out, format!("{N} {N}\n"));
    // Each of the 2N probes walks the one N-slot cluster about once: at most 2 N^2 reads. A
    // re-walk per compare step costs about N^3 / 3 (9.1M at N = 300).
    assert!(
        reads <= 2 * N * N,
        "{reads} slot reads for {N} keys, over {}",
        2 * N * N
    );
}

/// M2.4 — a struct of more than 3 fields keeps them in a `Spill` RVec of exactly their count. A
/// field write is a slot store: the block stays, nothing retires, and after a collection every
/// field reads back intact (in the heap and in a program under gc-stress).
#[test]
fn spill_field_write_is_slot_store() {
    let _g = super::rvec::test_serial();
    let mut heap = Heap::new();
    let kept = heap.alloc(Obj::Str("kept".into()));
    let h = heap.alloc(Obj::Struct {
        tid: crate::base::op::TID_NONE,
        fields: Fields::from_vec((1..=5).map(Value::int).collect()),
    });
    for i in 0..5 {
        let Obj::Struct { fields, .. } = heap.get_mut(h) else {
            unreachable!("a struct")
        };
        let v = if i == 2 {
            Value::obj(kept)
        } else {
            Value::int(i as i64 * 10)
        };
        assert!(fields.set(i, v), "field {i} is in range");
    }
    let Obj::Struct {
        fields: Fields::Spill(s),
        ..
    } = heap.get(h)
    else {
        panic!("5 fields spill")
    };
    assert_eq!(s.capacity(), 5, "the spill block holds exactly the fields");
    assert_eq!(heap.retired_len(), (0, 0), "a field write retires nothing");
    heap.mark(h);
    for c in heap.children(h) {
        heap.mark(c);
    }
    heap.sweep();
    let Obj::Struct { fields, .. } = heap.get(h) else {
        unreachable!("a struct")
    };
    let got: Vec<Value> = fields.iter().collect();
    assert_eq!(
        got,
        [
            Value::int(0),
            Value::int(10),
            Value::obj(kept),
            Value::int(30),
            Value::int(40)
        ]
    );
    assert!(matches!(heap.get(kept), Obj::Str(s) if &**s == "kept"));
    let src = "\
struct P:
    a: int
    b: str
    c: int
    d: List[int]
    e: int

fn main():
    p := P(a=1, b=\"x\", c=3, d=[4], e=5)
    p.a = 10
    p.b = str(20)
    p.d = [str(40).len()]
    p.e = 50
    junk := [str(1), str(2)]
    print(p.a, p.b, p.c, p.d, p.e, junk.len())
main()";
    assert_eq!(run_capture_stress(src), "10 20 3 [2] 50 2\n");
}

/// The block of an M2.4 fixed buffer: an `RVec` (so the test does not compile on a `Vec`), and
/// its capacity.
fn rvec_cap(v: &RVec<Value>) -> usize {
    v.capacity()
}

/// M2.4 — a closure's captures live in an exact-length `RVec`: `GetCaptured` reads a slot, and the
/// captures survive collections (gc-stress).
#[test]
fn closure_captures_are_a_fixed_rvec() {
    let mut heap = Heap::new();
    let home = heap.alloc(Obj::Str("home".into()));
    let h = heap.alloc(Obj::Closure {
        proto: 0,
        captured: vec![Value::int(1), Value::int(2)].into(),
        home,
    });
    let Obj::Closure { captured, .. } = heap.get(h) else {
        unreachable!("a closure")
    };
    assert_eq!(rvec_cap(captured), 2);
    let src = "\
fn mk(a: str, b: int) -> fn() -> str:
    return fn() -> str: a + str(b)

fn main():
    f := mk(str(7), 8)
    junk := [str(1), str(2)]
    print(f(), f(), junk.len())
main()";
    assert_eq!(run_capture_stress(src), "78 78 2\n");
}

/// M2.4 — an enum payload lives in an exact-length `RVec`, and an empty payload (`None`, every
/// unit variant) allocates no block at all.
#[test]
fn enum_payload_is_a_fixed_rvec_and_empty_allocates_nothing() {
    use super::rvec::{test_counts, test_serial};
    let _g = test_serial();
    let mut vm = Vm::new(Arc::new(crate::vm::tests::empty_program()));
    let (a0, _) = test_counts();
    let none = vm.alloc_enum("Option", "None", &[]);
    assert_eq!(test_counts().0, a0, "an empty payload allocates no block");
    let some = vm.alloc_enum("Option", "Some", &[Value::int(1)]);
    assert_eq!(test_counts().0, a0 + 1, "a one-value payload is one block");
    for (v, cap) in [(none, 0), (some, 1)] {
        let Obj::Enum { payload, .. } = vm.heap.get(v.as_obj().expect("an object")) else {
            unreachable!("an enum")
        };
        assert_eq!(rvec_cap(payload), cap);
    }
    let src = "\
enum Shape:
    Dot
    Circle(str, int)

fn main():
    xs := [Shape.Dot, Shape.Circle(str(3), 4), Shape.Dot]
    junk := [str(1), str(2)]
    n := 0
    for x in xs:
        match x:
            Shape.Dot:
                n += 1
            Shape.Circle(s, r):
                print(s, r)
    print(n, junk.len())
main()";
    assert_eq!(run_capture_stress(src), "3 4\n2 2\n");
}

/// M2.4 — a module's global slots live in an `RVec`: a global write is a slot store (no block
/// retires), and the globals survive collections (gc-stress).
#[test]
fn module_slots_are_an_rvec() {
    let _g = super::rvec::test_serial();
    let src = "\
K := [str(7)]
N := 0
fn main():
    for i in 0..20:
        N = N + i
    junk := [str(1), str(2)]
    print(K, N, junk.len())
main()";
    let owned = src.to_string();
    let (out, caps, retired) = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let mut vm = Vm::new(Arc::new(test_program(&owned).expect("compiles")));
            let result = vm.run();
            vm.finish_run(result).expect("runs");
            // The entry module (the one that binds `K`): its block holds exactly its slots.
            let caps = vm
                .module_objs
                .iter()
                .find_map(|&m| match vm.heap.get(m) {
                    Obj::Module(md) if md.index.contains_key("K") => {
                        let mut names: Vec<&str> = md.index.keys().map(|k| &**k).collect();
                        names.sort();
                        Some((rvec_cap(&md.slots), md.slots.len(), names.join(",")))
                    }
                    _ => None,
                })
                .expect("the entry module");
            let (own, core) = vm.heap.retired_len();
            (captured(vm.out), caps, own + core)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked");
    assert_eq!(out, "['7'] 190 2\n");
    assert_eq!(
        caps,
        (3, 3, "K,N,main".to_string()),
        "the entry module's block holds exactly its 3 slots"
    );
    assert_eq!(retired, 0, "a global write retires nothing");
    assert_eq!(run_capture_stress(src), "['7'] 190 2\n");
}

/// M2.4 — a parked generator sits behind one atomic owner pointer. The first take owns it; a
/// second take, before the put-back, sees it running and faults; after the put-back a take
/// succeeds again.
#[test]
fn generator_ctx_take_is_exclusive() {
    let mut vm = Vm::new(Arc::new(crate::vm::tests::empty_program()));
    let home = vm.heap.alloc(Obj::Str("home".into()));
    let g = vm.alloc_generator(0, home, None, vec![Value::int(1)]);
    let h = g.as_obj().expect("a generator object");
    let span = Span::RUNTIME;
    let first = vm
        .gen_take(h, span)
        .expect("the first take owns the parked context");
    let second = vm
        .gen_take(h, span)
        .err()
        .expect("a second take sees it running");
    assert!(
        second.message.contains("generator already running"),
        "{}",
        second.message
    );
    vm.gen_put(h, first);
    let third = vm
        .gen_take(h, span)
        .expect("a take after the put-back succeeds");
    assert!(matches!(third.state, GenState::Pending(ref a) if a.len() == 1));
    vm.gen_put(h, third);
}

/// Share the module globals `names` of `src` by handle between `n` worker `Vm`s on one heap. A
/// root `Vm` runs `src`, then waits detached with its roots published; each worker
/// (`spawn_worker`) runs on its own `VM_STACK_BYTES` thread: it attaches, roots the handles on its
/// stack, waits at one barrier with the others, runs `body(i, worker, handles)` and leaves. After
/// the join the root, attached again, returns `check(root, handles, results)`, the results in
/// worker order.
fn share_handle<T: Send + 'static, R: Send + 'static>(
    src: String,
    names: &'static [&'static str],
    n: usize,
    body: impl Fn(usize, &mut Vm, &[GcRef]) -> T + Send + Sync + 'static,
    check: impl FnOnce(&mut Vm, &[GcRef], Vec<T>) -> R + Send + 'static,
) -> R {
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let mut root = Vm::new(Arc::new(test_program(&src).expect("compiles")));
            let result = root.run();
            root.finish_run(result).expect("runs");
            let handles: Arc<[GcRef]> = names
                .iter()
                .map(|name| {
                    root.module_objs
                        .iter()
                        .find_map(|&m| match root.heap.get(m) {
                            Obj::Module(md) => {
                                md.index.get(*name).map(|&i| md.slots.get(i as usize))
                            }
                            _ => None,
                        })
                        .and_then(|v| v.as_obj())
                        .unwrap_or_else(|| panic!("the module global {name}"))
                })
                .collect();
            root.gc_attach();
            let go = Arc::new(std::sync::Barrier::new(n));
            let body = Arc::new(body);
            let workers: Vec<Vm> = (0..n).map(|_| root.spawn_worker()).collect();
            // The root waits on the join in the blocking door, so its roots (the module that
            // holds the globals) stay published while the workers collect.
            let results = root.gc_blocking(&[], |_| {
                let threads: Vec<_> = workers
                    .into_iter()
                    .enumerate()
                    .map(|(i, mut w)| {
                        let (go, body, handles) =
                            (Arc::clone(&go), Arc::clone(&body), Arc::clone(&handles));
                        std::thread::Builder::new()
                            .stack_size(VM_STACK_BYTES)
                            .spawn(move || {
                                w.gc_attach();
                                w.stack.extend(handles.iter().map(|&h| Value::obj(h)));
                                go.wait();
                                let out = body(i, &mut w, &handles);
                                w.gc_leave();
                                out
                            })
                            .expect("spawn a worker")
                    })
                    .collect();
                threads
                    .into_iter()
                    .map(|t| t.join().expect("a worker panicked"))
                    .collect()
            });
            check(&mut root, &handles, results)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// M2.4 — two threads drive one generator through two `Vm` handles on one heap. Each `.next()`
/// gives a value, `None`, or the "generator already running" fault; no call crashes, every value
/// goes to exactly one caller, in order, and `None` comes only after the last value (a resume
/// that raced a running generator never reads it as exhausted). Run under `CHEZZI_GC_STRESS=1`
/// too.
#[test]
fn generator_next_from_two_tasks_faults_cleanly() {
    const N: i64 = 300;
    let src = format!(
        "\
fn count(n: int) -> Iterator[int]:
    for i in 0..n:
        s := [str(i)]
        yield i + s.len() - 1

G := count({N})

fn main():
    pass
main()"
    );
    let clock = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (got, faults, last_val, first_none) = share_handle(
        src,
        &["G"],
        2,
        move |_, w, h| {
            let g = h[0];
            let (mut vals, mut faults, mut none) = (Vec::new(), 0usize, None);
            let tick = || clock.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for _ in 0..10_000_000 {
                // The loop's back edge: a safepoint, as a bytecode loop has.
                w.gc_poll();
                match w.generator_next(g, Span::RUNTIME) {
                    Ok(r) => match w.heap.get(r.as_obj().expect("an Option")) {
                        Obj::Enum {
                            variant_id,
                            payload,
                        } if *variant_id == crate::base::op::VID_SOME => {
                            vals.push((tick(), payload.get(0).as_int()))
                        }
                        _ => {
                            none = Some(tick());
                            break;
                        }
                    },
                    Err(e) => {
                        assert!(
                            e.message.contains("generator already running"),
                            "{}",
                            e.message
                        );
                        faults += 1;
                        std::thread::yield_now();
                    }
                }
            }
            (vals, faults, none)
        },
        |_, _, drivers| {
            let (mut got, mut faults, mut last_val, mut first_none) =
                (Vec::new(), 0, 0, usize::MAX);
            for (vals, f, none) in drivers {
                assert!(vals.is_sorted(), "one caller sees the values in order");
                last_val = vals
                    .iter()
                    .map(|&(t, _)| t)
                    .max()
                    .unwrap_or(0)
                    .max(last_val);
                first_none = first_none.min(none.expect("each caller ends on None"));
                got.extend(vals.into_iter().map(|(_, v)| v));
                faults += f;
            }
            got.sort();
            (got, faults, last_val, first_none)
        },
    );
    assert_eq!(
        got,
        (0..N).collect::<Vec<_>>(),
        "each value goes to one caller ({faults} faults)"
    );
    assert!(
        first_none > last_val,
        "a None at tick {first_none} came before the last value at tick {last_val}"
    );
}

/// M2.4 fix 1 — a Rust panic inside a resume puts the generator back `Done` (Python marks a
/// generator that raised as finished), so a later resume reads it exhausted, not running.
#[test]
fn a_panic_inside_a_resume_leaves_the_generator_done() {
    let mut vm = Vm::new(Arc::new(crate::vm::tests::empty_program()));
    let home = vm.heap.alloc(Obj::Str("home".into()));
    // No proto 9999 exists, so building the first frame panics inside the resume.
    let g = vm.alloc_generator(9999, home, None, Vec::new());
    let h = g.as_obj().expect("a generator object");
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = vm.generator_next(h, Span::RUNTIME);
    }));
    assert!(r.is_err(), "the resume panicked");
    let Ok(parked) = vm.gen_take(h, Span::RUNTIME) else {
        panic!("the panic put the box back")
    };
    assert!(matches!(parked.state, GenState::Done));
}

/// M2.5 — two threads race on one list `L` and one map `M` through two `Vm` handles on one heap,
/// by the paths a Chezzi program takes (`core_method`, `get_index`, `drain_iterable`). Thread A
/// pushes, inserts, removes every 7th op (an index rebuild and an entry block retire) and sorts
/// every 500th (`replace`); thread B reads `L[i]`, `L.len()`, probes `M` and iterates `M`. A racy
/// wrong value or a fault is the user's bug (Go parity); a read of freed memory is not: every read
/// gives a value of its element type, and after a collection `L` and `M` are sane. Run under
/// `CHEZZI_GC_STRESS=1` too.
#[test]
fn two_vms_race_on_one_list_and_map_without_uaf() {
    const OPS: i64 = 10_000;
    const READS: u32 = 16;
    let src = "\
L: List[int] = []
M: Map[int, str] = {}

fn main():
    pass
main()"
        .to_string();
    // One op of one thread: `Ok(true)` when every value it read has its element type.
    fn writer_op(w: &mut Vm, l: GcRef, m: GcRef, k: i64) -> Result<bool, RuntimeError> {
        let sp = Span::RUNTIME;
        w.core_method(l, "push", &[Value::int(k)], sp)?;
        let s = w.alloc_str(k.to_string());
        w.core_method(m, "insert", &[Value::int(k), s], sp)?;
        if k > 0 && k % 7 == 0 {
            w.core_method(m, "remove", &[Value::int(k - 3)], sp)?;
        }
        if k > 0 && k % 500 == 0 {
            w.core_method(l, "sort", &[], sp)?;
        }
        Ok(true)
    }
    fn is_str(w: &Vm, v: Value) -> bool {
        matches!(v.as_obj().map(|h| w.heap.get(h)), Some(Obj::Str(_)))
    }
    /// `seen` counts the reads that found data: an in-range `L[i]` and a `Some` probe hit.
    fn reader_op(
        w: &mut Vm,
        l: GcRef,
        m: GcRef,
        k: i64,
        r: u64,
        seen: &mut (usize, usize),
    ) -> Result<bool, RuntimeError> {
        let sp = Span::RUNTIME;
        let mut sane = true;
        // A burst of reads per op, so a read overlaps the writer's grow under stress too (each
        // op starts with a stop there).
        for j in 0..READS {
            let x = r.rotate_left(j * 7) ^ u64::from(j);
            let len = w.core_method(l, "len", &[], sp)?.as_int();
            if len > 0 {
                w.push(Value::obj(l));
                w.push(Value::int((x % len as u64) as i64));
                w.get_index(sp)?;
                let v = w.pop();
                let good = v.as_int_inline().is_some_and(|x| (0..OPS).contains(&x));
                seen.0 += usize::from(good);
                sane &= good;
            }
            let got = w.core_method(m, "get", &[Value::int((x >> 20) as i64 % OPS)], sp)?;
            sane &= match w.heap.get(got.as_obj().expect("an Option")) {
                Obj::Enum {
                    variant_id,
                    payload,
                } if *variant_id == crate::base::op::VID_SOME => {
                    let good = is_str(w, payload.get(0));
                    seen.1 += usize::from(good);
                    good
                }
                Obj::Enum { .. } => true,
                _ => false,
            };
        }
        if k % 100 == 0 {
            for pair in w.drain_iterable(Value::obj(m), sp)? {
                sane &= match pair.as_obj().map(|h| w.heap.get(h)) {
                    Some(Obj::Tuple(kv)) => match kv[..] {
                        [k, v] => k.as_int_inline().is_some() && is_str(w, v),
                        _ => false,
                    },
                    _ => false,
                };
            }
        }
        Ok(sane)
    }
    let (faults, garbage, (reads, hits)) = share_handle(
        src,
        &["L", "M"],
        2,
        |i, w, g| {
            let (l, m) = (g[0], g[1]);
            let (mut faults, mut garbage, mut seen) = (0usize, 0usize, (0usize, 0usize));
            let mut r = 0x9E37_79B9_7F4A_7C15u64;
            for k in 0..OPS {
                // The loop's back edge: a collection when one is due, else a safepoint, as
                // `run_until` does per instruction. Under stress only the writer asks for one per
                // op: a stop that both threads ask for each op runs their ops one at a time (each
                // asker waits out the other's op), and a race needs them to overlap.
                if (w.gc_stress && i == 0) || w.heap.should_collect() {
                    w.gc_collect();
                }
                w.gc_poll();
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                let op = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if i == 0 {
                        writer_op(w, l, m, k)
                    } else {
                        reader_op(w, l, m, k, r, &mut seen)
                    }
                }));
                match op {
                    Ok(Ok(true)) => {}
                    Ok(Ok(false)) => garbage += 1,
                    // A racy fault is the user's bug: count it and go on.
                    Ok(Err(_)) => faults += 1,
                    Err(p) => {
                        let msg = p
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| p.downcast_ref::<&str>().copied())
                            .unwrap_or("");
                        assert!(msg.contains("index out of range"), "a panic: {msg}");
                        faults += 1;
                    }
                }
            }
            (faults, garbage, seen)
        },
        |root, g, results| {
            let (l, m) = (g[0], g[1]);
            // The race freed retired blocks: collections ran while the threads raced.
            assert!(root.gc_coord.epoch() > 0, "no collection ran in the race");
            root.collect();
            let Obj::List(items) = root.heap.get(l) else {
                panic!("L is a list")
            };
            let mut xs: Vec<i64> = items.iter().map(|v| v.as_int()).collect();
            xs.sort();
            assert_eq!(xs, (0..OPS).collect::<Vec<_>>(), "L holds each push once");
            let removed = |k: i64| k >= 4 && k % 7 == 4 && k + 3 < OPS;
            let want: Vec<i64> = (0..OPS).filter(|&k| !removed(k)).collect();
            let entries = match root.heap.get(m) {
                Obj::Map(md) => md.entries.to_vec(),
                _ => panic!("M is a map"),
            };
            let mut keys: Vec<i64> = entries.iter().map(|e| e.1.as_int()).collect();
            keys.sort();
            assert_eq!(keys, want, "M holds each live key once");
            for (p, (_, k, v)) in entries.into_iter().enumerate() {
                let hk = root.scalar_hash(k);
                let at = root.map_probe(m, hk, k, Span::RUNTIME).expect("a probe");
                assert_eq!(at, Some(p), "key {} probes to its own entry", k.as_int());
                let Obj::Str(s) = root.heap.get(v.as_obj().expect("a str")) else {
                    panic!("a str value")
                };
                assert_eq!(s.to_string(), k.as_int().to_string());
            }
            let faults: usize = results.iter().map(|r| r.0).sum();
            let garbage: usize = results.iter().map(|r| r.1).sum();
            let seen = results
                .iter()
                .fold((0, 0), |a, r| (a.0 + r.2.0, a.1 + r.2.1));
            (faults, garbage, seen)
        },
    );
    assert_eq!(
        garbage, 0,
        "a read gave a value outside its type ({faults} faults)"
    );
    // The reader raced real data: it read `L` while it held elements and hit `M`'s keys.
    assert!(
        reads > 0 && hits > 0,
        "the reader found no data: {reads} L[i] reads, {hits} M hits"
    );
    // A racy fault is rare: at most 1% of one thread's ops (0 seen in the M2.5 runs).
    assert!(
        faults <= OPS as usize / 100,
        "{faults} faults in {OPS} ops a thread"
    );
}

/// M3.0a C3 — a native that holds its host thread runs in the GC door, detached, so a stop does
/// not wait for it. Thread A calls one (`Kind::Blocking`) on the shared list `L`; the native reads
/// `L` through `arg_str_list` a few times. Thread B, until A is done, overwrites `L[k]` with a new
/// str (the old one is unlinked), sorts `L` (the old block retires), allocates junk that a freed
/// slot can be reused for, and collects. The door copies the args out of the heap BEFORE it opens
/// (Go: `syscall.SlicePtrFromStrings` runs before `entersyscall`), so each read gives one list of
/// `s<n>` strs, and no read meets freed memory. Run under `CHEZZI_GC_STRESS=1` too.
#[test]
fn a_detached_native_reads_a_shared_list_without_uaf() {
    const OPS: i64 = 3_000;
    let src = "\
L: List[str] = []

fn main():
    for i in 0..64:
        L.push(\"s\" + str(i))
main()"
        .to_string();
    /// Reads its list arg 4 times; `false` when an element is not an `s<n>` str.
    fn probe(
        h: &mut dyn crate::native::Host,
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        let mut sane = true;
        for _ in 0..4 {
            for s in h.arg_str_list(0)? {
                sane &= s
                    .strip_prefix('s')
                    .is_some_and(|n| n.parse::<i64>().is_ok());
            }
        }
        Ok(crate::native::NativeRet::Bool(sane))
    }
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (garbage, collects) = share_handle(
        src,
        &["L"],
        2,
        move |i, w, g| {
            let l = g[0];
            let sp = Span::RUNTIME;
            let (mut garbage, mut collects) = (0usize, 0usize);
            if i == 0 {
                for _ in 0..OPS {
                    w.gc_poll();
                    let r = w.invoke_native(
                        probe,
                        "probe",
                        crate::native::Kind::Blocking,
                        vec![Value::obj(l)],
                        sp,
                    );
                    garbage += usize::from(r.expect("the probe runs") != Value::bool(true));
                }
                done.store(true, std::sync::atomic::Ordering::Release);
            } else {
                let mut k = 0i64;
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    let s = w.alloc_str(format!("s{k}"));
                    w.push(Value::obj(l));
                    w.push(Value::int(k % 64));
                    w.push(s);
                    w.set_index(sp).expect("a store");
                    if k % 4 == 0 {
                        w.core_method(l, "sort", &[], sp).expect("a sort");
                    }
                    // Junk that a freed slot of an unlinked str can be reused for.
                    for _ in 0..8 {
                        let j = w.alloc_str("junk".to_string());
                        w.heap.alloc(Obj::List(RVec::from_slice(&[j])));
                    }
                    w.gc_collect();
                    collects += 1;
                    k += 1;
                }
            }
            (garbage, collects)
        },
        |_, _, results| {
            let g = results.iter().map(|r| r.0).sum::<usize>();
            (g, results.iter().map(|r| r.1).sum::<usize>())
        },
    );
    assert!(collects > 100, "B collected {collects} times while A ran");
    assert_eq!(
        garbage, 0,
        "a detached read gave a value that is not an s<n> str"
    );
}

/// M3.0a C6 — the print walk waits for a busy `Writer` through the GC door while it holds the
/// elements of the list it renders in a Rust copy. Thread A sets `outer[1]` to a new list `xs`
/// and prints `outer = [W, xs]` while a plain thread holds `W`'s lock in bursts (a write in
/// progress), so the walk opens the door before it reaches `xs`; thread B sets `outer[1] = 0`
/// (`xs` is unlinked), allocates junk strs and collects. The walk roots every element it holds,
/// so it prints `xs` or `0`, never freed memory. Run under `CHEZZI_GC_STRESS=1` too.
#[test]
fn the_print_walk_roots_what_it_holds_across_a_door() {
    const PRINTS: usize = 100;
    let src = "\
import std.io
W := io.stdout()

fn main():
    pass
main()"
        .to_string();
    let outer_ref = Arc::new(Mutex::new(None::<GcRef>));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // B's collections and A's finished prints: the lock holder's handshake.
    let collects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let printed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (bad, doors) = share_handle(
        src,
        &["W"],
        2,
        move |i, w, g| {
            let sp = Span::RUNTIME;
            let ordering = std::sync::atomic::Ordering::SeqCst;
            let (mut bad, mut doors) = (Vec::new(), 0usize);
            if i == 0 {
                let Obj::Writer(core) = w.heap.get(g[0]) else {
                    panic!("W is a Writer")
                };
                let core = Arc::clone(core);
                let outer = w.heap.alloc(Obj::List(RVec::from_slice(&[
                    Value::obj(g[0]),
                    Value::int(0),
                ])));
                w.push(Value::obj(outer));
                *outer_ref.lock().unwrap() = Some(outer);
                // Ends the other two loops on a panic too, so a failed print fails the test.
                struct Done<'a>(&'a std::sync::atomic::AtomicBool);
                impl Drop for Done<'_> {
                    fn drop(&mut self) {
                        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                let _done = Done(&done);
                std::thread::scope(|s| {
                    // A write in progress: hold the lock until B collected 3 times, then let
                    // the print that waits on it finish before the next hold. The walk finds the
                    // lock busy and waits in the door while B unlinks and collects.
                    s.spawn(|| {
                        while !done.load(ordering) {
                            let g = core.inner.lock().unwrap();
                            let c0 = collects.load(ordering);
                            while !done.load(ordering) && collects.load(ordering) < c0 + 3 {
                                std::hint::spin_loop();
                            }
                            drop(g);
                            let p0 = printed.load(ordering);
                            while !done.load(ordering) && printed.load(ordering) == p0 {
                                std::hint::spin_loop();
                            }
                        }
                    });
                    // Until enough prints met a collection.
                    for p in 0..PRINTS * 1000 {
                        if doors >= PRINTS {
                            break;
                        }
                        w.gc_poll();
                        let xs = w
                            .heap
                            .alloc(Obj::List(RVec::from_slice(&[Value::int(p as i64 + 1); 3])));
                        w.push(Value::obj(outer));
                        w.push(Value::int(1));
                        w.push(Value::obj(xs));
                        w.set_index(sp).expect("a store");
                        let before = w.gc_coord.epoch();
                        let text = w.stringify(Value::obj(outer), sp, 0).expect("a print");
                        printed.fetch_add(1, ordering);
                        doors += usize::from(w.gc_coord.epoch() != before);
                        let n = p + 1;
                        if text != format!("[Writer(open), [{n}, {n}, {n}]]")
                            && text != "[Writer(open), 0]"
                        {
                            bad.push(text);
                        }
                    }
                    drop(_done);
                });
            } else {
                let outer = loop {
                    if let Some(o) = *outer_ref.lock().unwrap() {
                        break o;
                    }
                    w.gc_poll();
                    std::thread::yield_now();
                };
                while !done.load(ordering) {
                    w.push(Value::obj(outer));
                    w.push(Value::int(1));
                    w.push(Value::int(0));
                    w.set_index(sp).expect("a store");
                    // Junk that a freed slot of the unlinked list can be reused for.
                    for _ in 0..8 {
                        w.alloc_str("junk".to_string());
                    }
                    w.gc_collect();
                    collects.fetch_add(1, ordering);
                }
            }
            (bad, doors)
        },
        |_, _, results| {
            let doors = results.iter().map(|r| r.1).sum::<usize>();
            let bad: Vec<String> = results.into_iter().flat_map(|r| r.0).collect();
            (bad, doors)
        },
    );
    assert!(doors >= PRINTS, "{doors} prints met a collection");
    assert!(
        bad.is_empty(),
        "{} prints gave wrong text: {:?}",
        bad.len(),
        &bad[..bad.len().min(3)]
    );
}
