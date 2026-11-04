//! TICKET-261 (U9): one unpacking grammar at `:=`, `=`, `for`, a comprehension, a wait arm and
//! `match`; a wait arm binds one `T!RecvError` (D2), so a tuple target there does not type-check.
//! Binding site x pattern shape x source of the element, one generated program per cell,
//! run through the built `chezzi` binary. Every accept cell RUNS and prints; every reject cell names
//! the rule. The rule: outside `match` only tuples unpack (`_` skips a slot, a nested target takes
//! parentheses, no outer parentheses); a struct or an enum variant unpacks in `match` only; no list
//! unpack and no `*rest`. Ancestor: CPython 3.14 prints `1 2 3`, `1 x`, `(1, 2)`, `(3, 1)` for the
//! nested / Set-of-tuples / bare-tuple rows (`for a, (b, c) in [(1, (2, 3))]`, `for a, b in
//! {(1, "x")}`, `x = 1, 2`, `return a // b, a % b`).

#[path = "support/grid_cell.rs"]
mod grid_cell;

use chezzi::ast::{Pattern, StmtKind};
use grid_cell::{Cell, Expect, run_grid};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Site {
    Walrus,
    Assign,
    For,
    Comp,
    Wait,
    WaitAssign,
    Match,
}

impl Site {
    const ALL: [Site; 7] = [
        Site::Walrus,
        Site::Assign,
        Site::For,
        Site::Comp,
        Site::Wait,
        Site::WaitAssign,
        Site::Match,
    ];

