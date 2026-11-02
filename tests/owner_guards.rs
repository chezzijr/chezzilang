//! One owner per fact (`docs/contributing/owners.md`, CLAUDE.md "Derive, don't mirror"). Each rule names a
//! spelling that decides a fact and the one file allowed to hold it. The scan reads production code
//! only: comments, string literals and `#[cfg(test)] mod NAME { .. }` blocks are removed first, and
//! test files (a `tests/` directory, `*tests.rs`, `test_support.rs`) are skipped. A second copy of a
//! fact fails here, at build time, instead of in review.

use std::fs;
use std::path::{Path, PathBuf};

/// (spelling, the one `src/`-relative file that may hold it, the fact and its owner)
const RULES: &[(&str, &str, &str)] = &[
    (
        "== \"_\"",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "!= \"_\"",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "\"_\" ==",
        "ast/mod.rs",
        "is this name the blank `_`: call `ast::is_blank_name`",
    ),
    (
        "== \"self\"",
        "ast/mod.rs",
        "is this the receiver `self`: call `ast::is_self_name` / `Param::is_self` / `FnDecl::takes_self`",
    ),
    (
        "!= \"self\"",
        "ast/mod.rs",
        "is this the receiver `self`: call `ast::is_self_name` / `Param::is_self` / `FnDecl::takes_self`",
    ),
    (
        "\"self\".to_string()",
        "ast/mod.rs",
        "the receiver's name: use `ast::SELF`",
    ),
    (
        "ExprKind::Ident(_) | ExprKind::Field { .. })",
        "ast/mod.rs",
        "may `head[X]` take type arguments: call `ExprKind::is_named_head`",
    ),
    (
        "struct TmpDir",
        "test_support.rs",
        "the unit-test fixture directory: use `test_support::TmpDir`",
    ),
];

/// (message text, the one `src/`-relative file that may hold it, the fact and its owner). Unlike
/// [`RULES`], these match inside string literals.
const MESSAGE_RULES: &[(&str, &str, &str)] = &[(
    "not allowed inside a",
    "checker/boundary.rs",
    "what may cross a spawn, defer, recover or generator block: read `boundary::RULES`",
)];

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).unwrap_or_else(|e| panic!("read {}: {e}", d.display())) {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn is_test_file(rel: &Path) -> bool {
    let name = rel.file_name().unwrap().to_string_lossy();
    rel.components().any(|c| c.as_os_str() == "tests")
        || name.ends_with("tests.rs")
        || name == "test_support.rs"
}

/// `text` without `//` comments, string-literal contents and inline `#[cfg(test)] mod NAME { .. }`
/// blocks. String delimiters stay (`""`), so a spelling with quotes still matches only code. With
/// `keep_strings`, string contents stay.
fn production_code(text: &str, keep_strings: bool) -> String {
    let mut out = String::new();
    let mut lines = text.lines().peekable();
    let mut skipping = false;
    while let Some(line) = lines.next() {
        if skipping {
            if line.starts_with('}') {
                skipping = false;
            }
            continue;
        }
        if line == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|n| n.starts_with("mod ") && n.trim_end().ends_with('{'))
        {
            lines.next();
            skipping = true;
            continue;
        }
        out.push_str(&strip_line(line, keep_strings));
        out.push('\n');
    }
    out
}

/// One line without its `//` comment; a string literal keeps its quotes and loses its contents,
/// except a short identifier-like literal (`"_"`, `"self"`, `"Add"`): the fact spellings this test
/// looks for are names, never prose.
fn strip_line(line: &str, keep_strings: bool) -> String {
    let mut out = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            break;
        }
        if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.extend(&chars[i..i + 3]);
            i += 3;
            continue;
        }
        if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            let body: String = chars[start + 1..i.min(chars.len())].iter().collect();
            out.push('"');
            let is_name = keep_strings
                || body.chars().count() <= 16
                    && !body.is_empty()
                    && body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if is_name {
                out.push_str(&body);
            }
            out.push('"');
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[test]
fn each_fact_is_spelled_only_by_its_owner() {
    let src = Path::new("src");
    let mut copies = Vec::new();
    for path in rust_files(src) {
        let rel = path.strip_prefix(src).unwrap();
        if is_test_file(rel) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let code = production_code(&text, false);
        let with_strings = production_code(&text, true);
        let rules = RULES
            .iter()
            .map(|r| (r, &code))
            .chain(MESSAGE_RULES.iter().map(|r| (r, &with_strings)));
        for ((spelling, owner, fact), code) in rules {
            if rel == Path::new(owner) {
                continue;
            }
            for (n, line) in code.lines().enumerate() {
                if line.contains(spelling) {
                    copies.push(format!(
                        "src/{}:{}: `{spelling}` ({fact}; owner {owner})",
                        rel.display(),
                        n + 1
                    ));
                }
            }
        }
    }
    assert!(
        copies.is_empty(),
        "a second copy of a fact with an owner:\n{}",
        copies.join("\n")
    );
}

/// The scan itself: a spelling inside a comment or a longer string never counts; one in code does.
#[test]
fn the_scan_reads_code_only() {
    let code = production_code(
        "let a = x == \"_\"; // y == \"_\"\nlet b = \"a == \\\"_\\\" here\";\n",
        false,
    );
    assert_eq!(code.matches("== \"_\"").count(), 1, "{code}");
    let gated = production_code(
        "#[cfg(test)]\nmod tests {\n    fn f() { x == \"_\" }\n}\nfn g() {}\n",
        false,
    );
    assert!(!gated.contains("== \"_\""), "{gated}");
    assert!(gated.contains("fn g()"), "{gated}");
    let named = production_code(
        "if p.name == \"self\" {}\nlet m = \"say p == \\\"self\\\" here\";\n",
        false,
    );
    assert_eq!(named.matches("== \"self\"").count(), 1, "{named}");
    let message = production_code(
        "// is not allowed inside a\nlet m = \"is not allowed inside a x\";\n",
        true,
    );
    assert_eq!(
        message.matches("not allowed inside a").count(),
        1,
        "{message}"
    );
}

