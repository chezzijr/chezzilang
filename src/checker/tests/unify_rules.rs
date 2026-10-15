//! Checker tests: protocol grid, type names are never values (U10), recover tail (U8), width fields
//! (262).
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

/// The operator form and the bound form of one protocol on one sample, one pair per needle for
/// `Contains`. Each entry is (label, operator program, bound program, expected answer if any).
fn grid_programs(protocol: &str, s: &GridSample) -> Vec<(String, String, String, Option<bool>)> {
    let head = format!("{}a := {}\n", s.prelude, s.lit);
    let bin = |p: &str, op: &str, ret: &str| {
        (
            protocol.to_string(),
            format!("{head}r := a {op} a\n"),
            format!(
                "{}fn f[T: {p}](x: T, y: T) -> {ret}:\n    return x {op} y\na := {}\nf(a, a)\n",
                s.prelude, s.lit
            ),
            None,
        )
    };
    let one = |label: &str, op_stmt: &str, bound_decl: &str, call: &str, want: Option<bool>| {
        (
            label.to_string(),
            format!("{head}{op_stmt}\n"),
            format!("{}{bound_decl}a := {}\n{call}\n", s.prelude, s.lit),
            want,
        )
    };
    match protocol {
        "Add" => vec![bin("Add", "+", "T")],
        "Sub" => vec![bin("Sub", "-", "T")],
        "Mul" => vec![bin("Mul", "*", "T")],
        "Div" => vec![bin("Div", "/", "T")],
        "Mod" => vec![bin("Mod", "%", "T")],
        "BitAnd" => vec![bin("BitAnd", "&", "T")],
        "BitOr" => vec![bin("BitOr", "|", "T")],
        "BitXor" => vec![bin("BitXor", "^", "T")],
        "Shl" => vec![bin("Shl", "<<", "T")],
        "Shr" => vec![bin("Shr", ">>", "T")],
        "Eq" => vec![bin("Eq", "==", "bool")],
        "Comparable" => vec![bin("Comparable", "<", "bool")],
        "Neg" => vec![one(
            "Neg",
            "r := -a",
            "fn f[T: Neg](x: T) -> T:\n    return -x\n",
            "f(a)",
            None,
        )],
        "Stringable" => vec![one(
            "Stringable",
            "r := str(a)",
            "fn f[T: Stringable](x: T) -> str:\n    return x.str()\n",
            "f(a)",
            None,
        )],
        "Hashable" => vec![one(
            "Hashable",
            "r := {a: 1}",
            "fn f[T: Hashable](x: T):\n    pass\n",
            "f(a)",
            None,
        )],
        "Contains" => {
            let mut v = Vec::new();
            let mut needles: Vec<((&str, &str), bool)> =
                s.items.iter().map(|n| (*n, true)).collect();
            match s.bad_item {
                Some(n) => needles.push((n, false)),
                None => needles.push((("1", "int"), false)),
            }
            for ((lit, ty), want) in needles {
                v.push(one(
                    &format!("Contains[{ty}] {lit}"),
                    &format!("r := {lit} in a"),
                    &format!(
                        "fn f[C: Contains[{ty}]](c: C, e: {ty}) -> bool:\n    return e in c\n"
                    ),
                    &format!("f(a, {lit})"),
                    Some(want),
                ));
            }
            v
        }
        "Iterable" | "Iterator" => {
            let e = s.elem.unwrap_or("int");
            vec![one(
                protocol,
                &format!("for v in a:\n    e: {e} = v"),
                &format!("fn f[C: {protocol}[{e}]](c: C):\n    for v in c:\n        e: {e} = v\n"),
                "f(a)",
                if protocol == "Iterable" {
                    Some(s.elem.is_some())
                } else {
                    None
                },
            )]
        }
        "Index" => {
            let (key, kt, vt) = s.index.unwrap_or(("0", "int", "int"));
            vec![one(
                "Index",
                &format!("k: {kt} = {key}\nr := a[k]"),
                &format!("fn f[C: Index[{kt}, {vt}]](c: C, k: {kt}) -> {vt}:\n    return c[k]\n"),
                &format!("f(a, {key})"),
                Some(s.index.is_some()),
            )]
        }
        "IndexSet" => {
            let (key, kt, vt) = s.index.unwrap_or(("0", "int", "int"));
            let val = s.set_val.unwrap_or("1");
            vec![one(
                "IndexSet",
                &format!("a[{key}] = {val}"),
                &format!("fn f[C: IndexSet[{kt}, {vt}]](c: C, k: {kt}, v: {vt}):\n    c[k] = v\n"),
                &format!("f(a, {key}, {val})"),
                Some(s.set_val.is_some()),
            )]
        }
        "Slice" => {
            let r = s.slice.unwrap_or("int");
            vec![one(
                "Slice",
                "r := a[0..1]",
                &format!("fn f[C: Slice[{r}]](c: C) -> {r}:\n    return c[0..1]\n"),
                "f(a)",
                Some(s.slice.is_some()),
            )]
        }
        other => panic!("grid has no template for protocol {other}"),
    }
}

