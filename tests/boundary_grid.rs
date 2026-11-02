//! What may cross a `spawn:`, `defer:` or `recover:` block, or sit in a generator body. Block
//! {spawn, defer, recover, `if` as the control} x construct {break, continue, return, `?` on a
//! `T?`, `?` on a `T!E`, `else` guard leaving by `panic`, `return` or `break`, yield} x {loop
//! around the block, no loop} x {fn, generator}, plus each statement a generator body rejects. The
//! owner is `boundary::RULES` (`src/checker/boundary.rs`); each rejected cell pins the message that
//! table gives. One generated program per cell, run through the built `chezzi` binary; every
//! accept cell RUNS.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LIB: &str = "fn g() -> int?:
    return 1
fn h() -> int!str:
    return 1
fn z() -> int?:
    return None
";

#[derive(Clone, Copy, PartialEq)]
enum Block {
    Spawn,
    Defer,
    Recover,
    If,
}

#[derive(Clone, Copy, PartialEq)]
enum Construct {
    Break,
    Continue,
    Return,
    TryOption,
    TryResult,
    ElseGuard,
    GuardReturn,
    GuardBreak,
    Yield,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Fn,
    Generator,
}

const SPAWN_LOOP: &str =
    "'break' is not allowed inside a spawn block: a spawned task runs outside the loop";
const DEFER_LOOP: &str =
    "is not allowed inside a defer block: a deferred block runs at scope exit, outside the loop";
const RECOVER_LOOP: &str = "is not allowed inside a recover block: a recover block produces a value and cannot leave to the loop";
const GEN_DEFER: &str = "'defer' is not allowed inside a generator body: a suspended generator does not manage frame defers";
const GEN_CONC: &str = "is not allowed inside a generator body: a suspended generator does not manage structured-concurrency state";

fn header(b: Block) -> &'static [&'static str] {
    match b {
        Block::Spawn => &["parallel:", "    spawn:"],
        Block::Defer => &["defer:"],
        Block::Recover => &["_r := recover:"],
        Block::If => &["if true:"],
    }
}

fn construct_lines(c: Construct, ctx: Ctx) -> Vec<&'static str> {
    match c {
        Construct::Break => vec!["break"],
        Construct::Continue => vec!["continue"],
        Construct::Return => match ctx {
            Ctx::Fn => vec!["return 1"],
            Ctx::Generator => vec!["return"],
        },
        Construct::TryOption => vec!["_ := g()?"],
        Construct::TryResult => vec!["_ := h()?"],
        Construct::ElseGuard => vec!["v := g() else:", "    panic(\"x\")", "print(v)"],
        Construct::GuardReturn => match ctx {
            Ctx::Fn => vec!["v := z() else:", "    return 1", "print(v)"],
            Ctx::Generator => vec!["v := z() else:", "    return", "print(v)"],
        },
        Construct::GuardBreak => vec!["v := z() else:", "    break", "print(v)"],
        Construct::Yield => vec!["yield 1"],
    }
}

fn program(b: Block, c: Construct, looped: bool, ctx: Ctx) -> String {
    let mut body: Vec<String> = Vec::new();
    let mut pad = String::from("    ");
    if looped {
        body.push(format!("{pad}for _i in 0..2:"));
        pad.push_str("    ");
    }
    let head = header(b);
    for h in head {
        body.push(format!("{pad}{h}"));
    }
    let inner = format!("{pad}{}", "    ".repeat(head.len()));
    for l in construct_lines(c, ctx) {
        body.push(format!("{inner}{l}"));
    }
    let body = body.join("\n");
    match ctx {
        Ctx::Fn => {
            format!("{LIB}fn f() -> int?:\n{body}\n    print(\"end\")\n    return 1\n_ := f()\n")
        }
        Ctx::Generator => format!(
            "import Iterator from std.iter\n{LIB}fn gen() -> Iterator[int]:\n{body}\n    yield 0\nfor x in gen():\n    print(x)\n"
        ),
    }
}

