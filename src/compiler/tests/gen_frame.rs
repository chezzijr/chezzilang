//! TICKET-190 — a generator frame slot is private (`Proto.private_slots` bit set) only when the
//! parent cannot reach its root; a param slot is then stamped at the creating call site. One cell
//! per rule direction (the cell map is the plan's step 3d).

use super::*;
use crate::base::op::Op;

const MAIN: &str = r#"import h
struct P:
    x: int
    fn bump(self):
        self.x += 1
struct R:
    d: List[int]
    fn index(self, k: int) -> R:
        return self
    fn iter(self) -> Iterator[int]:
        return self.d.iter()
G: List[List[int]] = []
fn t_id(xs: List[int]) -> List[int]:
    return xs
fn count(xs: List[int]) -> int:
    return xs.len()
fn keep(xs: List[int]):
    G.push(xs)
fn fwd(xs: List[int]):
    keep(xs)
fn vcount(...xs: List[int]) -> int:
    return xs.len()
fn vfirst(first: List[int], ...rest: int) -> int:
    return first.len()
fn gi(xs: List[int]) -> Iterator[int]:
    yield xs.len()
fn g1(xs: List[int]) -> Iterator[int]:
    xs.push(1)
    yield xs.len()
fn g2(xs: List[int]) -> Iterator[List[int]]:
    yield xs
fn g3() -> Iterator[int]:
    acc := [1]
    acc.push(2)
    yield acc.len()
fn g4() -> Iterator[int]:
    acc := t_id([1])
    yield acc.len()
fn g5() -> Iterator[int]:
    p := P(x=1)
    p.x = 2
    yield p.x
fn g6(sink: List[List[int]]) -> Iterator[int]:
    acc: List[int] = []
    sink.push(acc)
    yield 0
fn g7() -> Iterator[int]:
    acc := [1]
    _ := count(acc)
    yield 0
fn g8() -> Iterator[int]:
    acc := [1]
    keep(acc)
    yield 0
fn g9() -> Iterator[int]:
    acc := [1]
    fwd(acc)
    yield 0
fn g10() -> Iterator[int]:
    acc := [1]
    f := fn() -> int: acc.len()
    yield f()
fn g11() -> Iterator[int]:
    for v in [[1]]:
        v.push(2)
        yield 0
fn g12() -> Iterator[int]:
    acc := [1]
    b := acc
    yield b.len()
fn g13() -> Iterator[int]:
    acc := [1]
    acc = t_id([2])
    yield acc.len()
fn g14() -> Iterator[int]:
    p := P(x=1)
    p.bump()
    yield p.x
fn g15() -> Iterator[int]:
    acc := [1]
    _ := count(xs=acc)
    yield 0
fn g16() -> Iterator[int]:
    acc := [1]
    acc = [2]
    acc += [3]
    yield acc.len()
fn g17() -> Iterator[int]:
    acc := [1]
    acc[0] = 5
    yield acc[0]
fn g18() -> Iterator[int]:
    r := R(d=[1])
    s := r[0]
    yield s.d.len()
fn g19() -> Iterator[int]:
    acc := [1]
    for v in acc:
        yield v
fn g20() -> Iterator[int]:
    r := R(d=[1])
    for v in r:
        yield v
fn g21() -> Iterator[int]:
    acc := [1]
    n := acc + [2]
    yield n.len()
fn g22() -> Iterator[int]:
    p := P(x=1)
    _ := p == P(x=2)
    p.x = 3
    yield p.x
fn g23(t: (List[int], int)) -> Iterator[int]:
    yield t.1
fn g24() -> Iterator[int]:
    acc := [1]
    _ := h.hcount(acc)
    yield 0
fn g25() -> Iterator[int]:
    acc := [1]
    h.hkeep(acc)
    yield 0
fn g26() -> Iterator[int]:
    a, b := [1], [2]
    a.push(3)
    yield a.len() + b.len()
fn g27() -> Iterator[int]:
    acc := [1]
    _ := vcount(acc)
    yield 0
fn g28() -> Iterator[int]:
    acc := [1]
    _ := vfirst(acc, 1)
    yield 0
fn g29() -> Iterator[int]:
    acc := [1]
    _ := gi(acc)
    yield 0
fn g30() -> Iterator[int]:
    acc := [1]
    _ := h.hgen(acc)
    yield 0
fn g31() -> Iterator[int]:
    acc := [1]
    _ := h.hesc(acc)
    yield 0
xs := [1]
a := g1([])
b := g1(xs)
c := count([1])
"#;

const H: &str = r#"HG: List[List[int]] = []
fn hcount(xs: List[int]) -> int:
    return xs.len()
