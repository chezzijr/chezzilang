//! TICKET-260 (owner rule U8): a block expression (a multi-line `match`, a `recover:` block) is legal
//! where a whole value starts and nowhere else; the one-line forms (the inline `recover: expr`, the
//! one-line `if … else`, a lambda) are ordinary expressions legal in every expression position.
//!
//! Rows are the positions: one per `ValueSite` variant (the parser's own list, so a new variant fails
//! to compile in `site_row` until it has a template) plus one per other grammar rule whose right side
//! holds an `<expr>` or a `<value>` (`every_expr_rule_has_a_row` reads `docs/reference/grammar.bnf`). Columns
//! are the five forms. Each cell writes one program, runs `chezzi check` on it and, when the cell is
//! an accepted program in a typed row, `chezzi run`.
//!
//! "Accepted" means the parser took the program (no `resolve error`); a type error in a position that
//! has no universal type is not a position verdict. A block form in a non-value row must be refused
//! with the one message that names the rule: `a block expression`.

use chezzi::parser::ValueSite;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

/// One position: the grammar rules it exercises, whether a block expression may start there, the
/// program with `{F}` where the form goes, and the indent of the line that holds `{F}` (continuation
/// lines of a block form sit four spaces deeper).
struct Row {
    name: String,
    rules: &'static [&'static str],
    blocks_ok: bool,
    /// The slot has the universal type `Any`: an accepted cell must check clean and run.
    typed: bool,
    indent: usize,
    template: &'static str,
}

/// A form of the grid.
struct Form {
    name: &'static str,
    /// Written over several lines: `head`, then one line per `body` entry.
    block: bool,
    head: &'static str,
    body: &'static [&'static str],
}

const FORMS: [Form; 5] = [
    Form {
        name: "multi-line match",
        block: true,
        head: "match n:",
        body: &["1: 10", "_: 20"],
    },
    Form {
        name: "recover block",
        block: true,
        head: "recover:",
        body: &["print(\"in\")"],
    },
    Form {
        name: "inline recover",
        block: false,
        head: "recover: n + 1",
        body: &[],
    },
    Form {
        name: "one-line if",
        block: false,
        head: "if n == 1: 10 else: 20",
        body: &[],
    },
    Form {
        name: "lambda",
        block: false,
        head: "fn(): 5",
        body: &[],
    },
];

const PRELUDE: &str = "n := 1\nxs := [1]\nch := Channel[int](1)\n";

/// The row of one `ValueSite`. The match names every variant.
fn site_row(site: ValueSite) -> Row {
    let name = format!("site: {}", site.name());
    let value = |rules, typed, indent, template| Row {
        name: name.clone(),
        rules,
        blocks_ok: true,
        typed,
        indent,
        template,
    };
    match site {
        ValueSite::Let => value(
            &["letStmt", "plainLet", "value"],
            true,
            0,
            "x: Any = {F}\nprint(x)\n",
        ),
        ValueSite::Assign => value(&["assignStmt"], true, 0, "x: Any = 0\nx = {F}\nprint(x)\n"),
        ValueSite::DestructLet => value(&[], false, 0, "a, b := {F}\n"),
        ValueSite::TupleAssign => value(&[], false, 0, "a, b := 1, 2\na, b = {F}\n"),
        ValueSite::Return => value(
            &["returnStmt"],
            true,
            4,
            "fn f() -> Any:\n    return {F}\nprint(f())\n",
        ),
        ValueSite::Yield => value(
            &["yieldStmt"],
            true,
            4,
            "fn g() -> Iterator[Any]:\n    yield {F}\nfor v in g():\n    print(v)\n",
        ),
        ValueSite::Lambda => value(&["closureBody"], true, 0, "f := fn(): {F}\nprint(f())\n"),
        ValueSite::FnBody => value(&[], true, 0, "fn g() -> Any: {F}\nprint(g())\n"),
        ValueSite::FieldDefault => value(
            &["field"],
            true,
            4,
            "struct S:\n    a: Any = {F}\n    b: int = 0\nprint(S().b)\n",
        ),
        ValueSite::ParamDefault => value(
            &["param", "valueExpr"],
            true,
            0,
            "fn f(a: Any = {F}) -> Any:\n    return a\nprint(f())\n",
        ),
        ValueSite::Stmt => value(&["exprStmt"], false, 0, "{F}\n"),
        ValueSite::MatchArm => value(
            &["exprArm"],
            true,
            4,
            "r: Any = match n:\n    1: {F}\n    _: 0\nprint(r)\n",
        ),
    }
}

