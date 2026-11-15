//! Method dispatch, one cell per (receiver kind, column): a declared method, an intrinsic method
//! (a std protocol's method the kind has with no declaration behind it) and a miss. The kinds are
//! every runtime shape a receiver can have: the inline scalars and each `Obj` variant, each handle
//! kind among them ([`kind_of`] names every variant, so a new one fails the build until it has
//! cells). The declared and intrinsic cells run checked programs through the `chezzi` binary; a
//! checked program never reaches a miss, so the miss cells compile without the checker (the
//! compiler is type-blind) and pin the VM's miss text.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use chezzi::vm::heap::Obj;
use grid_cell::{Cell, Expect, run_grid};

const CONC: &str = "import std.concurrency\nimport Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\n";
const DECLS: &str = "struct P:\n    x: int\n    f: fn(int) -> int\n    fn get(self) -> int:\n        return self.x\nenum E:\n    A\n    B\n    fn name(self) -> str:\n        return \"e\"\nfn dbl(x: int) -> int:\n    return x * 2\nfn noop():\n    pass\nfn ho() -> int?:\n    return 1\nfn hr() -> int!str:\n    return 1\nimport Iterator from std.iter\nfn gen() -> Iterator[int]:\n    yield 1\n    yield 2\n";

/// The kind of an `Obj`, by the name its cells use. Every variant is named; the inline scalars
/// (`int`, `float`, `bool`, `None`) have no `Obj` and are listed in [`KINDS`] beside these.
fn kind_of(o: &Obj) -> &'static str {
    match o {
        Obj::Str(_) => "str",
        Obj::Bytes(_) => "bytes",
        Obj::ByteArray(_) => "bytearray",
        Obj::BigInt(_) => "bigint",
        // A float is always a Float-tagged value, never a receiver `Obj` (`Value::as_obj`).
        Obj::FloatBox(_) => "float",
        Obj::Iter { .. } => "cursor",
        Obj::List(_) => "list",
        Obj::Tuple(_) => "tuple",
        Obj::Map(_) => "map",
        Obj::Set(_) => "set",
        Obj::Struct { .. } => "struct",
        Obj::Enum { .. } => "enum",
        // A cell is never a receiver: a read of a captured local loads its value first.
        Obj::Cell(_) => "cell",
        Obj::Func { .. } | Obj::Closure { .. } | Obj::Native { .. } | Obj::Builtin(_) => "fn",
        Obj::Cffi(_) => "fn",
        Obj::Module(_) => "module",
        Obj::Ptr(_) => "ptr",
        Obj::Channel(_) => "channel",
        Obj::Shared(_) => "Shared",
        Obj::RwShared(_) => "RwShared",
        Obj::Atomic(_) => "Atomic",
        Obj::AtomicInt(_) => "AtomicInt",
        Obj::Executor(_) => "Executor",
        Obj::Socket(_) => "Socket",
        Obj::Listener(_) => "Listener",
        Obj::Writer(_) => "Writer",
        Obj::Reader(_) => "Reader",
        Obj::Generator(..) => "generator",
    }
}

/// Every kind with cells. `cell` and `ptr` have none: a cell is never a receiver, and a `ptr` is
/// made only by an `extern` call (its methods are its `Eq`/`Stringable` rows, which the table
/// test reads).
const KINDS: &[&str] = &[
    "int",
    "float",
    "bool",
    "None",
    "bigint",
    "str",
    "bytes",
    "bytearray",
    "list",
    "map",
    "set",
    "tuple",
    "struct",
    "enum",
    "option",
    "result",
    "fn",
    "channel",
    "cursor",
    "generator",
    "Shared",
    "RwShared",
    "Atomic",
    "AtomicInt",
    "Executor",
    "Socket",
    "Listener",
    "Writer",
    "Reader",
    "module",
];