/// The expected outcome of one cell. `None`: the cell is not built (it tests nothing the block adds).
fn expect(b: Block, c: Construct, looped: bool, ctx: Ctx) -> Option<Expect> {
    use Construct as C;
    let rej = |f: &'static str| Some(Expect::Rejects(f));
    let n = if looped { 2 } else { 1 };
    // The value a generator yields and a fn prints after the block, per normal run.
    let fell = |per_iter: &str| -> Option<Expect> {
        let mut out: Vec<&str> = Vec::new();
        for _ in 0..n {
            if !per_iter.is_empty() {
                out.push(per_iter);
            }
        }
        out.push(match ctx {
            Ctx::Fn => "end",
            Ctx::Generator => "0",
        });
        Some(Expect::Prints(out.join("\n")))
    };
    if ctx == Ctx::Generator {
        match b {
            Block::Spawn => return rej("'parallel:' is not allowed inside a generator body"),
            Block::Defer => return rej(GEN_DEFER),
            Block::Recover | Block::If => {}
        }
    }
    if c == C::Yield && ctx == Ctx::Fn {
        return None;
    }
    // A jump with no loop at all has nothing to cross to.
    if !looped {
        match c {
            C::Break | C::GuardBreak => return rej("break outside loop"),
            C::Continue => return rej("continue outside loop"),
            C::Return | C::TryOption | C::TryResult | C::ElseGuard | C::GuardReturn | C::Yield => {}
        }
    }
    // An `else` guard crosses every block: the statement that leaves its block decides.
    let c = match c {
        C::GuardReturn => C::Return,
        C::GuardBreak => C::Break,
        C::Break
        | C::Continue
        | C::Return
        | C::TryOption
        | C::TryResult
        | C::ElseGuard
        | C::Yield => c,
    };
    match (b, c) {
        (Block::Spawn, C::Break) => rej(SPAWN_LOOP),
        (Block::Spawn, C::Continue) => rej(
            "'continue' is not allowed inside a spawn block: a spawned task runs outside the loop",
        ),
        (Block::Spawn, C::Return) => rej(
            "'return' is not allowed inside a spawn block: a spawned task has no caller to return to",
        ),
        (Block::Spawn, C::TryOption | C::TryResult) => rej(
            "'?' is not allowed inside a spawn block: a spawned task has no caller to propagate to",
        ),
        (Block::Defer, C::Break) => rej(DEFER_LOOP),
        (Block::Defer, C::Continue) => rej(DEFER_LOOP),
        (Block::Defer, C::Return) => rej(
            "'return' is not allowed inside a defer block: a deferred block has no caller to return to",
        ),
        (Block::Defer, C::TryOption | C::TryResult) => rej(
            "'?' is not allowed inside a defer block: a deferred block has no caller to propagate to; handle the error in the block with `match` or `??`",
        ),
        (Block::Spawn | Block::Defer, C::Yield) => None,
        (Block::Recover, C::Break | C::Continue) => rej(RECOVER_LOOP),
        (Block::Recover, C::Return) => rej(
            "'return' is not allowed inside a recover block: a recover block produces a value and cannot leave the function",
        ),
        (Block::Recover, C::TryOption) => rej(
            "'?' is not allowed inside a recover block: the operand is a `T?` value, and the block's result is a `T!E` value; use `match` instead",
        ),
        (Block::Recover, C::TryResult) => fell(""),
        // A defer block outside a loop runs at the fn's scope exit, after `end`.
        (Block::Defer, C::ElseGuard) if !looped => Some(Expect::Prints("end\n1".to_string())),
        (Block::Spawn | Block::Defer | Block::Recover | Block::If, C::ElseGuard) => fell("1"),
        // A yield in a recover block runs: Python `try: yield` gives the same order.
        (Block::Recover, C::Yield) => fell("1"),
        (Block::If, C::Break | C::Continue) => fell(""),
        (Block::If, C::Return) => Some(Expect::Prints(String::new())),
        (Block::If, C::TryOption) => match ctx {
            Ctx::Fn => fell(""),
            Ctx::Generator => rej("'?' used in a function that returns Iterator[int]"),
        },
        (Block::If, C::TryResult) => None,
        (Block::If, C::Yield) => fell("1"),
        (
            Block::Spawn | Block::Defer | Block::Recover | Block::If,
            C::GuardReturn | C::GuardBreak,
        ) => unreachable!("mapped to the leaving statement"),
    }
}