fn hkeep(xs: List[int]):
    HG.push(xs)
fn hgen(xs: List[int]) -> Iterator[int]:
    yield xs.len()
fn hpush(xs: List[int]) -> int:
    HG.push(xs)
    return 0
hesc := fn(xs: List[int]) -> int: hpush(xs)
"#;

#[test]
fn generator_slot_privacy_grid() {
    let t = crate::test_support::TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    t.write("h.chz", H);
    let entry = t.write("main.chz", MAIN);
    let graph = crate::resolver::build_graph(&entry).expect("graph should build");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        panic!("the grid program must type-check: {errs:?}");
    }
    let program = compile_graph(&graph).expect("compile");
    let want: [(&str, u64); 31] = [
        ("g1", 0b1),
        ("g2", 0),
        ("g3", 0b1),
        ("g4", 0),
        ("g5", 0b1),
        ("g6", 0b1),
        ("g7", 0b1),
        ("g8", 0),
        ("g9", 0),
        ("g10", 0),
        ("g11", 0),
        ("g12", 0),
        ("g13", 0),
        ("g14", 0),
        ("g15", 0),
        ("g16", 0b1),
        ("g17", 0b1),
        ("g18", 0),
        ("g19", 0b1),
        ("g20", 0),
        ("g21", 0b11),
        ("g22", 0),
        ("g23", 0b1),
        ("g24", 0b1),
        ("g25", 0),
        ("g26", 0),
        ("g27", 0),
        ("g28", 0b1),
        ("g29", 0),
        ("g30", 0),
        // h's slot 'hesc' holds a closure that escapes 'acc' into HG.
        ("g31", 0),
    ];
    for (name, mask) in want {
        let p = program
            .protos
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no proto {name}"));
        // TICKET-240: a deep slot is private too; no cell loses a private bit.
        assert_eq!(
            p.private_slots,
            mask | p.deep_slots,
            "cell {name}: private_slots"
        );
    }
    // The creating call site stamps a generator's param slots: `g1([])` is fresh, `g1(xs)`
    // names a parent value, and `count([1])` makes no generator.
    let main_top = program.modules.last().expect("entry module").toplevel;
    let stamps = |code: &[Op]| -> Vec<u64> {
        code.iter()
            .filter_map(|op| match op {
                Op::StampGen(m) => Some(*m),
                _ => None,
            })
            .collect()
    };
    for (pid, p) in program.protos.iter().enumerate() {
        if p.name == "<toplevel>" && pid != main_top {
            assert_eq!(stamps(&p.code), Vec::<u64>::new(), "stamps: h.chz toplevel");
        }
    }
    assert_eq!(
        stamps(&program.protos[main_top].code),
        vec![
            crate::base::crossing::stamp_mask(&[crate::base::crossing::Fresh::All]),
            crate::base::crossing::stamp_mask(&[crate::base::crossing::Fresh::Marked]),
        ],
        "stamps: main.chz toplevel"
    );
}

const DEEP: &str = r#"struct N:
    kids: List[List[int]]
struct Key:
    id: int
    log: List[int]
    fn hash(self) -> int:
        return self.id
    fn eq(self, other: Key) -> bool:
        return self.id == other.id
fn count2(xs: List[List[int]]) -> int:
    return xs.len()
fn d1() -> Iterator[int]:
    buf: List[int]? = []
    match buf:
        ?a: a.push(1)
        None: pass
    yield 0
fn d2() -> Iterator[int]:
    buf := [[0]]
    buf[0].push(1)
    yield buf[0][0]
fn d3() -> Iterator[int]:
    buf := [[0]]
    for v in buf[0]:
        yield v
fn d4() -> Iterator[int]:
    buf := [[0]]
    buf.push([])
    buf[0] = [2]
    buf.reverse()
    yield buf.len()
fn d5() -> Iterator[int]:
    s := N([[0]])
    s.kids = [[9]]
    s.kids[0].push(1)
    yield 0
fn d6(p: List[List[int]]) -> Iterator[int]:
    p[0].push(1)
    yield 0
fn n1() -> Iterator[List[int]]:
    buf := [[0]]
    yield buf[0]
fn n2(p: List[int]) -> Iterator[int]:
    buf := [[0]]
    buf.push(p)
    yield 0
fn n3() -> Iterator[int]:
    buf := [[0]]
    x := buf[0]
    yield x.len()
fn n4(p: List[int]) -> Iterator[int]:
    buf := [[0]]
    buf[0].extend(p)
    yield 0
