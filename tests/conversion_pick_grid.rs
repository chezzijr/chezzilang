//! Which conversion overload is picked (wave 24 F5): ONE pick for (overload set, slot, input), where
//! the input is an argument checked under each candidate's key, a type at the slot (`[X]` or an
//! annotation target) or a fn type. A set of one decl and a set of 2+ give the same answer.
//!
//! Axes: use site (direct `T.from(x)`, `T.from[X](x)`, `T.from` value, `T.from[X]` value,
//! `x.into()` under an annotation, `x?.into()` under an annotation, `x.into()` with no target
//! (`print`, an open type var), a bound `T: From[X]`, `map(T.from)`, `map(T.from[X])`) × argument
//! form (`[1]`, `{"a": 1}`, `(1, 2)`, a lambda, a typed var) × set size (1, 2+). Plus the builtin
//! static heads (`bool.zz`, `int8.zz`, an alias) and `from(x: int?)` against `int`, direct and
//! through a bound. Owner decisions 2026-11-09/14: `T.from[X]` is a value (Rust
//! `<T as From<X>>::from`); a bound is met by the declared signature, exact (`from(x: int?)` meets
//! `From[int?]`, not `From[int]`), while a direct call is an ordinary call whose argument wraps; a
//! fn value picks by fn assignability; `Any` and an open type var name no target; every builtin
//! type name is a static head; a bare literal picks by the param slot.

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-convpick-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "main.chz"])
        .current_dir(&dir)
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The types every cell reads: each source shape as a set of one (`*1`) and of two (`*2`).
const PRELUDE: &str = "\
import From, Into from std.convert
struct L1:
    n: int
    fn from(x: List[int?]) -> L1:
        return L1(1)
struct L2:
    n: int
    fn from(x: List[int?]) -> L2:
        return L2(1)
    fn from(x: str) -> L2:
        return L2(2)
struct M1:
    n: int
    fn from(x: Map[str, int?]) -> M1:
        return M1(1)
struct M2:
    n: int
    fn from(x: Map[str, int?]) -> M2:
        return M2(1)
    fn from(x: str) -> M2:
        return M2(2)
struct T1:
    n: int
    fn from(x: (int, int?)) -> T1:
        return T1(1)
struct T2:
    n: int
    fn from(x: (int, int?)) -> T2:
        return T2(1)
    fn from(x: str) -> T2:
        return T2(2)
struct F1:
    n: int
    fn from(x: fn(int) -> int) -> F1:
        return F1(x(1))
struct F2:
    n: int
    fn from(x: fn(int) -> int) -> F2:
        return F2(x(1))
    fn from(x: str) -> F2:
        return F2(2)
struct O1:
    n: int
    fn from(x: int?) -> O1:
        return O1(x ?? -1)
struct O2:
    n: int
    fn from(x: int?) -> O2:
        return O2(x ?? -1)
    fn from(x: str) -> O2:
        return O2(x.len() * 100)
struct I1:
    n: int
    fn from(x: int) -> I1:
        return I1(x)
struct I2:
    n: int
    fn from(x: int) -> I2:
        return I2(x)
    fn from(x: str) -> I2:
        return I2(x.len())
struct V1:
    n: int
    fn into(self) -> float:
        return 1.5
struct V2:
    n: int
    fn into(self) -> float:
        return 1.5
    fn into(self) -> str:
        return \"v2\"
fn make[S, T: From[S]](x: S) -> T:
    return T.from(x)
fn mk[T: From[int]](v: int) -> T:
    return T.from(v)
fn get(b: bool) -> V2?:
    if b:
        return V2(1)
    return None
fn id[T](v: T) -> T:
    return v
";

