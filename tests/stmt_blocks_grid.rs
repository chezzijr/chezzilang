//! Which blocks a statement holds. A generator body's rules (`yield` makes the fn a generator,
//! `spawn` and `defer` are rejected) must reach a statement in every block the body holds, at any
//! depth. Grid: container {none, `if`, `for`, `while`, `match` arm} x holder {`recover:` after `:=`,
//! `recover:` after `=`, `else:` guard} x content {`yield`, `spawn`, `defer`}. The owner is
//! `ast::stmt_blocks` (`src/ast/mod.rs`). One generated program per cell, run through the built
//! `chezzi` binary.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

#[derive(Clone, Copy, Debug)]
enum Container {
    None,
    If,
    For,
    While,
    MatchArm,
}

#[derive(Clone, Copy, Debug)]
enum Holder {
    LetRecover,
    AssignRecover,
    ElseGuard,
}

#[derive(Clone, Copy, Debug)]
enum Content {
    Yield,
    Spawn,
    Defer,
}

const GEN_SPAWN: &str = "'spawn' is not allowed inside a generator body";
const GEN_DEFER: &str = "'defer' is not allowed inside a generator body";

/// The lines that open the container, and the indent of its body.
fn container(c: Container) -> Vec<&'static str> {
    match c {
        Container::None => vec![],
        Container::If => vec!["if true:"],
        Container::For => vec!["for _i in 0..1:"],
        Container::While => vec!["w := true", "while w:", "    w = false"],
        Container::MatchArm => vec!["match 1:", "    _:"],
    }
}

fn depth(c: Container) -> usize {
    match c {
        Container::None => 0,
        Container::If | Container::For | Container::While => 1,
        Container::MatchArm => 2,
    }
}

/// Lines before the block header (same indent), the header, and the lines that close the block.
fn holder(h: Holder) -> (Vec<&'static str>, &'static str, Vec<&'static str>) {
    match h {
        Holder::LetRecover => (vec![], "_r := recover:", vec![]),
        Holder::AssignRecover => (vec!["r := recover:", "    pass"], "r = recover:", vec![]),
        Holder::ElseGuard => (vec![], "_x := z() else:", vec!["return"]),
    }
}

fn content(x: Content) -> &'static str {
    match x {
        Content::Yield => "yield 1",
        Content::Spawn => "spawn print(9)",
        Content::Defer => "defer print(9)",
    }
}

fn program(c: Container, h: Holder, x: Content) -> String {
    let mut body: Vec<String> = Vec::new();
    for l in container(c) {
        body.push(format!("    {l}"));
    }
    let pad = format!("    {}", "    ".repeat(depth(c)));
    let (pre, head, close) = holder(h);
    for l in pre {
        body.push(format!("{pad}{l}"));
    }
    body.push(format!("{pad}{head}"));
    body.push(format!("{pad}    {}", content(x)));
    for l in close {
        body.push(format!("{pad}    {l}"));
    }
    format!(
        "import Iterator from std.iter\nfn z() -> int?:\n    return None\nfn gen() -> Iterator[int]:\n{}\n    yield 0\nfor v in gen():\n    print(v)\n",
        body.join("\n")
    )
}

fn expect(h: Holder, x: Content) -> Expect {
    match x {
        Content::Spawn => Expect::Rejects(GEN_SPAWN),
        Content::Defer => Expect::Rejects(GEN_DEFER),
        // The `else` guard leaves the generator after its yield, so the trailing `yield 0` never runs.
        Content::Yield => match h {
            Holder::LetRecover | Holder::AssignRecover => Expect::Prints("1\n0".to_string()),
            Holder::ElseGuard => Expect::Prints("1".to_string()),
        },
    }
}

#[test]
fn stmt_blocks_grid() {
    let mut cells = Vec::new();
    for c in [
        Container::None,
        Container::If,
        Container::For,
        Container::While,
        Container::MatchArm,
    ] {
        for h in [Holder::LetRecover, Holder::AssignRecover, Holder::ElseGuard] {
            for x in [Content::Yield, Content::Spawn, Content::Defer] {
                cells.push(Cell {
                    name: format!("{c:?}/{h:?}/{x:?}"),
                    files: vec![("main.chz".to_string(), program(c, h, x))],
                    expect: expect(h, x),
                });
            }
        }
    }
    run_grid("stmt_blocks", &cells, &[]);
}