    fn name(self) -> &'static str {
        match self {
            Site::Walrus => "walrus",
            Site::Assign => "assign",
            Site::For => "for",
            Site::Comp => "comp",
            Site::Wait => "wait",
            Site::WaitAssign => "wait-assign",
            Site::Match => "match",
        }
    }

    /// A loop site iterates the source itself; every other site runs once per element.
    fn is_loop(self) -> bool {
        match self {
            Site::For | Site::Comp => true,
            Site::Walrus | Site::Assign | Site::Wait | Site::WaitAssign | Site::Match => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    Flat,
    Nested,
    NestedGroup,
    Blank,
    ParenBlank,
    Struct,
    Variant,
    List,
    Rest,
}

impl Shape {
    const ALL: [Shape; 9] = [
        Shape::Flat,
        Shape::Nested,
        Shape::NestedGroup,
        Shape::Blank,
        Shape::ParenBlank,
        Shape::Struct,
        Shape::Variant,
        Shape::List,
        Shape::Rest,
    ];

    fn name(self) -> &'static str {
        match self {
            Shape::Flat => "flat",
            Shape::Nested => "nested",
            Shape::NestedGroup => "nested-group",
            Shape::Blank => "blank",
            Shape::ParenBlank => "paren-blank",
            Shape::Struct => "struct",
            Shape::Variant => "variant",
            Shape::List => "list",
            Shape::Rest => "rest",
        }
    }

    /// The element value and its type.
    fn value(self) -> (&'static str, &'static str) {
        match self {
            Shape::Flat
            | Shape::Nested
            | Shape::NestedGroup
            | Shape::Blank
            | Shape::ParenBlank
            | Shape::List
            | Shape::Rest => ("(1, (2, 3))", "(int, (int, int))"),
            Shape::Struct => ("P(1, 2)", "P"),
            Shape::Variant => ("E.V(1, 2)", "E"),
        }
    }

    /// The target as written at `:=`, `=`, `for`, a comprehension and a wait arm.
    fn target(self) -> &'static str {
        match self {
            Shape::Flat => "a, b",
            Shape::Nested => "a, (b, c)",
            Shape::NestedGroup => "a, ((b, c))",
            Shape::Blank => "a, _",
            Shape::ParenBlank => "(_)",
            Shape::Struct => "P(a, b)",
            Shape::Variant => "E.V(a, b)",
            Shape::List => "[a, b]",
            Shape::Rest => "a, *r",
        }
    }

    /// The same shape as a `match` arm pattern (a tuple pattern takes parentheses there).
    fn pattern(self) -> &'static str {
        match self {
            Shape::Flat => "(a, b)",
            Shape::Nested => "(a, (b, c))",
            Shape::NestedGroup => "(a, ((b, c)))",
            Shape::Blank => "(a, _)",
            Shape::ParenBlank => "(_)",
            Shape::Struct => "P(a, b)",
            Shape::Variant => "E.V(a, b)",
            Shape::List => "[a, b]",
            Shape::Rest => "(a, *r)",
        }
    }

    /// The names the target binds, as printed.
    fn vars(self) -> &'static [&'static str] {
        match self {
            Shape::Flat | Shape::Struct | Shape::Variant | Shape::List => &["a", "b"],
            Shape::Nested | Shape::NestedGroup => &["a", "b", "c"],
            Shape::Blank => &["a"],
            Shape::ParenBlank => &["_"],
            Shape::Rest => &["a", "r"],
        }
    }

    /// The declarations `=` stores into.
    fn assign_decls(self) -> &'static [&'static str] {
        match self {
            Shape::Flat => &["a := 0", "b := (0, 0)"],
            Shape::Nested | Shape::NestedGroup => &["a := 0", "b := 0", "c := 0"],
            Shape::Blank => &["a := 0"],
            Shape::ParenBlank => &[],
            Shape::Struct | Shape::Variant | Shape::List => &["a := 0", "b := 0"],
            Shape::Rest => &["a := 0", "r := 0"],
        }
    }

    /// What an accepting cell prints. `None`: the rule rejects the cell.
    fn accepts(self, site: Site) -> Option<&'static str> {
        // D2: a wait arm binds one `T!RecvError`, so no tuple target unpacks there.
        if matches!(site, Site::Wait | Site::WaitAssign)
            && matches!(
                self,
                Shape::Flat | Shape::Nested | Shape::NestedGroup | Shape::Blank
            )
        {
            return None;
        }
        match self {
            Shape::Flat => Some("1 (2, 3)"),
            Shape::Nested | Shape::NestedGroup => Some("1 2 3"),
            Shape::Blank => Some("1"),
            // `(_)` skips the slot and binds nothing, so the cell reads `_` and meets the blank rule.
            Shape::ParenBlank => None,
            Shape::Struct | Shape::Variant => match site {
                Site::Match => Some("1 2"),
                Site::Walrus
                | Site::Assign
                | Site::For
                | Site::Comp
                | Site::Wait
                | Site::WaitAssign => None,
            },
            Shape::List | Shape::Rest => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Source {
    Tuple,
    ListOf,
    SetOf,
    Map,
    Generator,
    Channel,
    ParamBound,
    IterHook,
    NextHook,
    IterableAnn,
}

impl Source {
    const ALL: [Source; 10] = [
        Source::Tuple,
        Source::ListOf,
        Source::SetOf,
        Source::Map,
        Source::Generator,
        Source::Channel,
        Source::ParamBound,
        Source::IterHook,
        Source::NextHook,
        Source::IterableAnn,
    ];

    fn name(self) -> &'static str {
        match self {
            Source::Tuple => "tuple",
            Source::ListOf => "list",
            Source::SetOf => "set",
            Source::Map => "map",
            Source::Generator => "generator",
            Source::Channel => "channel",
            Source::ParamBound => "param-bound",
            Source::IterHook => "iter-hook",
            Source::NextHook => "next-hook",
            Source::IterableAnn => "iterable-ann",
        }
    }
}

/// Cells the grid does not generate, with the reason.
fn not_applicable(site: Site, shape: Shape, source: Source) -> bool {
    match source {
        // A tuple is not iterable, so a loop site has no tuple source.
        Source::Tuple => site.is_loop(),
        // A struct and an enum variant are not `Hashable`, and a Map element is a pair.
        Source::SetOf | Source::Map => match shape {
            Shape::Struct | Shape::Variant => true,
            Shape::Flat
            | Shape::Nested
            | Shape::NestedGroup
            | Shape::Blank
            | Shape::ParenBlank
            | Shape::List
            | Shape::Rest => false,
        },
        Source::ListOf
        | Source::Generator
        | Source::Channel
        | Source::ParamBound
        | Source::IterHook
        | Source::NextHook
        | Source::IterableAnn => false,
    }
}