/// (tag, body, expected stdout). Each body runs after [`PRELUDE`].
const ACCEPT: &[(&str, &str, &str)] = &[
    // Direct call, a bare literal: the param slot types it, for one decl and for two.
    (
        "d_list",
        "print(L1.from([1]), L2.from([1]))",
        "L1(n=1) L2(n=1)",
    ),
    (
        "d_map",
        "print(M1.from({\"a\": 1}), M2.from({\"a\": 1}))",
        "M1(n=1) M2(n=1)",
    ),
    (
        "d_tuple",
        "print(T1.from((1, 2)), T2.from((1, 2)))",
        "T1(n=1) T2(n=1)",
    ),
    (
        "d_tuple_none",
        "print(T1.from((1, None)), T2.from((1, None)))",
        "T1(n=1) T2(n=1)",
    ),
    (
        "d_lambda",
        "print(F1.from(fn(a): a + 1), F2.from(fn(a): a + 1))",
        "F1(n=2) F2(n=2)",
    ),
    (
        "d_typed_var",
        "xs: List[int?] = [1]\nprint(L1.from(xs), L2.from(xs))",
        "L1(n=1) L2(n=1)",
    ),
    (
        "d_other",
        "print(L2.from(\"ab\"), O2.from(\"ab\"))",
        "L2(n=2) O2(n=200)",
    ),
    (
        "d_wrap",
        "print(O1.from(3), O2.from(3), O2.from(None))",
        "O1(n=3) O2(n=3) O2(n=-1)",
    ),
    ("d_annotated", "r: L2 = L2.from([1])\nprint(r)", "L2(n=1)"),
    // `T.from[X](x)`: the `[X]` names the source, the argument is checked under it.
    (
        "t_list",
        "print(L1.from[List[int?]]([1]), L2.from[List[int?]]([1]))",
        "L1(n=1) L2(n=1)",
    ),
    (
        "t_other",
        "print(I2.from[str](\"abc\"), I1.from[int](4))",
        "I2(n=3) I1(n=4)",
    ),
    // `T.from` as a value: the position's fn type picks.
    (
        "v_annot",
        "f: fn(int) -> I1 = I1.from\ng: fn(int) -> I2 = I2.from\nprint(f(3), g(4))",
        "I1(n=3) I2(n=4)",
    ),
    (
        "v_map",
        "print([1].map(I1.from), [2].map(I2.from))",
        "[I1(n=1)] [I2(n=2)]",
    ),
    // `T.from[X]` as a value: the `[X]` picks, for one decl and for two.
    (
        "tv_bind",
        "g := I1.from[int]\nh := I2.from[int]\nprint(g(3), h(4))",
        "I1(n=3) I2(n=4)",
    ),
    (
        "tv_map",
        "print([1].map(I1.from[int]), [2].map(I2.from[int]))",
        "[I1(n=1)] [I2(n=2)]",
    ),
    ("tv_str", "h := I2.from[str]\nprint(h(\"ab\"))", "I2(n=2)"),
    // `x.into()` under an annotation picks by it.
    (
        "i_annot",
        "r := V2(1)\nx: float = r.into()\ns: str = r.into()\nprint(x, s)",
        "1.5 v2",
    ),
    (
        "i_one",
        "print(V1(1).into(), [V1(1)].map(V1.into))",
        "1.5 [1.5]",
    ),
    // `x?.into()` under an annotation: the trial inference keeps the caller's expected type.
    ("o_annot", "d: str? = get(true)?.into()\nprint(d)", "v2"),
    (
        "o_coalesce",
        "e: float = get(true)?.into() ?? 0.0\nprint(e)",
        "1.5",
    ),
    ("o_none", "d: str? = get(false)?.into()\nprint(d)", "None"),
    (
        "o_targ",
        "print(get(true)?.into[str](), get(false)?.into[str]())",
        "v2 None",
    ),
    // A bound `T: From[S]` picks by the direct call's relation: `int` wraps into `int?`.
    // A bound is met by the declared param, exact: `from(x: int?)` meets `From[int?]`.
    (
        "b_opt",
        "fn mko[T: From[int?]](v: int?) -> T:\n    return T.from(v)\nv: int? = 4\na: O1 = mko(?3)\nb: O2 = mko(None)\nc: O2 = mko(v)\nd: O2 = make(v)\nprint(a, b, c, d)",
        "O1(n=3) O2(n=-1) O2(n=4) O2(n=4)",
    ),
    (
        "b_exact",
        "a: I2 = make(\"ab\")\nb: I1 = mk(5)\nprint(a, b)",
        "I2(n=2) I1(n=5)",
    ),
];

