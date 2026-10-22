//! TICKET-247 (wave 23, Family "Airlock"): `?x` / `!x` hold the object bound to `x`, so every form
//! that wraps or stores a generator's frame-local gets the bare name's answer when a task then
//! writes it: a fault or a compile error, never a silent lost write. Grid: leak form x root type
//! x route x crossing, each cell its own process at `CHEZZI_THREADS` 1, 2 and default.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const THREADS: [Option<&str>; 3] = [Some("1"), Some("2"), None];
const FAULT: &str = "this value is this task's copy";

fn run(tag: &str, src: &str, threads: Option<&str>) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!(
        "chz-t247-{}-{tag}-{}",
        std::process::id(),
        threads.unwrap_or("d")
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path).stdin(Stdio::null());
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let out = cmd.output().expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `(name, type, init, write, decl)`.
const ROOTS: [(&str, &str, &str, &str, &str); 6] = [
    ("List", "List[int]", "[0]", "buf.push(1)", ""),
    ("Map", "Map[int, int]", "{0: 0}", "buf[1] = 1", ""),
    ("Set", "Set[int]", "{0}", "buf.insert(1)", ""),
    (
        "bytearray",
        "bytearray",
        "bytearray([0])",
        "buf.push(1)",
        "",
    ),
    ("tuple", "(List[int], int)", "([0], 1)", "buf.0.push(1)", ""),
    (
        "struct",
        "S",
        "S([0])",
        "buf.xs = [1]",
        "struct S:\n    xs: List[int]\n    fn message(self) -> str:\n        return \"e\"\n",
    ),
];

/// `(name, leak lines, leaked type, decl)`. `@[` and `]@` bracket the leaked expression; the
/// route puts its sink there. `$T` is the root type and `$I` its init.
const FORMS: [(&str, &str, &str, &str); 13] = [
    ("bare", "@[buf]@", "$T", ""),
    ("wrap", "@[?buf]@", "$T?", ""),
    ("errval", "@[!buf]@", "int!$T", ""),
    ("list", "@[[?buf]]@", "List[$T?]", ""),
    ("tuple", "@[(?buf, 1)]@", "($T?, int)", ""),
    ("map", "@[{0: ?buf}]@", "Map[int, $T?]", ""),
    ("ctor", "@[W(?buf)]@", "W", "struct W:\n    o: $T?\n"),
    (
        "ifelse",
        "c := true\nv: $T? = if c: ?buf else: None\n@[v]@",
        "$T?",
        "",
    ),
    (
        "matchstmt",
        "match ?buf:\n    ?b: @[b]@\n    None: pass",
        "$T",
        "",
    ),
    ("coalesce", "@[(?buf) ?? $I]@", "$T", ""),
    (
        "elseguard",
        "v := ?buf else:\n    panic(\"none\")\n@[v]@",
        "$T",
        "",
    ),
    (
        "unw",
        "@[unw(?buf)]@",
        "$T?",
        "fn unw(o: $T?) -> $T?:\n    return o\n",
    ),
    ("recover", "v := recover: ?buf\n@[v]@", "$T?!", ""),
];

/// `yield`: the generator yields it. `store`: it stores it in a parent struct. `arg`: it passes
/// it to a callee that stores it. `ret`: a callee returns it. `cstore`: a callee stores it.
const ROUTES: [&str; 5] = ["yield", "store", "arg", "ret", "cstore"];

const CROSSINGS: [(&str, &str); 3] = [
    ("spawn_arg", "    parallel:\n        spawn resume(it)\n"),
    (
        "spawn_block",
        "    parallel:\n        spawn:\n            _ := it.next()\n",
    ),
    (
        "executor",
        "    ex := Executor(1)\n    _ := ex.submit(fn(): resume(it))\n    ex.shutdown()\n",
    ),
];

fn ind(s: &str) -> String {
    s.split('\n').map(|l| format!("    {l}\n")).collect()
}