fn n5() -> Iterator[int]:
    buf: List[int]? = []
    match buf:
        ?a: a.push(1)
        None: pass
    match buf:
        ?a: a.push(2)
        None: pass
    yield 0
fn n6() -> Iterator[int]:
    buf: List[int]? = []
    match buf:
        ?a:
            f := fn() -> int: a.len()
            yield f()
        None: pass
fn n7(p: List[int]) -> Iterator[int]:
    buf := [p, []]
    buf.reverse()
    yield 0
fn n8(p: Key) -> Iterator[int]:
    buf: Map[Key, List[int]] = {}
    buf[p] = []
    yield 0
fn n9() -> Iterator[List[int]]:
    buf: List[int]? = []
    match buf:
        ?a:
            yield a
        None: pass
fn n10() -> Iterator[int]:
    buf := [[0]]
    n := buf + [[1]]
    yield n.len()
fn n11() -> Iterator[int]:
    buf := [[0]]
    _ := count2(buf)
    yield 0
fn n12() -> Iterator[int]:
    buf := [[0]]
    s := buf[0..1]
    yield s.len()
fn n13(i: int) -> Iterator[int]:
    buf := [[i]]
    buf[0].push(1)
    yield 0
fn n15() -> Iterator[int]:
    buf := [[0] for _i in 0..1]
    buf[0].push(1)
    yield 0
fn n14() -> Iterator[int]:
    buf: List[int]? = []
    match buf:
        ?a:
            a = [9]
            a.push(1)
        None: pass
    yield 0
xs := [0]
ys := [[0]]
a := d6([[0]])
b := d6(ys)
c := d6([xs])
"#;

/// TICKET-240 Rule G: a frame slot's whole graph is private (`Proto.deep_slots`) only when
/// every binding is all-fresh, no store puts a parent value into the graph and no view of it
/// ends in a value that can hold a mark. One generator per rule line, as
/// `(name, private_slots, deep_slots)`.
#[test]
fn generator_deep_privacy_grid() {
    let t = crate::test_support::TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    let entry = t.write("main.chz", DEEP);
    let graph = crate::resolver::build_graph(&entry).expect("graph should build");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        panic!("the grid program must type-check: {errs:?}");
    }
    let program = compile_graph(&graph).expect("compile");
    let want: [(&str, u64, u64); 21] = [
        // Deep: a carrier's payload through a match binder, a child receiver, a loop over a
        // child, fresh stores with a permuting method, a struct field store, a param.
        ("d1", 0b1, 0b1),
        ("d2", 0b1, 0b1),
        ("d3", 0b1, 0b1),
        ("d4", 0b1, 0b1),
        ("d5", 0b1, 0b1),
        ("d6", 0b1, 0b1),
        // Root only: a child leaves through `yield`, a param is pushed in, a child is bound to
        // a name, a param is extended into a child.
        ("n1", 0b1, 0),
        ("n2", 0b10, 0),
        ("n3", 0b1, 0),
        ("n4", 0b10, 0),
        // Marked: the root was matched and its binder is bound twice, captured by a closure,
        // yielded, or reassigned.
        ("n5", 0, 0),
        ("n6", 0, 0),
        ("n9", 0, 0),
        ("n14", 0, 0),
        // Root only: a literal holding a param (a permuting method may move it), a store under
        // a param key, an operator, a named callee, a slice, a scalar name in the literal.
        ("n7", 0b10, 0),
        ("n8", 0b10, 0),
        ("n10", 0b11, 0),
        ("n11", 0b1, 0),
        ("n12", 0b11, 0),
        ("n13", 0b10, 0),
        // A ceiling: the checker's verdict is `All`, but the local takes the slot the
        // comprehension's hidden loop slots left, and a reused slot is the AND of its claims.
        ("n15", 0, 0),
    ];
    for (name, private, deep) in want {
        let p = program
            .protos
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("no proto {name}"));
        assert_eq!(
            (p.private_slots, p.deep_slots),
            (private, deep),
            "cell {name}: (private_slots, deep_slots)"
        );
    }
    // The creating call stamps two bits per param: all-fresh, named, fresh root over a named
    // child.
    use crate::base::crossing::{Fresh, stamp_mask};
    let main_top = program.modules.last().expect("entry module").toplevel;
    let stamps: Vec<u64> = program.protos[main_top]
        .code
        .iter()
        .filter_map(|op| match op {
            Op::StampGen(m) => Some(*m),
            _ => None,
        })
        .collect();
    assert_eq!(
        stamps,
        vec![
            stamp_mask(&[Fresh::All]),
            stamp_mask(&[Fresh::Marked]),
            stamp_mask(&[Fresh::root()]),
        ]
    );
}