/// (tag, body, error text). Each body runs after [`PRELUDE`] and must be rejected with this text.
const REJECT: &[(&str, &str, &str)] = &[
    // No widening, one decl or two.
    (
        "d_float1",
        "print(I1.from(1.5))",
        "argument 1 of 'from': expected int, found float",
    ),
    (
        "d_float2",
        "print(I2.from(1.5))",
        "type I2 has no from for float (it has: int, str)",
    ),
    (
        "t_float1",
        "print(L1.from[float](1))",
        "type L1 has no from for float (it has: List[int?])",
    ),
    (
        "t_float2",
        "print(L2.from[float](1))",
        "type L2 has no from for float (it has: List[int?], str)",
    ),
    // A `T.from[X]` value whose `[X]` no overload takes.
    (
        "tv_float1",
        "g := I1.from[float]",
        "type I1 has no from for float (it has: int)",
    ),
    (
        "tv_float2",
        "g := I2.from[float]",
        "type I2 has no from for float (it has: int, str)",
    ),
    // A value with no fn type: the set is ambiguous.
    (
        "v_none",
        "g := I2.from",
        "type annotations needed: I2.from has 2 overloads (int, str)",
    ),
    // A fn value picks by fn assignability: `fn(int?) -> O` is no `fn(int) -> O`, one decl or two.
    (
        "v_fn_assign1",
        "f: fn(int) -> O1 = O1.from",
        "cannot assign fn(int?) -> O1 to variable of type fn(int) -> O1",
    ),
    (
        "v_fn_assign2",
        "f: fn(int) -> O2 = O2.from",
        "type O2 has no from for int (it has: int?, str)",
    ),
    // `into` with no target: no annotation, `print`'s `Any`, an open type var.
    (
        "i_bare",
        "x := V2(1).into()",
        "into on V2 needs a target type: write `.into[T]()`",
    ),
    (
        "i_any",
        "print(V2(1).into())",
        "into on V2 needs a target type: write `.into[T]()`",
    ),
    (
        "i_open",
        "print(id(V2(1).into()))",
        "into on V2 needs a target type: write `.into[T]()`",
    ),
    (
        "i_map_open",
        "xs := [V2(1)].map(V2.into)",
        "type annotations needed: V2.into has 2 overloads (float, str)",
    ),
    // A bound the type takes no overload for.
    (
        "b_none1",
        "protocol Mk:\n    fn from(x: int?) -> Self\nfn mkk[T: Mk](v: int?) -> T:\n    return T.from(v)\nr: I1 = mkk(None)",
        "type I1 does not satisfy Mk",
    ),
    (
        "b_none2",
        "protocol Mk:\n    fn from(x: int?) -> Self\nfn mkk[T: Mk](v: int?) -> T:\n    return T.from(v)\nr: I2 = mkk(None)",
        "type I2 does not satisfy Mk",
    ),
    // A bound is met exactly: `from(x: int?)` is no `From[int]`, one decl or two (the direct
    // call `O2.from(3)` wraps, `d_wrap`).
    (
        "b_wrap1",
        "c: O1 = mk(3)",
        "type O1 does not satisfy From (method 'from' has the wrong signature)",
    ),
    (
        "b_wrap2",
        "d: O2 = mk(4)",
        "type O2 does not satisfy From (method 'from' has the wrong signature)",
    ),
    (
        "b_wrap_make",
        "a: O2 = make(3)",
        "type O2 does not satisfy From (method 'from' has the wrong signature)",
    ),
    // Every builtin type name is a static head.
    (
        "h_int",
        "print(int.zz(1))",
        "type 'int' has no static method 'zz'",
    ),
    (
        "h_bool",
        "print(bool.zz(1))",
        "type 'bool' has no static method 'zz'",
    ),
    (
        "h_str",
        "print(str.zz(1))",
        "type 'str' has no static method 'zz'",
    ),
    (
        "h_width",
        "import int8 from std.ffi\nprint(int8.zz(1))",
        "type 'int8' has no static method 'zz'",
    ),
    (
        "h_alias",
        "type B = bool\nprint(B.zz(1))",
        "type 'B' has no static method 'zz'",
    ),
];

#[test]
fn conversion_pick_accept_cells() {
    let mut bad = Vec::new();
    for (tag, body, want) in ACCEPT {
        let (ok, text) = chezzi(tag, &format!("{PRELUDE}{body}\n"));
        if !ok || text.trim_end() != *want {
            bad.push(format!("[{tag}] want {want:?}\n{text}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} accept cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn conversion_pick_reject_cells() {
    let mut bad = Vec::new();
    for (tag, body, want) in REJECT {
        let (ok, text) = chezzi(tag, &format!("{PRELUDE}{body}\n"));
        if ok || !text.contains(want) {
            bad.push(format!("[{tag}] want error {want:?}\n{text}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} reject cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