/// The grid's checker: a program that imports a native std module (`Shared`, `RwShared`, `Atomic`)
/// resolves only through `build_graph`; every other program uses the single-module check.
fn grid_check(src: &str) -> Vec<CheckError> {
    if src.contains("import std.") {
        check_entry(src)
    } else {
        check_src(src)
    }
}

/// TICKET-257 (owner rule U5) — an operator accepts a type exactly when the type satisfies the
/// operator's protocol. Every cell of `RecvKind::ALL` x `HOOKS` runs the operator form and the bound
/// form and requires the same answer, plus the expected answer where the sample states one.
#[test]
fn builtin_operator_and_bound_agree_grid() {
    use super::proto::RecvKind;
    let mut red: Vec<String> = Vec::new();
    for row in HOOKS {
        for kind in RecvKind::ALL {
            for sample in grid_samples(kind) {
                for (label, op, bound, want) in grid_programs(row.protocol, &sample) {
                    let op_ok = grid_check(&op).is_empty();
                    let bound_ok = grid_check(&bound).is_empty();
                    let agree = if row.protocol == "Iterator" {
                        !bound_ok || op_ok
                    } else {
                        op_ok == bound_ok
                    };
                    let want_ok = want.is_none_or(|w| w == bound_ok);
                    if !agree || !want_ok {
                        let f = |b: bool| if b { "ok" } else { "err" };
                        red.push(format!(
                            "{} x {} x {} [{}]: operator={} bound={} want={}",
                            row.protocol,
                            kind,
                            label,
                            sample.lit,
                            f(op_ok),
                            f(bound_ok),
                            want.map_or("-", f),
                        ));
                    }
                }
            }
        }
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// TICKET-257 (owner decision U16) — membership has one name, `contains`. `has` on a Map or a Set is
/// a migration error that names the replacement.
#[test]
fn map_and_set_has_is_a_migration_error_naming_contains() {
    for src in [
        "m := {1: 2}\nb := m.has(1)\n",
        "s := {1, 2}\nb := s.has(1)\n",
    ] {
        let errs = check_src(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("write") && e.message.contains("contains(")),
            "expected a migration error naming contains, got: {errs:?}"
        );
    }
    ok("m := {1: 2}\nb := m.contains(1)\ns := {1, 2}\nc := s.contains(1)\n");
}

/// TICKET-257 — `+` and `-` on builtin types ask `Add` and `Sub`. What the grant must NOT allow:
/// mixed element types, a set `+`, a str `-`, str + bytes.
#[test]
fn builtin_add_sub_accept_and_reject_boundaries() {
    ok(
        "a := b\"a\" + bytearray(b\"b\")\nb := bytearray(b\"a\") + b\"b\"\nc := [] + [1]\nd := {1, 2} - {1}\ne := \"a\" + \"b\"\n",
    );
    rejects("x := [1] + [\"a\"]\n", "cannot apply + to");
    rejects("x := {1} + {2}\n", "cannot apply + to");
    rejects("x := \"a\" - \"b\"\n", "cannot apply - to");
    rejects("x := b\"a\" + \"b\"\n", "cannot apply + to");
    rejects("x := \"a\" + 1\n", "cannot apply + to");
    rejects("x := [1] - [1]\n", "cannot apply - to");
}

/// TICKET-257 — the bit operators ask `BitAnd`..`Shr`. A struct with `bitor` gets `|` only.
#[test]
fn bit_operator_protocols_accept_and_reject_boundaries() {
    let decl = "struct B:\n    n: int\n    fn bitor(self, other: Self) -> Self:\n        return B(self.n)\n";
    ok(&format!(
        "{decl}x := B(1) | B(2)\ny := {{1}} | {{2}}\nz := 1 << 2\n"
    ));
    rejects(&format!("{decl}x := B(1) & B(2)\n"), "bitwise operator &");
    rejects("x := 1.5 | 2.5\n", "bitwise operator |");
    rejects("x := \"a\" & \"b\"\n", "bitwise operator &");
    rejects("x := {1} << {2}\n", "bitwise operator <<");
    ok(
        "fn orr[T: BitOr](a: T, b: T) -> T:\n    return a | b\nx := orr({1}, {2})\ny := orr(1, 2)\n",
    );
    rejects(
        "fn orr[T: BitOr](a: T, b: T) -> T:\n    return a | b\nx := orr(1.5, 2.5)\n",
        "BitOr",
    );
}

/// TICKET-257 — bytes and bytearray order like str, through `Comparable`; handles are `Eq`.
#[test]
fn bytes_order_and_handles_compare_through_protocols() {
    ok(
        "a := b\"a\" < b\"b\"\nb := bytearray(b\"a\") < bytearray(b\"b\")\nc := b\"a\" < bytearray(b\"b\")\n",
    );
    ok(
        "fn lt[T: Comparable](a: T, b: T) -> bool:\n    return a < b\nx := lt(b\"a\", b\"b\")\ny := lt(bytearray(b\"a\"), bytearray(b\"b\"))\n",
    );
    rejects("x := b\"a\" < \"b\"\n", "cannot compare");
    rejects("x := b\"a\" < 1\n", "cannot compare");
    rejects("x := \"a\" < b\"b\"\n", "cannot compare");
    entry_ok(
        "import std.concurrency\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nch := Channel[int](1)\nx := same(ch, ch)\n",
    );
}

/// TICKET-259 (U10) — a type name is never a value. One grid: type-name kind x value position,
/// every cell the same "is a type, not a value" error. The builtin kinds come from
/// `BuiltinType::ALL` and `std.ffi`'s declared types, not from a hand list.
#[test]
fn type_name_is_never_a_value_grid() {
    const PRELUDE: &str = "import am
import std.ffi
struct P:
    x: int
    fn mk(a: int) -> P:
        return P(a)
enum E[T]:
    V(T)
    A
protocol Pr:
    fn name(self) -> str
type Al = P
type Ci = int
type Lt = List[int]
type Tp = (int, str)
type Fa = fn(int) -> int
type Qa = Pr
import V from E
fn top(a: int) -> int:
    return a
";
    const LIB: &str = "type C = int
type L = List[int]
struct S:
    x: int
enum F:
    W(int)
";
    let positions = [
        "print([1, 2].map(T).len())\n",
        "v := T\n",
        "fn f():\n    return T\n",
        "xs := [T]\n",
        "t := (T, 1)\n",
        "m := {1: T}\n",
        "g := fn(e): T\n",
    ];
    let run =
        |body: &str| check_files(&[("am.chz", LIB), ("main.chz", &format!("{PRELUDE}{body}"))]);
    let is_type_error = |errs: &[CheckError]| {
        errs.iter()
            .any(|e| e.message.contains("is a type, not a value"))
    };

    let mut kinds: Vec<String> = [
        "P",
        "E",
        "Pr",
        "Al",
        "Ci",
        "Lt",
        "Tp",
        "Fa",
        "Qa",
        "E.V",
        "E[int].V",
        "V",
        "List[int]",
    ]
    .iter()
    .map(|k| k.to_string())
    .collect();
    kinds.extend(BuiltinType::ALL.iter().map(|b| b.name().to_string()));
    kinds.extend(
        crate::native::ffi::declared_types()
            .iter()
            .filter(|d| BuiltinType::from_name(&d.name).is_none())
            .map(|d| d.name.clone()),
    );
    // A `native type` export read through its module (`ffi.int32`, `ffi.ptr`).
    kinds.extend(
        crate::native::ffi::declared_types()
            .iter()
            .map(|d| format!("ffi.{}", d.name)),
    );
    kinds.extend(
        ["am.C", "am.L", "am.S", "am.F.W"]
            .iter()
            .map(|k| k.to_string()),
    );

    let mut missed = Vec::new();
    for kind in &kinds {
        for pos in positions {
            let body = pos.replace('T', kind);
            let errs = run(&body);
            if !is_type_error(&errs) {
                missed.push(format!("{kind} in {pos:?}: {errs:?}"));
            }
        }
    }

    // `Self` is a type inside an inherent method body. A nested fn body resets it
    // (`check_fn_body` sets `current_self_ty` to `None`), so `x: Self` there is `unknown type`
    // too and the `fn f():` position is no cell.
    for pos in positions.iter().filter(|p| !p.starts_with("fn f()")) {
        let indented: String = pos
            .replace('T', "Self")
            .lines()
            .map(|l| format!("        {l}\n"))
            .collect();
        // The parser accepts no default method body in a protocol, so the struct method is the one
        // place `Self` is a type inside a body.
        let src = format!("struct SS:\n    x: int\n    fn m(self):\n{indented}");
        let errs = check_src(&src);
        if !is_type_error(&errs) {
            missed.push(format!("Self in a struct method, {pos:?}: {errs:?}"));
        }
    }

    // Every builtin type still resolves as an annotation: the value rule must not touch a type position.
    for b in BuiltinType::ALL {
        let n = b.name();
        let src = match b {
            BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray
            | BuiltinType::Range => format!("fn f(x: {n}):\n    pass\n"),
            BuiltinType::Iterator | BuiltinType::List | BuiltinType::Set | BuiltinType::Channel => {
                format!("fn f(x: {n}[int]):\n    pass\n")
            }
            BuiltinType::Map => "fn f(x: Map[int, int]):\n    pass\n".to_string(),
            BuiltinType::Socket | BuiltinType::Listener => {
                format!("import std.net\nfn f(x: {n}):\n    pass\n")
            }
            BuiltinType::Writer | BuiltinType::Reader => {
                format!("import std.io\nfn f(x: {n}):\n    pass\n")
            }
            BuiltinType::Ptr => format!("import std.ffi\nfn f(x: {n}):\n    pass\n"),
            BuiltinType::OwnedStr => {
                "import std.ffi\nextern \"libc\":\n    fn getenv(name: str) -> owned_str\n"
                    .to_string()
            }
            BuiltinType::Shared | BuiltinType::RwShared | BuiltinType::Atomic => {
                format!("import std.concurrency\nfn f(x: {n}[int]):\n    pass\n")
            }
            BuiltinType::AtomicInt | BuiltinType::Executor => {
                format!("import std.concurrency\nfn f(x: {n}):\n    pass\n")
            }
        };
        let errs = check_files(&[("main.chz", &src)]);
        if !errs.is_empty() {
            missed.push(format!("annotation {n}: {errs:?}"));
        }
    }
    assert!(
        missed.is_empty(),
        "type name read as a value was not rejected as a type:\n{}",
        missed.join("\n")
    );

    // Still values: a payload-less variant, a top-level fn, a method through its type, a lambda, a
    // nested fn. Other errors (`cannot infer ...`, `declares no return type`) are allowed.
    let mut wrong = Vec::new();
    for kind in ["E.A", "P.mk", "top", "fn(e: int) -> int: e"] {
        for pos in positions {
            let errs = run(&pos.replace('T', kind));
            if is_type_error(&errs) {
                wrong.push(format!("{kind} in {pos:?}: {errs:?}"));
            }
        }
    }
    let errs = run(
        "fn outer():\n    fn nf(a: int) -> int:\n        return a\n    v := nf\n    print(v(1))\n",
    );
    if is_type_error(&errs) {
        wrong.push(format!("nested fn: {errs:?}"));
    }
    assert!(
        wrong.is_empty(),
        "a value was reported as a type:\n{}",
        wrong.join("\n")
    );
}

// TICKET-260 (U8): a `recover:` block has no tail value; its type is `!`.
#[test]
fn recover_block_tail_value_is_rejected() {
    rejects(
        "s := recover:\n    x := 10\n    x / 2\nt: int! = s\nprint(t)\n",
        "has type `!`",
    );
}

// TICKET-260 (U8): the block form is `!` -- the error or nothing -- so it binds to a `!` slot.
#[test]
fn recover_block_is_none_bang() {
    ok("s := recover:\n    x := 1\n    x + 1\nt: ! = s\nprint(t)\n");
}

/// TICKET-261 (U9): `for a, b in X` unpacks X's element for every iterable X, so a Set of
/// 2-tuples binds `a` and `b` as CPython does. `for_bindings` demands a map today.
#[test]
fn for_pair_unpacks_a_set_of_tuples() {
    ok("fn main():\n    for a, b in {(1, \"x\")}:\n        print(a, b)\n");
}

/// TICKET-262 — owner rules 1 and 2 over the whole matrix: every resolver position x every `std.ffi`
/// width x {in-range constant, out-of-range constant, out-of-range runtime value}, checked and run.
/// Rule 1: a width type outside a C signature or a struct field's own type is a compile error.
/// Rule 2: every store into a width slot is range-checked; a constant is a compile error, a runtime
/// value is a fault with the C boundary's own text. CPython ctypes wraps silently
/// (`ctypes.c_int8(300).value` -> `44`); Chezzi faults.
#[test]
fn width_position_grid() {
    /// What a run must do: print exactly this, fault with this text, or print a line holding this.
    enum Ran {
        Out(String),
        Fault(String),
        OutHas(String),
    }
    use super::sig::TypePos;
    use crate::native::cffi::{CType, width_ctype};

    #[derive(Clone, Copy, PartialEq)]
    enum Shape {
        /// The width type itself is the rule-1 error, whatever the value.
        Outside,
        /// `{K}` is stored once: constant out of range = check error, runtime = fault.
        Store,
        /// `{K}` is stored the second time round a loop, so the store takes the field IC fast path.
        Loop,
        /// A constant default: no runtime value exists.
        ConstOnly,
        /// A C call argument: the constant is checked, the runtime value is the C boundary's.
        Call,
        /// A signature with no value dimension.
        CheckOnly,
        /// `s.q += {K}`: checks clean (DEC-258), faults at run time.
        Compound,
        /// `json.decode[S]` with the value in the JSON text: the `Err` holds the fault text.
        Json,
        /// A C read: prints the zero that was stored.
        Read,
    }
    use Shape::*;

    // An exhaustive match: a new `TypePos` variant does not compile until it has templates.
    // `{W}` width, `{K}` value, `{Z}` zero, `{J}` JSON number text.
    fn templates(pos: TypePos) -> &'static [(&'static str, Shape)] {
        match pos {
            TypePos::Value => &[
                ("x: {W} = {K}\nprint(x)\n", Outside),
                ("fn f(a: {W}) -> None:\n    print(a)\nf({K})\n", Outside),
                ("fn f(a: {W} = {K}) -> None:\n    print(a)\nf()\n", Outside),
                ("c := fn(a: {W}) -> int: 0\nprint(c({K}))\n", Outside),
                (
                    "struct M:\n    n: int\n    fn m(self, a: {W}) -> None:\n        print(a)\nM(1).m({K})\n",
                    Outside,
                ),
                ("protocol Pr:\n    fn m(self, a: {W}) -> None\n", Outside),
                ("enum E:\n    A({W})\nprint(E.A({K}))\n", Outside),
                ("l: List[{W}] = [{K}]\nprint(l)\n", Outside),
                ("o: {W}? = ?({K})\nprint(o)\n", Outside),
                ("t: ({W}, int) = ({K}, 1)\nprint(t)\n", Outside),
                ("m: Map[str, {W}] = {\"a\": {K}}\nprint(m)\n", Outside),
                (
                    "struct Bx[T]:\n    v: T\nb := Bx[{W}]({K})\nprint(b.v)\n",
                    Outside,
                ),
                (
                    "struct Bx[T]:\n    v: T\n    fn make(x: T) -> Bx[T]:\n        return Bx[T](x)\nprint(Bx[{W}].make({K}).v)\n",
                    Outside,
                ),
                (
                    "struct Bx[T]:\n    v: T\n    fn make(x: T) -> Bx[T]:\n        return Bx[T](x)\nf := Bx[{W}].make\nprint(f({K}).v)\n",
                    Outside,
                ),
                (
                    "fn id[T](x: T) -> T:\n    return x\nprint(id[{W}]({K}))\n",
                    Outside,
                ),
                ("type A = {W}\n", Outside),
                ("x: ffi.{W} = {K}\nprint(x)\n", Outside),
                (
                    "G: {W} = {Z}\nfn setg() -> None:\n    G = {K}\nsetg()\nprint(G)\n",
                    Outside,
                ),
            ],
            TypePos::Void => &[
                ("fn r() -> {W}:\n    return {K}\nprint(r())\n", Outside),
                ("fn r() -> {W}!str:\n    return {K}\nprint(r())\n", Outside),
                ("c := fn() -> {W}: {K}\nprint(c())\n", Outside),
            ],
            TypePos::CParam => &[
                (
                    "p := ffi.alloc(8)\nffi.store_{W}(p, {K})\nprint(ffi.load_{W}(p))\n",
                    Store,
                ),
                (
                    "extern \"libc.so.6\":\n    fn abs(x: {W}) -> int\nprint(abs({K}))\n",
                    Call,
                ),
                (
                    "extern \"libc.so.6\":\n    fn qsort(cb: fn({W}) -> {W}) -> int\n",
                    CheckOnly,
                ),
            ],
            TypePos::CRet => &[
                (
                    "extern \"libc.so.6\":\n    fn abs(x: int) -> {W}\n",
                    CheckOnly,
                ),
                (
                    "p := ffi.alloc(8)\nffi.store_{W}(p, {Z})\nx := ffi.load_{W}(p)\nprint(x)\n",
                    Read,
                ),
            ],
            TypePos::Field => &[
                ("struct S:\n    q: {W}\ns := S({K})\nprint(s.q)\n", Store),
                ("struct S:\n    q: {W}\ns := S(q={K})\nprint(s.q)\n", Store),
                (
                    "struct S:\n    q: {W}\ns := S({Z})\ns.q = {K}\nprint(s.q)\n",
                    Store,
                ),
                (
                    "struct S:\n    q: {W}\ns := S({Z})\nfor v in [{Z}, {K}]:\n    s.q = v\nprint(s.q)\n",
                    Loop,
                ),
                (
                    "struct S:\n    q: {W}\ns := S({Z})\ns.q += {K}\nprint(s.q)\n",
                    Compound,
                ),
                (
                    "struct S:\n    q: {W} = {K}\ns := S()\nprint(s.q)\n",
                    ConstOnly,
                ),
                (
                    "struct S:\n    q: ffi.{W}\ns := S({K})\nprint(s.q)\n",
                    Store,
                ),
                (
                    "struct S:\n    q: {W}\ntxt := r'{\"q\": {J}}'\nr := json.decode[S](txt)\nmatch r:\n    ?s: print(\"ok\", s.q)\n    !e: print(\"err\", e)\n",
                    Json,
                ),
                (
                    "struct S:\n    q: {W}\nextern \"libc.so.6\":\n    fn abs(s: S) -> int\nx := S({K})\nprint(x.q)\n",
                    Store,
                ),
                // A fn type under a field is a program fn type, not a C callback.
                ("struct S:\n    cb: fn({W}) -> int\n", Outside),
            ],
        }
    }

    // `TypePos::ALL` must hold every variant: the same exhaustive shape as `templates`.
    let mut seen = [false; 5];
    for p in TypePos::ALL {
        let i = match p {
            TypePos::Value => 0,
            TypePos::Void => 1,
            TypePos::CParam => 2,
            TypePos::CRet => 3,
            TypePos::Field => 4,
        };
        seen[i] = true;
    }
    assert!(seen.iter().all(|s| *s), "TypePos::ALL misses a variant");

    let run = |src: &str| -> Result<String, String> {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (out, _err, result, _code) = crate::vm::run_file(&entry);
        match result {
            Ok(()) => Ok(out.trim().to_string()),
            Err(e) => Err(format!("{e}")),
        }
    };
    let rule1 = "describes the C boundary only";
    let mut red: Vec<String> = Vec::new();
    for d in crate::native::ffi::declared_types() {
        if d.scalar.is_none() {
            continue;
        }
        let w = d.name.as_str();
        let ct = width_ctype(w).unwrap();
        let float = ct == CType::Float32;
        let (zero, in_k) = if float { ("0.0", "1.5") } else { ("0", "1") };
        // The out-of-range constant, and the out-of-range runtime value (`uint64` takes any int as
        // its bit pattern at run time; `int64` has no value out of range).
        let oor_const: Option<String> = if float {
            Some("1e39".to_string())
        } else if ct == CType::Int64 {
            None
        } else if ct == CType::UInt64 {
            Some("-1".to_string())
        } else {
            ct.int_range().map(|(_, hi)| (hi + 1).to_string())
        };
        let oor_run: Option<String> = if ct == CType::UInt64 {
            None
        } else {
            oor_const.clone()
        };
        let big_ty = if float { "float" } else { "int" };
        let big_val = oor_run.clone().unwrap_or_else(|| in_k.to_string());
        let prelude = format!(
            "import std.ffi\nimport std.json\nimport {w} from std.ffi\nfn big() -> {big_ty}:\n    return {big_val}\n"
        );
        let fail_const = |k: &str| format!("constant {k} does not fit {w} {}", ct.range_text());
        let fail_run = |k: &str| format!("value {k} does not fit {w} {}", ct.range_text());
        for pos in TypePos::ALL {
            for (tpl, shape) in templates(pos) {
                if !pos.width_ok() && *shape != Outside {
                    red.push(format!(
                        "{tpl:?}: a position that rejects widths has a non-Outside template"
                    ));
                    continue;
                }
                let build = |k: &str, j: &str| {
                    let body = tpl
                        .replace("{W}", w)
                        .replace("{Z}", zero)
                        .replace("{K}", k)
                        .replace("{J}", j);
                    format!("{prelude}{body}")
                };
                let mut cell = |what: &str,
                                src: String,
                                check: Result<(), String>,
                                ran: Option<Ran>| {
                    // `check`: Ok = must be clean, Err(needle) = an error must contain it.
                    let errs = check_entry(&src);
                    let bad = match &check {
                        Ok(()) if !errs.is_empty() => Some(format!("want clean, got {errs:?}")),
                        Err(n) if !errs.iter().any(|e| e.message.contains(n.as_str())) => {
                            Some(format!("want error {n:?}, got {errs:?}"))
                        }
                        Ok(()) | Err(_) => None,
                    };
                    if let Some(b) = bad {
                        red.push(format!("{w} {what}: {b}\n{src}"));
                        return;
                    }
                    if check.is_err() {
                        return;
                    }
                    match (ran, run(&src)) {
                        (None, _) => {}
                        (Some(Ran::Out(want)), Ok(out)) if out == want => {}
                        (Some(Ran::Fault(n)), Err(f)) if f.contains(n.as_str()) => {}
                        (Some(Ran::OutHas(n)), Ok(out)) if out.contains(n.as_str()) => {}
                        (Some(Ran::Out(want)), got) => {
                            red.push(format!("{w} {what}: run want {want:?}, got {got:?}\n{src}"))
                        }
                        (Some(Ran::Fault(n) | Ran::OutHas(n)), got) => red.push(format!(
                            "{w} {what}: run want a fault or output containing {n:?}, got {got:?}\n{src}"
                        )),
                    }
                };
                let uses_k = tpl.contains("{K}");
                match shape {
                    Outside => {
                        let mut vals = vec![("in-range", in_k.to_string())];
                        if uses_k {
                            if let Some(k) = &oor_const {
                                vals.push(("out-of-range constant", k.clone()));
                            }
                            vals.push(("runtime", "big()".to_string()));
                        }
                        for (what, k) in vals {
                            cell(
                                &format!("{tpl:?} {what}"),
                                build(&k, in_k),
                                Err(rule1.to_string()),
                                None,
                            );
                        }
                    }
                    Store | Loop => {
                        cell(
                            &format!("{tpl:?} in-range"),
                            build(in_k, in_k),
                            Ok(()),
                            Some(Ran::Out(in_k.to_string())),
                        );
                        if let Some(k) = &oor_const {
                            // A `Loop` constant rides a list, so it meets the slot at run time.
                            if *shape == Loop {
                                if oor_run.is_some() {
                                    cell(
                                        &format!("{tpl:?} out-of-range constant"),
                                        build(k, in_k),
                                        Ok(()),
                                        Some(Ran::Fault(fail_run(k))),
                                    );
                                }
                            } else {
                                cell(
                                    &format!("{tpl:?} out-of-range constant"),
                                    build(k, in_k),
                                    Err(fail_const(k)),
                                    None,
                                );
                            }
                        }
                        if let Some(k) = &oor_run {
                            cell(
                                &format!("{tpl:?} runtime"),
                                build("big()", in_k),
                                Ok(()),
                                Some(Ran::Fault(fail_run(k))),
                            );
                        }
                    }
                    Compound => {
                        if float {
                            continue;
                        }
                        cell(
                            &format!("{tpl:?} in-range"),
                            build(in_k, in_k),
                            Ok(()),
                            Some(Ran::Out(in_k.to_string())),
                        );
                        if let Some(k) = &oor_run {
                            for v in [k.as_str(), "big()"] {
                                cell(
                                    &format!("{tpl:?} {v}"),
                                    build(v, in_k),
                                    Ok(()),
                                    Some(Ran::Fault(fail_run(k))),
                                );
                            }
                        }
                    }
                    ConstOnly => {
                        cell(
                            &format!("{tpl:?} in-range"),
                            build(in_k, in_k),
                            Ok(()),
                            Some(Ran::Out(in_k.to_string())),
                        );
                        if let Some(k) = &oor_const {
                            cell(
                                &format!("{tpl:?} out-of-range constant"),
                                build(k, in_k),
                                Err(fail_const(k)),
                                None,
                            );
                        }
                    }
                    Call => {
                        cell(
                            &format!("{tpl:?} in-range"),
                            build(in_k, in_k),
                            Ok(()),
                            None,
                        );
                        if let Some(k) = &oor_const {
                            cell(
                                &format!("{tpl:?} out-of-range constant"),
                                build(k, in_k),
                                Err(fail_const(k)),
                                None,
                            );
                        }
                        cell(
                            &format!("{tpl:?} runtime"),
                            build("big()", in_k),
                            Ok(()),
                            None,
                        );
                    }
                    CheckOnly => {
                        cell(&format!("{tpl:?}"), build(in_k, in_k), Ok(()), None);
                    }
                    Json => {
                        cell(
                            &format!("{tpl:?} in-range"),
                            build(in_k, in_k),
                            Ok(()),
                            Some(Ran::Out(format!("ok {in_k}"))),
                        );
                        if let Some(k) = &oor_run {
                            cell(
                                &format!("{tpl:?} out-of-range"),
                                build(k, k),
                                Ok(()),
                                Some(Ran::OutHas(fail_run(k))),
                            );
                        }
                    }
                    Read => {
                        cell(
                            &format!("{tpl:?}"),
                            build(in_k, in_k),
                            Ok(()),
                            Some(Ran::Out(zero.to_string())),
                        );
                    }
                }
            }
        }
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// TICKET-262 — the field-width table is keyed by the runtime identity key, so two modules that both
/// declare `S` keep separate checks: `lib.S` has an `int8` field and faults on 300, the entry's own
/// `S` has an `int` field and holds 300. Fails if the table were keyed by the bare name.
#[test]
fn width_field_check_follows_the_identity_key_across_a_name_clash() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "import int8 from std.ffi\n\nstruct S:\n    q: int8\n",
    );
    let entry = t.write(
        "main.chz",
        "import lib\n\nstruct S:\n    q: int\n\na := S(300)\nprint(a.q)\nb := lib.S(100)\nfor v in [1, 300]:\n    b.q = v\nprint(\"unreached\")\n",
    );
    let (out, _err, result, _code) = crate::vm::run_file(&entry);
    assert_eq!(out.trim(), "300");
    let fault = format!("{}", result.expect_err("lib.S(int8) must fault on 300"));
    assert!(
        fault.contains("value 300 does not fit int8 (-128..127)"),
        "{fault}"
    );
}

#[test]
fn width_type_local_annotation_rejected() {
    // TICKET-262 rule 1: a width type describes the C boundary only. A local annotation is an error
    // that names the rule and the fix (`int` / `float`).
    entry_rejects(
        "import int8 from std.ffi\nfn main():\n    x: int8 = 5\n    print(x)\n",
        "describes the C boundary only",
    );
}

/// `_` is the blank identifier at the `else` guard's error binder too: `else _:` binds nothing, so a
/// read of `_` in the guard body is the blank-read error. It was check-OK and then a compiler panic
/// (`global '_' has no slot`): the checker bound `_` as a local and the compiler did not.
#[test]
fn else_guard_blank_binder_binds_nothing() {
    let guarded = |body: &str| {
        format!(
            "fn f() -> int!:\n    return !\"boom\"\nfn main():\n    x := f() else _:\n        {body}\n        return\n    print(x)\nmain()\n"
        )
    };
    rejects(&guarded("print(_)"), "cannot use '_' as a value");
    ok(&guarded("print(\"err\")"));
}
