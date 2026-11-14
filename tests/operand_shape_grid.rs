//! Wave 24 F6 — the operand shapes of `wait` arms, `spawn`/`defer` targets and `Range` bounds are
//! each decided once. A malformed arm head or a non-call target is a PARSE error with one text per
//! malformation, the same for a recv arm and a send arm and the same for `spawn` and `defer`. A
//! wait arm's receiver is judged by one checker helper for both directions, and a call target's
//! callee by one checker helper over the callee's resolution. A range with a missing bound is
//! refused by `Range.iter` / `Range.len` in `std/prelude.chz` at run time, a literal and a value
//! alike (Python `range` has no open bound; Rust `(..5).len()` is a compile error, but there the
//! literal and the value are one type rule, not a literal-only check).
//!
//! Axes: wait arm (recv / send) × malformation (wrong arg count, wrong method, a non-call, a
//! non-Channel receiver); call target (bare name, literal, `1 + 2`, method value, ctor, dotted ctor,
//! native ctor, unknown name, call, lambda call, method call, static method, first-class builtin)
//! × {spawn, defer}; range (`..5`, `Range(None, 5)`, a var holding `..5`) × (`for`, comprehension,
//! `.len()`, `.iter()`).

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, cmd: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-opshape-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args([cmd, "main.chz"])
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

/// A parse error prints as `resolve error` (the resolver parses each module).
const PARSE: &str = "resolve error";
const TYPE: &str = "type error";

/// The one text for a wait arm head that is neither `v := ch.recv()` nor `ch.send(v)`.
const ARM_FORMS: &str =
    "a wait arm must be a recv (`v := ch.recv():`), a send (`ch.send(v):`), or `else`";
const NOT_CHANNEL: &str = "a wait arm must recv from or send to a Channel, found Box";

fn wait_src(arm: &str) -> String {
    format!(
        "struct Box:\n    v: int\n    fn recv(self) -> int:\n        return self.v\n    fn send(self, x: int):\n        print(x)\nb := Box(1)\nch := Channel[int](1)\nch.send(7)\nwait:\n    {arm}:\n        print(\"arm\")\n"
    )
}

/// (cell, arm head, class, text). Each reject cell gives exactly one diagnostic.
const WAIT_REJECT: &[(&str, &str, &str, &str)] = &[
    (
        "recv_args",
        "r := ch.recv(1)",
        PARSE,
        "`recv` in a wait arm takes no arguments",
    ),
    (
        "recv_named",
        "r := ch.recv(x=1)",
        PARSE,
        "`recv` in a wait arm takes no arguments",
    ),
    (
        "send_noarg",
        "ch.send()",
        PARSE,
        "`send` in a wait arm takes one argument",
    ),
    (
        "send_2args",
        "ch.send(1, 2)",
        PARSE,
        "`send` in a wait arm takes one argument",
    ),
    (
        "send_named",
        "ch.send(v=1)",
        PARSE,
        "`send` in a wait arm takes one argument",
    ),
    ("recv_try", "r := ch.try_recv()", PARSE, ARM_FORMS),
    ("send_try", "ch.try_send(1)", PARSE, ARM_FORMS),
    ("recv_noncall", "r := ch", PARSE, ARM_FORMS),
    ("send_noncall", "ch", PARSE, ARM_FORMS),
    ("recv_user", "r := b.recv()", TYPE, NOT_CHANNEL),
    ("send_user", "b.send(2)", TYPE, NOT_CHANNEL),
    (
        "send_elem",
        "ch.send(\"s\")",
        TYPE,
        "argument 1 of 'send': expected int, found str",
    ),
];

/// (cell, arm head, expected stdout).
const WAIT_ACCEPT: &[(&str, &str, &str)] = &[
    ("recv_ok", "r := ch.recv()", "arm\n"),
    ("recv_assign", "_ = ch.recv()", "arm\n"),
];

const SEND_ACCEPT_SRC: &str = "ch := Channel[int](1)\nwait:\n    ch.send(3):\n        print(\"sent\")\nprint(ch.recv() ?? 0)\n";

fn diag_count(text: &str) -> usize {
    text.lines()
        .filter(|l| l.starts_with(PARSE) || l.starts_with(TYPE))
        .count()
}

fn reject(failures: &mut Vec<String>, cell: &str, src: &str, class: &str, want: &str) {
    let (ok, text) = chezzi(cell, "check", src);
    let head = format!("{class} (");
    if ok || !text.contains(&head) || !text.contains(want) || diag_count(&text) != 1 {
        failures.push(format!(
            "{cell}: want one {class} `{want}`, got (ok={ok}):\n{text}"
        ));
    }
}