/// (kind, setup lines in `main`, receiver expression) — the setup binds what the receiver needs.
fn receivers() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("int", "", "7"),
        ("float", "", "1.5"),
        ("bool", "", "true"),
        ("None", "", "noop()"),
        ("bigint", "", "4611686018427387904"),
        ("str", "", "\"abc\""),
        ("bytes", "", "b\"ab\""),
        ("bytearray", "", "bytearray.from_iter(b\"ab\")"),
        ("list", "", "[3, 1]"),
        ("map", "", "{\"a\": 1}"),
        ("set", "", "Set.from_iter([1, 2])"),
        ("tuple", "", "(1, 2)"),
        ("struct", "", "P(4, dbl)"),
        ("enum", "", "E.A"),
        ("option", "", "ho()"),
        ("result", "", "hr()"),
        ("fn", "", "dbl"),
        ("channel", "    c := Channel[int](2)\n    c.send(1)\n", "c"),
        ("cursor", "", "[5, 6].iter()"),
        ("generator", "", "gen()"),
        ("Shared", "", "Shared(3)"),
        ("RwShared", "", "RwShared(3)"),
        ("Atomic", "", "Atomic(2)"),
        ("AtomicInt", "", "AtomicInt(9)"),
        ("Executor", "", "Executor()"),
        (
            "Socket",
            "    l := net.listen(\"127.0.0.1:0\") else: panic(\"listen\")\n    addr := l.addr() else: panic(\"addr\")\n    so := net.connect(addr) else: panic(\"connect\")\n",
            "so",
        ),
        (
            "Listener",
            "    l := net.listen(\"127.0.0.1:0\") else: panic(\"listen\")\n",
            "l",
        ),
        ("Writer", "", "io.stdout()"),
        (
            "Reader",
            "    w := io.create(\"f.txt\") else: panic(\"create\")\n    _ := w.write(\"line1\\n\")\n    _ := w.close()\n    rd := io.open(\"f.txt\") else: panic(\"open\")\n",
            "rd",
        ),
        ("module", "", "math"),
    ]
}

fn program(setup: &str, body: &str) -> String {
    format!(
        "{CONC}import std.io\nimport std.net\nimport std.math\n{DECLS}fn main():\n{setup}    r := {body}\n    print(r)\nmain()\n"
    )
}

fn cell(name: String, src: String, prints: &str) -> Cell {
    Cell {
        name,
        files: vec![("main.chz".to_string(), src)],
        expect: Expect::Prints(prints.to_string()),
    }
}

/// (kind, declared call or `None` when the kind declares no method, its output, intrinsic call,
/// its output). `{r}` is the receiver.
type Row = (
    &'static str,
    Option<(&'static str, &'static str)>,
    Option<(&'static str, &'static str)>,
);

fn rows() -> Vec<Row> {
    vec![
        (
            "int",
            Some(("{r}.to_float()", "7.0")),
            Some(("{r}.str()", "7")),
        ),
        (
            "float",
            Some(("{r}.to_int()", "1")),
            Some(("{r}.add(1.0)", "2.5")),
        ),
        ("bool", None, Some(("{r}.eq(false)", "false"))),
        ("None", None, None),
        (
            "bigint",
            Some(("{r}.to_float()", "4.611686018427388e+18")),
            Some(("{r}.add(1)", "4611686018427387905")),
        ),
        (
            "str",
            Some(("{r}.upper()", "ABC")),
            Some(("{r}.compare(\"a\")", "1")),
        ),
        (
            "bytes",
            Some(("{r}.len()", "2")),
            Some(("{r}.compare(b\"ac\")", "-1")),
        ),
        (
            "bytearray",
            Some(("{r}.len()", "2")),
            Some(("{r}.str()", "bytearray(b'ab')")),
        ),
        (
            "list",
            Some(("{r}.len()", "2")),
            Some(("{r}.eq([3, 1])", "true")),
        ),
        (
            "map",
            Some(("{r}.len()", "1")),
            Some(("{r}.str()", "{'a': 1}")),
        ),
        (
            "set",
            Some(("{r}.len()", "2")),
            Some(("{r}.str()", "{1, 2}")),
        ),
        ("tuple", None, Some(("{r}.compare((1, 3))", "-1"))),
        (
            "struct",
            Some(("{r}.get()", "4")),
            Some(("{r}.eq({r})", "true")),
        ),
        (
            "enum",
            Some(("{r}.name()", "e")),
            Some(("{r}.eq(E.B)", "false")),
        ),
        ("option", None, Some(("{r}.eq(ho())", "true"))),
        ("result", None, Some(("{r}.str()", "1"))),
        ("fn", None, Some(("{r}.eq(dbl)", "true"))),
        (
            "channel",
            Some(("{r}.len()", "1")),
            Some(("{r}.eq({r})", "true")),
        ),
        ("cursor", None, Some(("{r}.next()", "5"))),
        ("generator", None, Some(("{r}.next()", "1"))),
        (
            "Shared",
            Some(("{r}.get()", "3")),
            Some(("{r}.str()", "Shared(3)")),
        ),
        (
            "RwShared",
            Some(("{r}.get()", "3")),
            Some(("{r}.str()", "RwShared(3)")),
        ),
        (
            "Atomic",
            Some(("{r}.load()", "2")),
            Some(("{r}.str()", "Atomic(2)")),
        ),
        (
            "AtomicInt",
            Some(("{r}.load()", "9")),
            Some(("{r}.str()", "AtomicInt(9)")),
        ),
        (
            "Executor",
            Some(("{r}.submit(fn() -> int: 5).get()", "5")),
            None,
        ),
        ("Socket", Some(("{r}.write(\"x\")", "1")), None),
        ("Listener", Some(("{r}.accept(0)", "!timeout")), None),
        ("Writer", Some(("{r}.write(\"\")", "0")), None),
        ("Reader", Some(("{r}.read_line()", "line1")), None),
        ("module", Some(("{r}.sqrt(4.0)", "2.0")), None),
    ]
}