fn interp(vars: &[&str]) -> String {
    let parts: Vec<String> = vars.iter().map(|v| format!("{{{v}}}")).collect();
    format!("\"{}\"", parts.join(" "))
}

/// The statements of a non-loop site, run on the element `e`.
fn element_lines(site: Site, shape: Shape, ty: &str) -> Vec<String> {
    let print = format!("print({})", shape.vars().join(", "));
    match site {
        Site::Walrus => vec![format!("{} := e", shape.target()), print],
        Site::Assign => {
            let mut lines: Vec<String> =
                shape.assign_decls().iter().map(|d| d.to_string()).collect();
            lines.push(format!("{} = e", shape.target()));
            lines.push(print);
            lines
        }
        Site::Wait => vec![
            format!("c := Channel[{ty}](1)"),
            "c.send(e)".to_string(),
            "wait:".to_string(),
            format!("    {} := c.recv():", shape.target()),
            format!("        {print}"),
        ],
        Site::WaitAssign => {
            let mut lines: Vec<String> =
                shape.assign_decls().iter().map(|d| d.to_string()).collect();
            lines.extend([
                format!("w := Channel[{ty}](1)"),
                "w.send(e)".to_string(),
                "wait:".to_string(),
                format!("    {} = w.recv():", shape.target()),
                format!("        {print}"),
            ]);
            lines
        }
        Site::Match => vec![
            "match e:".to_string(),
            format!("    {}:", shape.pattern()),
            format!("        {print}"),
        ],
        Site::For | Site::Comp => unreachable!("a loop site iterates the source"),
    }
}

/// The statements of a loop site over the expression `src`.
fn loop_lines(site: Site, shape: Shape, src: &str) -> Vec<String> {
    match site {
        Site::For => vec![
            format!("for {} in {src}:", shape.target()),
            format!("    print({})", shape.vars().join(", ")),
        ],
        Site::Comp => vec![format!(
            "print([{} for {} in {src}][0])",
            interp(shape.vars()),
            shape.target()
        )],
        Site::Walrus | Site::Assign | Site::Wait | Site::WaitAssign | Site::Match => {
            unreachable!("a non-loop site runs per element")
        }
    }
}

/// The site's statements over the collection `src`.
fn site_lines(site: Site, shape: Shape, source: Source, src: &str, ty: &str) -> Vec<String> {
    if site.is_loop() {
        return loop_lines(site, shape, src);
    }
    let mut lines = vec![format!("for e in {src}:")];
    lines.extend(
        element_lines(site, shape, ty)
            .into_iter()
            .map(|l| format!("    {l}")),
    );
    let _ = source;
    lines
}

fn indent(lines: &[String], by: &str) -> String {
    lines.iter().map(|l| format!("{by}{l}\n")).collect()
}

const DECLS: &str = "struct P:
    a: int
    b: int
enum E:
    V(int, int)
";