#[test]
fn wait_arm_reject_cells() {
    let mut failures = Vec::new();
    for (cell, arm, class, want) in WAIT_REJECT {
        reject(&mut failures, cell, &wait_src(arm), class, want);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn wait_arm_accept_cells() {
    let mut failures = Vec::new();
    for (cell, arm, want) in WAIT_ACCEPT {
        let (ok, text) = chezzi(cell, "run", &wait_src(arm));
        if !ok || text != *want {
            failures.push(format!("{cell}: want {want:?}, got (ok={ok}) {text:?}"));
        }
    }
    let (ok, text) = chezzi("send_ok", "run", SEND_ACCEPT_SRC);
    if !ok || text != "sent\n3\n" {
        failures.push(format!("send_ok: got (ok={ok}) {text:?}"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

fn task_src(kw: &str, target: &str) -> String {
    format!(
        "import std.concurrency as c\nstruct P:\n    x: int\nstruct H:\n    fn build(n: int):\n        print(n)\nenum C:\n    V(int)\nenum G[T]:\n    W(T)\nfn f():\n    print(\"f\")\nxs := [1]\nfn main():\n    g := f\n    if false:\n        g()\n    {kw} {target}\nmain()\n"
    )
}

/// The one text for a `spawn`/`defer` target that is not a call.
fn not_a_call(kw: &str) -> String {
    format!("{kw} requires a function or method call (`{kw} f(x)`) or a block (`{kw}:`)")
}

/// The one text for a call target whose callee is a constructor or a non-first-class builtin.
fn not_first_class(kw: &str) -> String {
    format!(
        "{kw} requires a function or method call (built-ins and constructors must be wrapped in a function)"
    )
}

/// (cell, target, which text). `None` text: the target's own diagnostic, given once.
enum Want {
    NotACall,
    NotFirstClass,
    Own(&'static str),
}

const TASK_REJECT: &[(&str, &str, Want)] = &[
    ("name", "f", Want::NotACall),
    ("literal", "5", Want::NotACall),
    ("binary", "1 + 2", Want::NotACall),
    ("method_value", "xs.push", Want::NotACall),
    ("lambda", "fn(): print(1)", Want::NotACall),
    ("ctor", "P(1)", Want::NotFirstClass),
    ("dotted_ctor", "C.V(1)", Want::NotFirstClass),
    ("turbofish_ctor", "G[int].W(1)", Want::NotFirstClass),
    ("generic_ctor", "G.W(1)", Want::NotFirstClass),
    ("native_ctor", "Channel[int](1)", Want::NotFirstClass),
    ("qualified_native_ctor", "c.Shared(1)", Want::NotFirstClass),
    ("unknown_some", "Some(1)", Want::Own("unknown name 'Some'")),
    ("unknown_len", "len([1])", Want::Own("unknown name 'len'")),
];

/// (cell, target, expected stdout).
const TASK_ACCEPT: &[(&str, &str, &str)] = &[
    ("call", "f()", "f\n"),
    ("local_fn", "g()", "f\n"),
    ("lambda_call", "(fn(): print(\"l\"))()", "l\n"),
    ("method_call", "xs.push(2)", ""),
    ("static_method", "H.build(3)", "3\n"),
    ("builtin_fn", "print(1)", "1\n"),
];

#[test]
fn spawn_defer_reject_cells() {
    let mut failures = Vec::new();
    for kw in ["spawn", "defer"] {
        for (cell, target, want) in TASK_REJECT {
            let (class, text) = match want {
                Want::NotACall => (PARSE, not_a_call(kw)),
                Want::NotFirstClass => (TYPE, not_first_class(kw)),
                Want::Own(t) => (TYPE, (*t).to_string()),
            };
            let tag = format!("{kw}_{cell}");
            reject(&mut failures, &tag, &task_src(kw, target), class, &text);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn spawn_defer_accept_cells() {
    let mut failures = Vec::new();
    for kw in ["spawn", "defer"] {
        for (cell, target, want) in TASK_ACCEPT {
            let tag = format!("{kw}_{cell}");
            let (ok, text) = chezzi(&tag, "run", &task_src(kw, target));
            if !ok || text != *want {
                failures.push(format!("{tag}: want {want:?}, got (ok={ok}) {text:?}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

const NO_START: &str = "a range with no start cannot be iterated";
const NO_BOUND: &str = "len() needs a range with a start and an end";

/// Every range cell checks clean and faults at run time with the std text.
#[test]
fn range_missing_bound_cells_are_run_faults() {
    let mut failures = Vec::new();
    for (form_cell, form) in [("lit", "..5"), ("ctor", "Range(None, 5)"), ("var", "r")] {
        let uses: [(&str, String, &str); 4] = [
            ("for", format!("for i in {form}:\n    print(i)\n"), NO_START),
            ("comp", format!("print([i for i in {form}])\n"), NO_START),
            ("len", format!("print(({form}).len())\n"), NO_BOUND),
            ("iter", format!("print(({form}).iter().next())\n"), NO_START),
        ];
        for (use_cell, body, want) in uses {
            let tag = format!("{form_cell}_{use_cell}");
            let src = format!("r := ..5\n{body}");
            let (ok, text) = chezzi(&tag, "check", &src);
            if !ok {
                failures.push(format!("{tag}: check should pass:\n{text}"));
                continue;
            }
            let (ok, text) = chezzi(&tag, "run", &src);
            if ok || !text.contains("runtime error (") || !text.contains(want) {
                failures.push(format!(
                    "{tag}: want a run fault `{want}`, got (ok={ok}):\n{text}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