#[test]
fn every_kind_has_cells() {
    for k in KINDS {
        assert!(
            receivers().iter().any(|(n, ..)| n == k),
            "kind {k} has no receiver"
        );
        assert!(rows().iter().any(|(n, ..)| n == k), "kind {k} has no row");
    }
    // A kind `kind_of` names is listed, or is one of the two that cannot be a receiver.
    let named = [
        "str",
        "bytes",
        "bytearray",
        "bigint",
        "float",
        "cursor",
        "list",
        "tuple",
        "map",
        "set",
        "struct",
        "enum",
        "cell",
        "fn",
        "module",
        "ptr",
        "channel",
        "Shared",
        "RwShared",
        "Atomic",
        "AtomicInt",
        "Executor",
        "Socket",
        "Listener",
        "Writer",
        "Reader",
        "generator",
    ];
    let _ = kind_of;
    for k in named {
        assert!(
            KINDS.contains(&k) || k == "cell" || k == "ptr",
            "kind {k} has no cells"
        );
    }
}

#[test]
fn declared_and_intrinsic_methods_answer() {
    let mut cells = Vec::new();
    for (kind, declared, intrinsic) in rows() {
        let (_, setup, recv) = receivers()
            .into_iter()
            .find(|(n, ..)| *n == kind)
            .expect("a receiver");
        for (col, c) in [("declared", declared), ("intrinsic", intrinsic)] {
            if let Some((call, out)) = c {
                let body = call.replace("{r}", recv);
                cells.push(cell(format!("{kind}/{col}"), program(setup, &body), out));
            }
        }
    }
    run_grid("method-dispatch", &cells, &[]);
}

/// The text a miss faults with, per kind: `a.nope()` compiled without the checker.
fn misses() -> Vec<(&'static str, &'static str)> {
    vec![
        ("int", "type int has no method 'nope'"),
        ("float", "type float has no method 'nope'"),
        ("bool", "type bool has no method 'nope'"),
        ("None", "type None has no method 'nope'"),
        ("bigint", "type int has no method 'nope'"),
        ("str", "type str has no method 'nope'"),
        ("bytes", "type bytes has no method 'nope'"),
        ("bytearray", "type bytearray has no method 'nope'"),
        ("list", "type list has no method 'nope'"),
        ("map", "type map has no method 'nope'"),
        ("set", "type set has no method 'nope'"),
        ("tuple", "type tuple has no method 'nope'"),
        ("struct", "struct 'P' has no method 'nope'"),
        ("enum", "type E has no method 'nope'"),
        ("option", "an optional value (`T?`) has no method 'nope'"),
        ("result", "a result value (`T!E`) has no method 'nope'"),
        ("fn", "type function has no method 'nope'"),
        ("channel", "type Channel has no method 'nope'"),
        (
            "cursor",
            "a cursor has no method 'nope' (only `next()`/`iter()`)",
        ),
        (
            "generator",
            "a generator has no method 'nope' (only `next()`)",
        ),
        ("Shared", "type Shared has no method 'nope'"),
        ("RwShared", "type RwShared has no method 'nope'"),
        ("Atomic", "type Atomic has no method 'nope'"),
        ("AtomicInt", "type AtomicInt has no method 'nope'"),
        ("Executor", "type Executor has no method 'nope'"),
        ("Socket", "type Socket has no method 'nope'"),
        ("Listener", "type Listener has no method 'nope'"),
        ("Writer", "type Writer has no method 'nope'"),
        ("Reader", "type Reader has no method 'nope'"),
        ("module", "module 'std.math' has no member 'nope'"),
    ]
}

/// Compile `src` without the checker, run it and return its fault text, or `ok: <stdout>`.
fn run_unchecked(dir: &std::path::Path, src: &str) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    let (out, _, res, _) = chezzi::vm::on_vm_stack(move || {
        chezzi::vm::run_built(
            || chezzi::compiler::compile_entry_file(&entry, None),
            chezzi::native::HostConfig::default(),
            None,
            false,
        )
    });
    match res {
        Ok(()) => format!("ok: {}", String::from_utf8_lossy(&out)),
        Err(e) => e.message,
    }
}

