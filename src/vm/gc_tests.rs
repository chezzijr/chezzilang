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
    assert_eq!(run_capture_stress(src), "[1, 2]\n");
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
    assert_eq!(run_capture_stress(src), "[7, 8]\n");
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
        "01234567\ntrue\nSome(5)\nSome(2)\nSome(0)\n7\n"
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

/// Same hazard via `sort_by` with an allocating comparator on an inline-temporary list.
#[test]
fn struct_sort_by_inline_temporary_survives_gc_stress() {
    let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c)]
        return self.c - o.c
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
    let normal = run_capture(src).unwrap();
    assert_eq!(run_capture_stress(src), normal);
    // M:N delivers the same channel values, but the three racing spawns can interleave, so compare
    // order-insensitively (the exact cooperative order is pinned above).
    assert_same_lines(&normal, &run_capture_parallel(src).expect("M:N run"));
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
    let normal = run_capture(src).unwrap();
    assert_eq!(run_capture_stress(src), normal);
    // (No M:N cross-check: the two spawns race under the pooled engine, so their push order — and
    // thus the printed list — is not deterministic. GC survival, the point here, is cooperative.)
}

/// Executor: submitted task closures (queued in the heap obj) and the popped task drained at
/// `shutdown` must survive GC firing between submit and drain, and across each task's re-entrant
/// call. Each task allocates a string into a Channel — a missing root would corrupt the output or
/// dangle a `GcRef`.
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
    let normal = run_capture(src).unwrap();
    assert_eq!(
        run_capture_stress(src),
        normal,
        "VM gc_stress diverged (executor rooting bug?)"
    );
    // The two Executor tasks race on the M:N pool, so their sends arrive in either order — compare
    // order-insensitively (the exact cooperative order is pinned above).
    assert_same_lines(&normal, &run_capture_parallel(src).expect("M:N run"));
}