fn program(site: Site, shape: Shape, source: Source) -> String {
    let (value, ty) = shape.value();
    let import = match source {
        Source::Generator | Source::IterHook => "import Iterator from std.iter\n",
        Source::ParamBound | Source::IterableAnn => "import Iterable from std.iter\n",
        Source::Tuple
        | Source::ListOf
        | Source::SetOf
        | Source::Map
        | Source::Channel
        | Source::NextHook => "",
    };
    let mut out = String::from(import);
    out.push_str(DECLS);
    let mut main_lines: Vec<String> = Vec::new();
    match source {
        Source::Tuple => {
            main_lines.push(format!("e := {}", shape.value().0));
            main_lines.extend(element_lines(site, shape, shape.value().1));
        }
        Source::ListOf => {
            main_lines.extend(site_lines(site, shape, source, &format!("[{value}]"), ty));
        }
        Source::SetOf => {
            main_lines.extend(site_lines(site, shape, source, &format!("{{{value}}}"), ty));
        }
        Source::Map => {
            main_lines.extend(site_lines(site, shape, source, "{1: (2, 3)}", ty));
        }
        Source::Generator => {
            out.push_str(&format!("fn gen() -> Iterator[{ty}]:\n    yield {value}\n"));
            main_lines.extend(site_lines(site, shape, source, "gen()", ty));
        }
        Source::Channel => {
            main_lines.push(format!("ch := Channel[{ty}](1)"));
            main_lines.push(format!("ch.send({value})"));
            main_lines.push("ch.close()".to_string());
            main_lines.extend(site_lines(site, shape, source, "ch", ty));
        }
        Source::ParamBound => {
            let body = site_lines(site, shape, source, "xs", ty);
            out.push_str(&format!(
                "fn run[S: Iterable[{ty}]](xs: S):\n{}",
                indent(&body, "    ")
            ));
            main_lines.push(format!("run([{value}])"));
        }
        Source::IterableAnn => {
            let body = site_lines(site, shape, source, "xs", ty);
            out.push_str(&format!(
                "fn run(xs: Iterable[{ty}]):\n{}",
                indent(&body, "    ")
            ));
            main_lines.push(format!("run([{value}])"));
        }
        Source::IterHook => {
            out.push_str(&format!(
                "struct Bag:\n    xs: List[{ty}]\n    fn iter(self) -> Iterator[{ty}]:\n        return self.xs.iter()\n"
            ));
            main_lines.extend(site_lines(
                site,
                shape,
                source,
                &format!("Bag([{value}])"),
                ty,
            ));
        }
        Source::NextHook => {
            out.push_str(&format!(
                "struct Once:\n    done: bool\n    v: {ty}\n    fn next(self) -> {ty}?:\n        if self.done:\n            return None\n        self.done = true\n        x := self.v\n        return ?x\n"
            ));
            main_lines.extend(site_lines(
                site,
                shape,
                source,
                &format!("Once(false, {value})"),
                ty,
            ));
        }
    }
    out.push_str("fn main():\n");
    out.push_str(&indent(&main_lines, "    "));
    out.push_str("main()\n");
    out
}

/// The fragment of the rejection a (site, shape) pair gets. Recorded from the built binary's run.
fn reject_fragment(site: Site, shape: Shape) -> &'static str {
    match (site, shape) {
        (Site::Walrus, Shape::Struct | Shape::Variant | Shape::List) => {
            "left side of ':=' must be a name"
        }
        (Site::Walrus | Site::Assign | Site::Wait | Site::WaitAssign, Shape::Rest) => {
            "unexpected '*' in expression"
        }
        (Site::Assign | Site::WaitAssign, Shape::Struct | Shape::Variant | Shape::List) => {
            "invalid assignment target"
        }
        (Site::Wait, Shape::Struct | Shape::Variant | Shape::List) => {
            "left side of ':=' in a wait arm must be a name"
        }
        // One answer at every site: `(_)` is the blank target, so reading `_` is the error.
        (
            Site::Walrus
            | Site::Assign
            | Site::For
            | Site::Comp
            | Site::Wait
            | Site::WaitAssign
            | Site::Match,
            Shape::ParenBlank,
        ) => "cannot use '_' as a value",
        (Site::Wait, Shape::Flat | Shape::Nested | Shape::NestedGroup | Shape::Blank) => {
            "tuple pattern cannot match a value of type"
        }
        (Site::WaitAssign, Shape::Flat | Shape::Nested | Shape::NestedGroup | Shape::Blank) => {
            "!RecvError to 2 targets"
        }
        (Site::For | Site::Comp, Shape::Struct) => "expected 'in', found '('",
        (Site::For | Site::Comp, Shape::Variant) => "expected 'in', found '.'",
        (Site::For | Site::Comp, Shape::List) => "expected identifier, found '['",
        (Site::For | Site::Comp, Shape::Rest) => "expected identifier, found '*'",
        (Site::Match, Shape::List) => "expected identifier, found '['",
        (Site::Match, Shape::Rest) => "expected identifier, found '*'",
        // The rule accepts these cells, and `unpack_grid` never asks for their rejection.
        (
            Site::Walrus | Site::Assign | Site::For | Site::Comp | Site::Match,
            Shape::Flat | Shape::Nested | Shape::NestedGroup | Shape::Blank,
        )
        | (Site::Match, Shape::Struct | Shape::Variant) => {
            unreachable!("an accepting cell has no rejection")
        }
    }
}