#[test]
fn a_miss_faults_with_its_kind_text() {
    let root = std::env::temp_dir().join(format!("chezzi-method-miss-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut fails = Vec::new();
    for (kind, want) in misses() {
        let (_, setup, recv) = receivers()
            .into_iter()
            .find(|(n, ..)| *n == kind)
            .expect("a receiver");
        // The Reader's file lives in the cell's own directory.
        let dir = root.join(kind);
        let setup = setup.replace("\"f.txt\"", &format!("{:?}", dir.join("f.txt")));
        let got = run_unchecked(&dir, &program(&setup, &format!("{recv}.nope()")));
        if got != want {
            fails.push(format!("{kind}: want {want:?}, got {got:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(fails.is_empty(), "red miss cells:\n{}", fails.join("\n"));
    assert_eq!(misses().len(), KINDS.len(), "every kind has a miss cell");
}

/// Every method the checker's method set grants a builtin kind has an entry in the one table the
/// VM reads (`Program::methods`): each declared method of a builtin type's std decl in that type's
/// row, each intrinsic row's method in each row its kind lands in, and each std protocol default
/// in every row that answers defaults. A struct and an enum of the probe program stand for every
/// user struct and enum.
#[test]
fn every_granted_method_has_a_table_entry() {
    use chezzi::base::builtin_method::{DECLARED_METHODS, ErrorMethod, NativeStatic};
    use chezzi::base::hooks::protocol_method;
    use chezzi::base::intrinsic::INTRINSIC_PROTO_METHODS;
    use chezzi::base::method_table::{KindRows, MethodEntry, MethodRow};
    let dir = std::env::temp_dir().join(format!("chezzi-method-table-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, format!("{CONC}import std.io\n{DECLS}print(1)\n")).unwrap();
    let program = chezzi::compiler::compile_entry_file(&entry, None).expect("the probe compiles");
    let _ = std::fs::remove_dir_all(&dir);
    let t = &program.methods;
    let id = |m: &str| t.names().get(m).unwrap_or_else(|| panic!("no id for {m}"));
    let p_tid = program
        .structs
        .values()
        .find(|d| d.display_name == "P")
        .expect("struct P")
        .tid;
    let e_vid = program
        .variants_by_id
        .iter()
        .position(|v| v.enum_name.ends_with('E') && v.name == "A")
        .expect("enum E") as u32;
    assert!(
        program.native_methods["Executor"].contains_key("submit"),
        "the probe compiles a bodied std method"
    );
    let mut fails = Vec::new();
    for (ty, bodyless, bodied) in DECLARED_METHODS {
        let row = MethodRow::of_type(ty).unwrap_or_else(|| panic!("{ty} has no row"));
        // A bodyless static is a `NativeStatic`, called through the type, not a receiver.
        let instance = bodyless
            .iter()
            .filter(|m| NativeStatic::from_key(ty, m).is_none());
        // A bodied method runs only when the program compiled its body (a std module it imports,
        // the prelude's bodies when it uses one).
        let compiled = bodied.iter().filter(|m| {
            program
                .native_methods
                .get(*ty)
                .is_some_and(|ms| ms.contains_key(**m))
        });
        for m in instance.chain(compiled) {
            if t.fixed(row, id(m)) == MethodEntry::Miss {
                fails.push(format!("{ty}.{m}: declared, no entry"));
            }
        }
    }
    for (protocol, kind) in INTRINSIC_PROTO_METHODS {
        let Some(m) = protocol_method(protocol) else {
            continue;
        };
        let got: Vec<MethodEntry> = match kind.method_rows() {
            KindRows::Fixed(rows) => rows.iter().map(|r| t.fixed(*r, id(m))).collect(),
            KindRows::Structs => vec![t.of_struct(p_tid, id(m))],
            KindRows::Enums => vec![t.of_enum(e_vid, id(m))],
            KindRows::Witness => Vec::new(),
        };
        if got.contains(&MethodEntry::Miss) {
            fails.push(format!("({protocol}, {kind}).{m}: granted, no entry"));
        }
    }
    for m in ErrorMethod::ALL.iter().map(|m| m.name()) {
        for row in [MethodRow::Int, MethodRow::Str, MethodRow::Tuple] {
            if t.fixed(row, id(m)) == MethodEntry::Miss {
                fails.push(format!("{row:?}.{m}: a protocol default, no entry"));
            }
        }
        if t.of_struct(p_tid, id(m)) == MethodEntry::Miss {
            fails.push(format!("struct.{m}: a protocol default, no entry"));
        }
    }
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}