/// The statements a generator body rejects, directly and inside a recover block.
fn generator_cells() -> Vec<Cell> {
    let stmts: [(&str, &str, &'static str); 4] = [
        ("defer", "defer print(0)", GEN_DEFER),
        ("spawn", "spawn print(0)", GEN_CONC),
        ("parallel", "parallel:\n        print(0)", GEN_CONC),
        (
            "wait",
            "c := Channel[int](1)\n    c.send(1)\n    wait:\n        v := c.recv():\n            print(v)",
            GEN_CONC,
        ),
    ];
    let mut out = Vec::new();
    for (name, stmt, msg) in stmts {
        let direct = format!(
            "import Iterator from std.iter\nfn gen() -> Iterator[int]:\n    {stmt}\n    yield 0\nfor x in gen():\n    print(x)\n"
        );
        let nested = stmt.replace("\n", "\n    ");
        let in_recover = format!(
            "import Iterator from std.iter\nfn gen() -> Iterator[int]:\n    _r := recover:\n        {nested}\n    yield 0\nfor x in gen():\n    print(x)\n"
        );
        for (pos, src) in [("direct", direct), ("in recover", in_recover)] {
            out.push(Cell {
                name: format!("generator {name} {pos}"),
                files: vec![("main.chz".to_string(), src)],
                expect: Expect::Rejects(msg),
            });
        }
    }
    out
}

#[test]
fn boundary_grid() {
    let blocks = [
        ("spawn", Block::Spawn),
        ("defer", Block::Defer),
        ("recover", Block::Recover),
        ("if", Block::If),
    ];
    let constructs = [
        ("break", Construct::Break),
        ("continue", Construct::Continue),
        ("return", Construct::Return),
        ("?T?", Construct::TryOption),
        ("?T!E", Construct::TryResult),
        ("else", Construct::ElseGuard),
        ("else-return", Construct::GuardReturn),
        ("else-break", Construct::GuardBreak),
        ("yield", Construct::Yield),
    ];
    let mut cells = Vec::new();
    for ctx in [Ctx::Fn, Ctx::Generator] {
        for (bn, b) in blocks {
            for (cn, c) in constructs {
                for looped in [true, false] {
                    let Some(want) = expect(b, c, looped, ctx) else {
                        continue;
                    };
                    let ctx_name = match ctx {
                        Ctx::Fn => "fn",
                        Ctx::Generator => "gen",
                    };
                    let loop_name = if looped { "loop" } else { "no loop" };
                    cells.push(Cell {
                        name: format!("{ctx_name} {bn} {cn} {loop_name}"),
                        files: vec![("main.chz".to_string(), program(b, c, looped, ctx))],
                        expect: want,
                    });
                }
            }
        }
    }
    cells.extend(generator_cells());
    run_grid("boundary", &cells, &[]);
}

/// A `return` a block rejects gives one error: the enclosing fn's return type does not apply to it.
#[test]
fn a_rejected_return_reports_once() {
    let cells = [
        (
            "fn f():\n    defer:\n        return 5\nf()\n",
            "'return' is not allowed inside a defer block",
        ),
        (
            "fn f() -> str:\n    r := recover:\n        return 5\n    print(r)\n    return \"a\"\nprint(f())\n",
            "'return' is not allowed inside a recover block",
        ),
        (
            "fn f() -> int?:\n    spawn:\n        return\n    return 1\nprint(f())\n",
            "'return' is not allowed inside a spawn block",
        ),
    ];
    let root = std::env::temp_dir().join(format!("chezzi-boundary-once-{}", std::process::id()));
    let mut bad = Vec::new();
    for (i, (src, want)) in cells.iter().enumerate() {
        let dir = root.join(format!("c{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.chz"), src).unwrap();
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args(["check", "main.chz"])
            .current_dir(&dir)
            .output()
            .expect("spawn chezzi");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let errors: Vec<&str> = stderr
            .lines()
            .filter(|l| l.starts_with("type error"))
            .collect();
        if errors.len() != 1 || !errors[0].contains(want) {
            bad.push(format!("cell {i}: want one {want:?}, got {errors:?}"));
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