/// Every line of `code` that spells a protocol method name (a `PROTOCOL_METHODS` row) as a literal
/// in a place that decides a method: a comparison (`== "add"`), a `matches!(x, .. "add" ..)`, an
/// arm of a `match` (`"add" =>`, `("add", 1) =>`), a `"Add", "add"` protocol and method pair, or a
/// `name: "add"` field. The std decl owns the name: such a site reads `Hook::method` or
/// `Unhooked::method`.
fn protocol_method_literals(code: &str) -> Vec<(usize, String)> {
    let rows = chezzi::native::std_embed::PROTOCOL_METHODS;
    let mut hits = Vec::new();
    let mut in_match: Option<usize> = None;
    for (n, line) in code.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        if in_match.is_some_and(|at| !line.trim().is_empty() && indent <= at) {
            in_match = None;
        }
        let arm = in_match.is_some();
        let trimmed = line.trim();
        if (trimmed.starts_with("match ") || trimmed.contains(" match ")) && trimmed.ends_with('{')
        {
            in_match = Some(indent);
        }
        let in_matches = line.find("matches!(");
        for (protocol, method) in rows {
            let lit = format!("\"{method}\"");
            let decides = line.contains(&format!("== {lit}"))
                || line.contains(&format!("!= {lit}"))
                || line.contains(&format!("{lit} =="))
                || line.contains(&format!("\"{protocol}\", {lit}"))
                || line.contains(&format!("name: {lit}"))
                || in_matches.is_some_and(|at| line[at..].contains(&lit))
                || arm
                    && (trimmed.starts_with(&format!("{lit} "))
                        || trimmed.starts_with(&format!("({lit},")));
            if decides && !hits.contains(&(n + 1, method.to_string())) {
                hits.push((n + 1, method.to_string()));
            }
        }
    }
    hits
}

/// The sites that spell a protocol method's name for a method of another owner:
/// `(src-relative file, the code on the line, why it is not a protocol method)`.
const NOT_PROTOCOL_METHODS: &[(&str, &str, &str)] = &[
    (
        "checker/satisfies.rs",
        "matches!(method, \"contains\" | \"index_of\" | \"dedup\" | \"unique\")",
        "the `List` methods that need an `Eq` element; the native `List` decl owns their names",
    ),
    (
        "checker/method_call.rs",
        "matches!(method, \"contains\" | \"index_of\" | \"dedup\" | \"unique\")",
        "the `List` methods that need an `Eq` element; the native `List` decl owns their names",
    ),
    (
        "checker/method_call.rs",
        "matches!(method, \"add\" | \"sub\")",
        "the `Atomic` `add`/`sub` methods; the native `Atomic` decl owns their names",
    ),
    (
        "checker/builtin_methods.rs",
        "\"str\" => \"\"",
        "the builtin type name `str` in `builtin_type_doc` (the scan blanks the long doc text)",
    ),
];

#[test]
fn a_protocol_method_name_is_read_from_its_std_decl() {
    let mut copies = Vec::new();
    for dir in ["checker", "compiler", "vm", "base"] {
        let root = Path::new("src");
        for path in rust_files(&root.join(dir)) {
            let rel = path.strip_prefix(root).unwrap();
            if is_test_file(rel) {
                continue;
            }
            let code = production_code(&fs::read_to_string(&path).unwrap(), false);
            let lines: Vec<&str> = code.lines().collect();
            for (line, method) in protocol_method_literals(&code) {
                let exempt = NOT_PROTOCOL_METHODS.iter().any(|(file, site, _)| {
                    rel == Path::new(file) && lines[line - 1].contains(site)
                });
                if !exempt {
                    copies.push(format!("src/{}:{line}: \"{method}\"", rel.display()));
                }
            }
        }
    }
    assert!(
        copies.is_empty(),
        "a protocol method name spelled outside its std decl (read `Hook::method` or \
         `hooks::protocol_methods`):\n{}",
        copies.join("\n")
    );
    // The rule itself: each shape counts once; a type name or an arity text does not.
    let sample = "if method == \"add\" {}\nmatch (method, n) {\n    (\"neg\", 0) => 1,\n    \"str\" => 2,\n}\nself.hook_method(ty, \"Eq\", \"eq\");\nOp::CallMethod { name: \"next\".to_string() };\nlet t = \"str\";\nself.arity_err(\"contains\", args, 1, span);\nif matches!(m, \"shl\" | \"x\") {}\nmatch name {\n    \"hash\" => 3,\n}\n";
    let found: Vec<String> = protocol_method_literals(&production_code(sample, false))
        .into_iter()
        .map(|(_, m)| m)
        .collect();
    assert_eq!(
        found,
        ["add", "neg", "str", "eq", "next", "shl", "hash"],
        "{found:?}"
    );
    // Each exemption still names a live site: a deleted site takes its row with it.
    for (file, site, why) in NOT_PROTOCOL_METHODS {
        let code = production_code(
            &fs::read_to_string(Path::new("src").join(file)).unwrap(),
            false,
        );
        assert!(
            code.contains(site),
            "stale exemption src/{file}: `{site}` ({why})"
        );
    }
}