#[test]
fn unpack_grid() {
    let mut cells = Vec::new();
    for site in Site::ALL {
        for shape in Shape::ALL {
            for source in Source::ALL {
                if not_applicable(site, shape, source) {
                    continue;
                }
                let expect = match shape.accepts(site) {
                    Some(out) => Expect::Prints(out.to_string()),
                    None => Expect::Rejects(reject_fragment(site, shape)),
                };
                cells.push(Cell {
                    name: format!("{} x {} x {}", site.name(), shape.name(), source.name()),
                    files: vec![("main.chz".to_string(), program(site, shape, source))],
                    expect,
                });
            }
        }
    }
    run_grid("unpack", &cells, &[]);
}

/// The shape a `match` arm pattern is, naming every `Pattern` variant: a new variant breaks this
/// build until the grid decides what it is. `None`: not one of the grid's shapes.
fn shape_of(p: &Pattern) -> Option<Shape> {
    match p {
        Pattern::Tuple(subs) => match subs.as_slice() {
            [Pattern::Ident(..), Pattern::Ident(..)] => Some(Shape::Flat),
            [Pattern::Ident(..), Pattern::Tuple(_)] => Some(Shape::Nested),
            [Pattern::Ident(..), Pattern::Wildcard] => Some(Shape::Blank),
            _ => None,
        },
        Pattern::Wildcard => Some(Shape::ParenBlank),
        Pattern::Variant { enum_name, .. } => match enum_name {
            Some(_) => Some(Shape::Variant),
            None => Some(Shape::Struct),
        },
        Pattern::Ident(..)
        | Pattern::Literal(_)
        | Pattern::Range { .. }
        | Pattern::Or(_)
        | Pattern::Carrier { .. } => None,
    }
}

/// The shape axis is the parser's: each spelling in `Shape::pattern` parses to the pattern
/// `shape_of` names, and a list or `*rest` pattern does not parse.
#[test]
fn shape_axis_matches_the_parser() {
    for shape in Shape::ALL {
        let src = format!("match e:\n    {}:\n        pass\n", shape.pattern());
        let parsed = chezzi::parser::parse(chezzi::lexer::tokenize(&src).unwrap());
        let want = match shape {
            Shape::NestedGroup => Some(Shape::Nested),
            Shape::Flat
            | Shape::Nested
            | Shape::Blank
            | Shape::ParenBlank
            | Shape::Struct
            | Shape::Variant => Some(shape),
            Shape::List | Shape::Rest => None,
        };
        let got = match parsed {
            Ok(m) => {
                let StmtKind::Match { arms, .. } = &m.stmts[0].kind else {
                    panic!("{}: expected a match", shape.name());
                };
                shape_of(&arms[0].pattern)
            }
            Err(_) => None,
        };
        assert_eq!(got, want, "{}", shape.name());
    }
}

#[test]
fn axes_are_complete() {
    let sites: Vec<_> = Site::ALL.iter().map(|s| s.name()).collect();
    let shapes: Vec<_> = Shape::ALL.iter().map(|s| s.name()).collect();
    let sources: Vec<_> = Source::ALL.iter().map(|s| s.name()).collect();
    for names in [&sites, &shapes, &sources] {
        let mut sorted = (*names).clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
    }
    assert_eq!((sites.len(), shapes.len(), sources.len()), (7, 9, 10));
}