fn program(
    root: (&str, &str, &str, &str, &str),
    form: (&str, &str, &str, &str),
    route: &str,
    crossing: &str,
) -> String {
    let (_, ty, init, write, rdecl) = root;
    let sub = |s: &str| s.replace("$I", init).replace("$T", ty);
    let (body, y, fdecl) = (sub(form.1), sub(form.2), sub(form.3));
    let (open, close) = match route {
        "yield" => ("yield ", ""),
        "store" | "cstore" => ("h.o = [", "]"),
        "arg" => ("keep(", ", h)"),
        _ => ("return ", ""),
    };
    let leak = body.replace("@[", open).replace("]@", close);
    let in_callee = leak.replace("buf", "b");
    let mut src = format!(
        "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nimport Iterator from std.iter\n{rdecl}{fdecl}struct H:\n    o: List[{y}]\n"
    );
    let int_gen = format!("fn gen(h: H) -> Iterator[int]:\n    buf := {init}\n");
    let (ity, tail) = match route {
        "yield" => {
            src += &format!(
                "fn gen(h: H) -> Iterator[{y}]:\n    buf := {init}\n{}",
                ind(&leak)
            );
            (y.clone(), ind(&leak))
        }
        "store" => {
            src += &format!("{int_gen}{}    yield 0\n", ind(&leak));
            ("int".to_string(), "    yield 0\n".to_string())
        }
        "arg" => {
            src += &format!(
                "fn keep(v: {y}, h: H):\n    h.o = [v]\n{int_gen}{}    yield 0\n",
                ind(&leak)
            );
            ("int".to_string(), "    yield 0\n".to_string())
        }
        "ret" => {
            // A `match` or an `else:` guard leaves a path with no `return`.
            let end = if body.contains("match") || body.contains("else:\n") {
                "    panic(\"unreachable\")\n"
            } else {
                ""
            };
            src += &format!("fn leak(b: {ty}) -> {y}:\n{}{end}", ind(&in_callee));
            src += &format!(
                "fn gen(h: H) -> Iterator[{y}]:\n    buf := {init}\n    yield leak(buf)\n"
            );
            (y.clone(), "    yield leak(buf)\n".to_string())
        }
        _ => {
            src += &format!(
                "fn put(b: {ty}, h: H):\n{}{int_gen}    put(buf, h)\n    yield 0\n",
                ind(&in_callee)
            );
            ("int".to_string(), "    yield 0\n".to_string())
        }
    };
    src += &format!("    {write}\n    print(\"task wrote\")\n{tail}");
    src += &format!("fn resume(it: Iterator[{ity}]):\n    _ := it.next()\n");
    src += "fn main():\n    h := H([])\n    it := gen(h)\n    seen := it.next()\n";
    src += crossing;
    src += "    print(\"parent sees\", seen, h.o)\nmain()\n";
    src
}

#[test]
fn wrapped_name_yield_faults_like_the_bare_name() {
    let mut cells = Vec::new();
    for root in ROOTS {
        for form in FORMS {
            for route in ROUTES {
                for crossing in CROSSINGS {
                    for t in THREADS {
                        cells.push((root, form, route, crossing, t));
                    }
                }
            }
        }
    }
    let next = AtomicUsize::new(0);
    let silent = Mutex::new(Vec::new());
    let drift = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(root, form, route, crossing, t)) = cells.get(i) else {
                        break;
                    };
                    let src = program(root, form, route, crossing.1);
                    let (code, out, err) = run(&format!("g{i}"), &src, t);
                    let cell = format!(
                        "{} / {} / {route} / {} / threads {t:?}",
                        root.0, form.0, crossing.0
                    );
                    let wrote = out.contains("task wrote");
                    // `!x` takes an operand that satisfies `Error`; only struct `S` does.
                    let expected = if form.0 == "errval" && root.0 != "struct" {
                        "does not satisfy Error"
                    } else {
                        FAULT
                    };
                    if code == 0 && wrote {
                        silent.lock().unwrap().push(format!("{cell}: {out:?}"));
                    } else if wrote || !err.contains(expected) {
                        drift
                            .lock()
                            .unwrap()
                            .push(format!("{cell}: rc {code} {out:?} {err:?}"));
                    }
                }
            });
        }
    });
    let (silent, drift) = (silent.into_inner().unwrap(), drift.into_inner().unwrap());
    assert!(
        silent.is_empty(),
        "silent lost write, no fault and no compile error:\n{}",
        silent.join("\n")
    );
    // A cell that fails for any other reason would pass the assert above without testing anything.
    assert!(
        drift.is_empty(),
        "template drift, a cell failed for another reason:\n{}",
        drift.join("\n")
    );
}

/// `(yielded expression, its type, init, write)`: `buf` is only an operand of an operator that
/// computes a new value, so the parent never holds it and the task's write must run.
const FRESH_OPERATORS: [(&str, &str, &str, &str); 9] = [
    ("buf + [1]", "List[int]", "[0]", "buf.push(1)"),
    ("buf == [0]", "bool", "[0]", "buf.push(1)"),
    ("0 in buf", "bool", "[0]", "buf.push(1)"),
    ("not (buf == [0])", "bool", "[0]", "buf.push(1)"),
    ("buf != [0] and buf == [0]", "bool", "[0]", "buf.push(1)"),
    (
        "buf.len() < 2 < buf.len() + 2",
        "bool",
        "[0]",
        "buf.push(1)",
    ),
    ("-buf.len()", "int", "[0]", "buf.push(1)"),
    ("0 in buf", "bool", "{0: 0}", "buf[1] = 1"),
    ("buf == {0}", "bool", "{0}", "buf.insert(1)"),
];

#[test]
fn fresh_operator_operands_keep_the_slot_writable() {
    for (i, (e, y, init, write)) in FRESH_OPERATORS.into_iter().enumerate() {
        let src = format!(
            "import Iterator from std.iter\nfn gen() -> Iterator[{y}]:\n    buf := {init}\n    yield {e}\n    {write}\n    print(\"task wrote\")\n    yield {e}\nfn resume(it: Iterator[{y}]):\n    _ := it.next()\nfn main():\n    it := gen()\n    seen := it.next()\n    parallel:\n        spawn resume(it)\n    print(\"parent sees\", seen)\nmain()\n"
        );
        for t in THREADS {
            let (code, out, err) = run(&format!("c{i}"), &src, t);
            assert!(
                code == 0 && out.contains("task wrote"),
                "`yield {e}` over `{init}` at threads {t:?} must leave `buf` writable: rc {code} {out:?} {err:?}"
            );
        }
    }
}
