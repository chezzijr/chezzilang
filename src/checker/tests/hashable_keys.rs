//! Checker tests: a map key and a set element must be `Hashable`, and `Hashable` embeds `Eq`.
//! Split out of `src/checker/tests/protocols_eq.rs`. `super::*` is the parent module.

use super::*;

/// **W7-45, the map/set half — a key type must be `Eq`, not merely `Hashable`.** A `Map`/`Set` probe
/// runs `values_equal` on a hash COLLISION, so a key whose `eq` has unsatisfied `where` bounds
/// check-cleaned and then faulted. It is the worse half of the row because the verdict depended on
/// hash values: the SAME unsound program with a constant `hash` faulted and with distinct hashes
/// printed a silent wrong `false`, rc=0.
///
/// **Rust owns this seam and spells it in the bound:** `impl<T: Eq + Hash> HashSet<T>` /
/// `impl<K: Eq + Hash, V> HashMap<K, V>` — `Hash` alone is never enough. Chezzi's `Hashable`
/// protocol embeds `Eq` (owner decision 2, TICKET-265), so the one `Hashable` question asks both.
///
/// One `Hashable` question answers every map-key and set-element position: `key_ty_reject` for the
/// literal, the comprehension, the annotation, `m[k]` read and write and each container method, and
/// the `T: Hashable` bound of the prelude decl for `Set.from_iter(xs)`. Both carry the same reason
/// (`Box[Tag]'s `eq` requires Tag: Comparable`), and this test asserts it at each position.
#[test]
fn a_map_key_or_set_element_must_be_eq_not_merely_hashable() {
    const COND: &str = "\
import Comparable from std.cmp
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
    fn hash(self) -> int:
        return 1
a := Box(Tag(1))
b := Box(Tag(2))
";
    const WHY: &str = "Box[Tag]'s `eq` requires Tag: Comparable";
    // Each of these was measured check-clean-then-faulting on the pre-change binary.
    for tail in [
        "print(Set.from_iter([a]).len())\n", // Set literal-ish ctor
        "s := Set.from_iter([a])\ns.insert(b)\nprint(s.len())\n", // .insert
        "s := Set.from_iter([a])\nprint(s.contains(b))\n", // .has
        "s := Set.from_iter([a])\nprint(b in s)\n", // `in` over a set
        "print({a: 1}.len())\n",             // map literal
        "m := {a: 1}\nprint(m.contains(b))\n", // Map.has
        "m := {a: 1}\nprint(m.get(b))\n",    // Map.get
        "m := {a: 1}\nm[b] = 2\nprint(m.len())\n", // index write
        "m := {a: 1}\nprint(m[b])\n",        // index read
        "m := {a: 1}\nprint(b in m)\n",      // `in` over a map
        "print({x for x in [a]}.len())\n",   // set comprehension
        "print({x: 1 for x in [a]}.len())\n", // map comprehension
        "xs := [a]\nprint(Set.from_iter(xs).len())\n", // Set(list) CONSTRUCTION
        "s: Set[Box[Tag]] = Set.from_iter([])\nprint(s.len())\n", // Set annotation
        "m: Map[Box[Tag], int] = {}\nprint(m.len())\n", // Map annotation
        "s: Set[Box[Tag]] = Set.from_iter([])\ns2 := s.union(s)\n", // set algebra
        "struct W:\n    s: Set[Box[Tag]]\nprint(W(Set.from_iter([a])).s.len())\n", // a struct FIELD's type
        "print(Set.from_iter([(a, 1)]).len())\n", // a tuple key holding the bad element
        "print({(1, a): 1}.len())\n",             // a tuple map key holding the bad element
    ] {
        let errs = check_entry(&format!("{COND}{tail}"));
        assert!(
            errs.iter().any(|e| e.message.contains(WHY)),
            "`{tail}`: expected the eq-bound reason, got {errs:?}"
        );
    }
    // The message TAIL changes with the reason; the four prefixes do not, so no existing needle moves.
    let errs = check_entry(&format!("{COND}print(Set.from_iter([a]).len())\n"));
    assert_eq!(
        errs[0].message,
        format!("type Box[Tag] does not satisfy Hashable ({WHY})"),
        "got {errs:?}"
    );
    let errs = check_entry(&format!("{COND}print({{a: 1}}.len())\n"));
    assert_eq!(
        errs[0].message,
        format!("map key type {WHY}"),
        "got {errs:?}"
    );
    // The Hashable half still reports the Hashable text — the new conjunct does not swallow it, and
    // a type that fails BOTH reports only the first (`Hashable` is checked first, as before).
    rejects(
        "struct Bare:\n    a: int\ns: Set[Bare] = Set()\n",
        "Set element type must implement Hashable",
    );

    // ---- the boundary: every row measured green on the PRE-CHANGE binary too ----

    // Ordinary key types, at the literal / comprehension / ctor / annotation / index sites.
    entry_ok(
        "struct H:\n    n: int\n    fn hash(self) -> int:\n        return self.n\n    fn eq(self, other: H) -> bool:\n        return self.n == other.n\nprint(Set.from_iter([1]).len())\nprint(Set.from_iter([\"s\"]).len())\nprint(Set.from_iter([true]).len())\nprint(Set.from_iter([b\"a\"]).len())\nprint(Set.from_iter([H(1)]).len())\nprint({\"k\": 1}.len())\nprint({x for x in [1]}.len())\nprint({x: 1 for x in [1]}.len())\nxs := [1, 2]\nprint(Set.from_iter(xs).len())\nm := {\"k\": 1}\nprint(m[\"k\"])\nm[\"j\"] = 2\n",
    );
    // The SATISFIED instantiation of the very same conditional-`eq` struct, everywhere.
    entry_ok(&format!(
        "{COND}s := Set.from_iter([Box(1)])\ns.insert(Box(2))\nprint(s.contains(Box(1)))\nm := {{Box(1): 1}}\nm[Box(2)] = 2\nprint(m[Box(1)])\nprint(Box(1) in m)\n"
    ));
    entry_ok(&format!(
        "{COND}s: Set[Box[int]] = Set.from_iter([])\nm: Map[Box[int], int] = {{}}\nprint(s.len() + m.len())\n"
    ));
    // An empty literal's element/key type is still `Unknown` — must not cascade.
    entry_ok(&format!(
        "{COND}print(Set.from_iter([]).len() + {{}}.len())\n"
    ));
    // NOT ERASED any more (W7-53): a free `T` reaching a spelled `Set[T]`/`Map[T, _]` position inside
    // a generic body is judged directly. `Hashable` embeds `Eq` (owner decision 2, TICKET-265;
    // measured: the `Holder`-shaped ordinary-`eq`-method escape hatch still checks and runs), so
    // `[T: Hashable]` alone licenses the body, and `[T: Hashable + Eq]` still checks.
    entry_ok(
        "import Eq, Hashable from std.cmp\nfn h[T: Hashable + Eq](xs: Set[T]) -> int:\n    return xs.len()\nfn k[T: Hashable + Eq](m: Map[T, int]) -> int:\n    return m.len()\nfn s2[T: Hashable + Eq](x: T) -> int:\n    s := Set.from_iter([x])\n    s.insert(x)\n    m := {x: 1}\n    return s.len() + m.len()\nprint(h(Set.from_iter([1])) + k({1: 2}) + s2(1))\n",
    );
    rejects(
        "fn h[T](xs: Set[T]) -> int:\n    return xs.len()\n",
        "Set element type must implement Hashable",
    );
    // W7-53's third measured instance: a `Set.from_iter(xs)` construction inside a body whose `T` has no
    // `Hashable` bound is rejected at ITS OWN definition. `[T: Hashable]` alone now checks.
    rejects(
        "fn f[T](xs: List[T]) -> int:\n    return Set.from_iter(xs).len()\n",
        "type T does not satisfy Hashable",
    );
    entry_ok(
        "import Hashable from std.cmp\nfn f[T: Hashable](xs: List[T]) -> int:\n    return Set.from_iter(xs).len()\nprint(f([1, 1]))\n",
    );
}