/// A bare tuple ends a statement: `return`, `:=`, a typed `:=`, `yield`, `=`. Each prints its tuple
/// (CPython: `x = 1, 2` is `(1, 2)`; Go returns two values).
#[test]
fn bare_tuple_ends_a_statement() {
    let cell = |name: &str, body: &str, want: &str| Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), format!("{body}\nmain()\n"))],
        expect: Expect::Prints(want.to_string()),
    };
    let reject_cell = |name: &str, body: &str, want: &'static str| Cell {
        name: name.to_string(),
        files: vec![("main.chz".to_string(), format!("{body}\nmain()\n"))],
        expect: Expect::Rejects(want),
    };
    run_grid(
        "bare-tuple",
        &[
            cell(
                "return",
                "fn f(a: int, b: int) -> (int, int):\n    return a / b, a % b\nfn main():\n    print(f(7, 2))",
                "(3, 1)",
            ),
            cell(
                "walrus",
                "fn main():\n    x := 1, 2\n    print(x)",
                "(1, 2)",
            ),
            cell(
                "typed walrus",
                "fn main():\n    x: (int, int) = 1, 2\n    print(x)",
                "(1, 2)",
            ),
            cell(
                "yield",
                "import Iterator from std.iter\nfn g() -> Iterator[(int, int)]:\n    yield 1, 2\nfn main():\n    for p in g():\n        print(p)",
                "(1, 2)",
            ),
            cell(
                "assign",
                "fn main():\n    x := (0, 0)\n    x = 3, 4\n    print(x)",
                "(3, 4)",
            ),
            cell(
                "grouped name takes a value list",
                "fn main():\n    (y) := 1, 2\n    print(y)",
                "(1, 2)",
            ),
            cell(
                "grouped for target",
                "fn main():\n    for (z) in [1]:\n        print(z)",
                "1",
            ),
            cell(
                "grouped comprehension target",
                "fn main():\n    print([z for (z) in [1]])",
                "[1]",
            ),
            cell(
                "destructure value list",
                "fn main():\n    a, b := 1, 2\n    print(a, b)",
                "1 2",
            ),
            cell(
                "tuple assign value list",
                "fn main():\n    a := 0\n    b := 0\n    a, b = 1, 2\n    a, b = b, a\n    print(a, b)",
                "2 1",
            ),
            Cell {
                name: "destructure value list arity".to_string(),
                files: vec![(
                    "main.chz".to_string(),
                    "fn main():\n    a, b := 1, 2, 3\n    print(a, b)\nmain()\n".to_string(),
                )],
                expect: Expect::Rejects("tuple pattern has 2 element(s), but the value has 3"),
            },
            // U9: a multi-name target takes no outer parentheses, at every site.
            reject_cell(
                "outer parens at :=",
                "fn main():\n    (a, b) := (1, 2)",
                "left side of ':=' must be a name",
            ),
            reject_cell(
                "outer parens at =",
                "fn main():\n    a := 0\n    b := 0\n    (a, b) = (1, 2)",
                "invalid assignment target",
            ),
            reject_cell(
                "outer parens at for",
                "fn main():\n    for (a, b) in [(1, 2)]:\n        pass",
                "takes no outer parentheses",
            ),
            reject_cell(
                "outer parens in a comprehension",
                "fn main():\n    print([a for (a, b) in [(1, 2)]])",
                "takes no outer parentheses",
            ),
            reject_cell(
                "outer parens in a wait arm",
                "fn main():\n    ch := Channel[(int, int)](1)\n    ch.send((1, 2))\n    wait:\n        (a, b) := ch.recv():\n            pass",
                "left side of ':=' in a wait arm must be a name",
            ),
            Cell {
                name: "value list takes no else guard".to_string(),
                files: vec![(
                    "main.chz".to_string(),
                    "fn main():\n    a, b := 1, 2 else:\n        return\nmain()\n".to_string(),
                )],
                expect: Expect::Rejects("expected end of line, found 'else'"),
            },
        ],
        &[],
    );
}