/// A position that is no value site, or a statement shape of its own: block forms are refused.
fn operand_row(
    name: &'static str,
    rules: &'static [&'static str],
    indent: usize,
    template: &'static str,
) -> Row {
    Row {
        name: format!("operand: {name}"),
        rules,
        blocks_ok: false,
        typed: false,
        indent,
        template,
    }
}

fn rows() -> Vec<Row> {
    let mut rows: Vec<Row> = ValueSite::ALL.iter().map(|s| site_row(*s)).collect();
    // value positions the sites do not name: the walrus let and the inline let
    rows.push(Row {
        name: "site: walrus let".to_string(),
        rules: &["plainLet", "letStmt"],
        blocks_ok: true,
        typed: true,
        indent: 0,
        template: "x := {F}\nprint(x)\n",
    });
    rows.push(Row {
        name: "site: let in an inline body".to_string(),
        rules: &["ifLineStmt", "inlineStmt"],
        blocks_ok: true,
        typed: false,
        indent: 0,
        template: "if true: _ := {F}\n",
    });
    let rest: &[(&'static str, &'static [&'static str], usize, &'static str)] = &[
        ("call argument", &["arg", "exprList"], 0, "print({F})\n"),
        ("named argument", &["arg"], 0, "print(1, end={F})\n"),
        ("list element", &["primary"], 0, "ys := [{F}]\n"),
        ("tuple element", &["primary"], 0, "t := ({F}, 1)\n"),
        ("map value", &["mapEntry"], 0, "m := {\"a\": {F}}\n"),
        ("parenthesized value", &["primary"], 0, "y := ({F})\n"),
        (
            "interpolation hole",
            &["primary"],
            0,
            "print(\"{({F})}\")\n",
        ),
        (
            "interpolation hole in a value",
            &["primary"],
            0,
            "y := \"{({F})}\"\nprint(y)\n",
        ),
        ("binary operand", &["primary"], 0, "y := 1 + {F}\n"),
        ("prefix operand", &["primary"], 0, "y := -{F}\n"),
        ("not operand", &["primary"], 0, "y := not {F}\n"),
        ("not operand in a call", &["primary"], 0, "print(not {F})\n"),
        ("index", &["subscript"], 0, "y := xs[{F}]\n"),
        ("lvalue index", &["lvalue"], 0, "xs[{F}] = 1\n"),
        ("inline if body", &["inlineStmt"], 0, "if true: {F}\n"),
        (
            "compound assignment",
            &["assignStmt"],
            0,
            "x := 0\nx += {F}\n",
        ),
        ("value list", &["exprList"], 0, "a, b := 1, {F}\n"),
        ("assert operand", &["assertStmt"], 0, "assert {F}\n"),
        (
            "defer operand",
            &["deferStmt"],
            4,
            "fn f():\n    defer {F}\nf()\n",
        ),
        ("if condition", &["ifStmt"], 0, "if {F}:\n    pass\n"),
        (
            "elif condition",
            &["elseIf"],
            0,
            "if false:\n    pass\nelif {F}:\n    pass\n",
        ),
        ("for iterable", &["forStmt"], 0, "for i in {F}:\n    pass\n"),
        (
            "while condition",
            &["whileStmt"],
            0,
            "while {F}:\n    pass\n",
        ),
        (
            "match scrutinee",
            &["matchStmt"],
            0,
            "match {F}:\n    _: pass\n",
        ),
        ("arm guard", &["arm"], 4, "match n:\n    _ if {F}: pass\n"),
        (
            "comprehension iterable",
            &["compClause"],
            0,
            "zs := [i for i in {F}]\n",
        ),
        (
            "comprehension guard",
            &["compGuards"],
            0,
            "zs := [i for i in xs if {F}]\n",
        ),
        (
            "match expression scrutinee",
            &["matchExpr"],
            0,
            "r := match {F}:\n    _: 0\n",
        ),
        (
            "if-expression condition",
            &["ifExpr"],
            0,
            "r := if {F}: 1 else: 2\n",
        ),
        (
            "if-expression else branch",
            &["ifExprTail"],
            0,
            "r := if n == 1: 1 else: {F}\n",
        ),
        (
            "spawn call argument",
            &["spawnCallStmt"],
            4,
            "fn w(a: Any):\n    pass\nparallel:\n    spawn w({F})\n",
        ),
        (
            "wait arm body",
            &["waitArm"],
            4,
            "wait:\n    v := ch.recv(): {F}\n    else: pass\n",
        ),
    ];
    for (name, rules, indent, template) in rest {
        rows.push(operand_row(name, rules, *indent, template));
    }
    rows
}

/// The program of one cell.
fn render(row: &Row, form: &Form) -> String {
    let mut text = form.head.to_string();
    let pad = " ".repeat(row.indent + 4);
    for line in form.body {
        text.push('\n');
        text.push_str(&pad);
        text.push_str(line);
    }
    format!("{PRELUDE}{}", row.template.replace("{F}", &text))
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

fn chezzi(dir: &PathBuf, cmd: &str) -> Run {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(cmd)
        .arg("main.chz")
        .current_dir(dir)
        .output()
        .expect("spawn chezzi");
    Run {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn block_forms_are_values_only_and_one_line_forms_are_expressions() {
    let root = std::env::temp_dir().join(format!("chezzi-block-value-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut red = Vec::new();
    let mut cells = 0;
    for (r, row) in rows().iter().enumerate() {
        for (f, form) in FORMS.iter().enumerate() {
            cells += 1;
            let src = render(row, form);
            let dir = root.join(format!("r{r}f{f}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("main.chz"), &src).unwrap();
            let check = chezzi(&dir, "check");
            let parsed = !check.stderr.contains("resolve error");
            let cell = format!("{} x {}", row.name, form.name);
            if form.block && !row.blocks_ok {
                if parsed || !check.stderr.contains("a block expression") {
                    red.push(format!(
                        "{cell}: want `a block expression` rejection; got {:?}\n{src}",
                        check.stderr.lines().next().unwrap_or("")
                    ));
                }
                continue;
            }
            if !parsed {
                red.push(format!(
                    "{cell}: want the parser to accept; got {:?}\n{src}",
                    check.stderr.lines().next().unwrap_or("")
                ));
                continue;
            }
            if row.typed {
                let run = chezzi(&dir, "run");
                if !check.ok || !run.ok {
                    red.push(format!(
                        "{cell}: want check and run clean; check {:?} run {:?} {:?}\n{src}",
                        check.stderr.lines().next().unwrap_or(""),
                        run.stdout,
                        run.stderr.lines().next().unwrap_or("")
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        red.is_empty(),
        "{} of {cells} cells red:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// Rows whose output is pinned: the block form has type `!`, so `print(s)` prints `None`, and a read
/// of its tail as a `T` is a type error that names the rule.
#[test]
fn a_recover_block_prints_none_and_has_no_tail_value() {
    let root = std::env::temp_dir().join(format!("chezzi-recover-tail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("main.chz"),
        "x := 10\ns := recover:\n    x / 2\nprint(s)\n",
    )
    .unwrap();
    let run = chezzi(&root, "run");
    assert!(run.ok, "{}", run.stderr);
    assert_eq!(run.stdout.trim_end(), "None");
    std::fs::write(
        root.join("main.chz"),
        "s := recover:\n    x := 10\n    x / 2\nt: int! = s\nprint(t)\n",
    )
    .unwrap();
    let check = chezzi(&root, "check");
    assert!(!check.ok);
    assert!(check.stderr.contains("has type `!`"), "{}", check.stderr);
    let _ = std::fs::remove_dir_all(&root);
}

/// Every grammar rule whose right side holds an `<expr>` or a `<value>` has a row.
#[test]
fn every_expr_rule_has_a_row() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/reference/grammar.bnf"
    ))
    .unwrap();
    let mut rules: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('<')
            && let Some((name, body)) = rest.split_once(">")
            && let Some(body) = body.trim_start().strip_prefix("::=")
        {
            rules.push((name.to_string(), body.to_string()));
        } else if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(last) = rules.last_mut()
        {
            last.1.push(' ');
            last.1.push_str(line);
        }
    }
    let wanted: BTreeSet<String> = rules
        .iter()
        .filter(|(_, body)| body.contains("<expr>") || body.contains("<value>"))
        .map(|(name, _)| name.clone())
        .collect();
    assert!(
        wanted.len() > 20,
        "the grammar scan found {} rules",
        wanted.len()
    );
    let covered: BTreeSet<String> = rows()
        .iter()
        .flat_map(|r| r.rules.iter().map(|s| s.to_string()))
        .collect();
    let missing: Vec<&String> = wanted.difference(&covered).collect();
    assert!(
        missing.is_empty(),
        "grammar rules with an <expr> or a <value> and no row: {missing:?}"
    );
}
