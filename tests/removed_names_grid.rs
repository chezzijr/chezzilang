//! TICKET-228 (D6) — `Option`, `Result`, `Some`, `Ok` and `Err` are no longer user syntax.
//! Every bare use is rejected with a message that names the replacement; a member position
//! (`Shadow.Some`, `s.Ok`) stays legal and RUNS; the prelude, which declares the two enums, still
//! parses for every reader; and no user-visible channel prints one of the five words.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const REMOVED: [&str; 5] = ["Option", "Result", "Some", "Ok", "Err"];

/// The first removed name `text` holds as a whole word (a maximal run of alphanumerics and `_`).
fn removed_word(text: &str) -> Option<&'static str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find_map(|w| REMOVED.iter().copied().find(|r| *r == w))
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn chezzi(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn chezzi")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A fresh directory holding `main.chz`.
fn scratch(tag: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chezzi-removed-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    dir
}

/// Every `.chz` under the five source trees, sorted.
fn chz_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "chz") {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for tree in ["std", "examples", "tests/chz", "tests/corpus", "benches"] {
        walk(&root().join(tree), &mut out);
    }
    out.sort();
    out
}

#[test]
fn each_removed_name_is_rejected_with_its_replacement() {
    let names = [
        ("Option", "`Option` is removed; write `T?`"),
        ("Result", "`Result[T, E]` is removed; write `T!E`"),
        ("Some", "`Some(x)` is removed; write `x` or `?x`"),
        ("Ok", "`Ok(x)` is removed; write `x` or `?x`"),
        ("Err", "`Err(e)` is removed; write `!e`"),
    ];
    let positions = [
        ("type", "fn f(x: NAME) -> None:\n    pass\n"),
        ("expression", "x := NAME(1)\nprint(x)\n"),
        (
            "pattern",
            "match 1:\n    NAME(v): print(v)\n    _: print(0)\n",
        ),
        ("import", "import NAME from lib\n"),
        ("import-from", "import x from NAME\n"),
        ("hole", "print(\"{NAME(1)}\")\n"),
    ];
    let mut red = Vec::new();
    for (name, want) in names {
        for (pos, tpl) in positions {
            let dir = scratch(&format!("{name}-{pos}"), &tpl.replace("NAME", name));
            for cmd in ["check", "run"] {
                let out = chezzi(&[cmd, "main.chz"], &dir);
                let t = text(&out);
                if out.status.success() || !t.contains(want) {
                    red.push(format!("{name}/{pos}/{cmd}: want {want:?}, got {t:?}"));
                }
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

#[test]
fn a_member_named_like_a_removed_name_still_runs() {
    let cells = [
        (
            "variant",
            "enum Shadow:\n    Some(int)\n    Nope\nx := Shadow.Some(5)\nmatch x:\n    Shadow.Some(v): print(v)\n    Shadow.Nope: print(0)\n",
            "5\n",
        ),
        (
            "field",
            "struct S:\n    Ok: int\ns := S(1)\nprint(s.Ok)\n",
            "1\n",
        ),
    ];
    for (tag, src, want) in cells {
        let dir = scratch(tag, src);
        let out = chezzi(&["run", "main.chz"], &dir);
        assert!(out.status.success(), "{tag}: {}", text(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout), want, "{tag}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn the_prelude_parses_for_every_reader() {
    let out = chezzi(&["check", "std/prelude.chz"], &root());
    assert!(text(&out).contains("ok: no type errors"), "{}", text(&out));
    let out = chezzi(&["ast", "std/prelude.chz"], &root());
    assert!(out.status.success(), "{}", text(&out));
}

/// The exemption is derived from a `native enum` declaration, and that is no user escape: the
/// checker rejects the declaration, and a name whose owner is not declared stays removed.
#[test]
fn a_user_native_enum_is_no_escape() {
    let dir = scratch(
        "native",
        "native enum Option[T]:\n    Some(T)\n    None\nx: Option[int] = None\ny := Ok(1)\n",
    );
    let out = chezzi(&["ast", "main.chz"], &dir);
    assert!(text(&out).contains("`Ok(x)` is removed"), "{}", text(&out));
    std::fs::write(
        dir.join("main.chz"),
        "native enum Option[T]:\n    Some(T)\n    None\nx: Option[int] = None\n",
    )
    .unwrap();
    let out = chezzi(&["check", "main.chz"], &dir);
    assert!(
        text(&out)
            .contains("native enum declarations are only allowed in standard-library modules"),
        "{}",
        text(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn no_tracked_source_spells_a_removed_name() {
    let mut red = Vec::new();
    for path in chz_files() {
        if path.ends_with("std/prelude.chz") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let Ok(tokens) = chezzi::lexer::tokenize(&src) else {
            continue;
        };
        if let Err(e) = chezzi::parser::parse(tokens)
            && e.message.contains("is removed")
        {
            red.push(format!("{}: {e}", path.display()));
        }
    }
    assert!(red.is_empty(), "{}", red.join("\n"));
}

/// Hover prints a declaration's comment, so a comment is part of the surface. This reads the
/// lexer's comment table, the one input of the parser's doc harvest; it has no allow-list.
#[test]
fn no_comment_names_a_removed_spelling() {
    let mut red = Vec::new();
    for path in chz_files() {
        let src = std::fs::read_to_string(&path).unwrap();
        let Ok((_, comments)) = chezzi::lexer::tokenize_with_comments(&src, 0) else {
            continue;
        };
        for (line, body) in comments {
            if removed_word(&body).is_some() {
                red.push(format!("{}:{line}: {body}", path.display()));
            }
        }
    }
    assert!(
        red.is_empty(),
        "{} comments:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// A carrier alias is a type spelling, never a variant head; a user-enum alias still is one.
#[test]
fn carrier_alias_heads_are_rejected() {
    let o = "type O = int?\n";
    let r = "type R = int!str\n";
    let some = "Some";
    let programs = [
        format!("{o}x := O.{some}(1)\nprint(x)\n"),
        format!("{o}x: O = O.None\nprint(x)\n"),
        format!("{r}x := R.Err(\"a\")\nprint(x)\n"),
        format!(
            "{o}fn f(x: int?) -> int:\n    match x:\n        O.{some}(v): return v\n        O.None: return 0\nprint(f(3))\n"
        ),
        format!(
            "{r}fn f(x: int!str) -> int:\n    match x:\n        R.Ok(v): return v\n        R.Err(e): return 0\nprint(f(3))\n"
        ),
    ];
    for (i, src) in programs.iter().enumerate() {
        let dir = scratch(&format!("alias{i}"), src);
        let out = chezzi(&["check", "main.chz"], &dir);
        assert!(!out.status.success(), "must be rejected:\n{src}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    let dir = scratch(
        "alias-user",
        "enum E:\n    A(int)\n    B\ntype F = E\nfn f(x: E) -> int:\n    match x:\n        F.A(v): return v\n        F.B: return 0\nprint(f(F.A(1)))\n",
    );
    let out = chezzi(&["run", "main.chz"], &dir);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "1\n",
        "{}",
        text(&out)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn goldens_name_no_removed_spelling() {
    let mut red = Vec::new();
    for path in chz_files() {
        let golden = path.with_extension("expected");
        if !path.starts_with(root().join("examples")) || !golden.exists() {
            continue;
        }
        if removed_word(&std::fs::read_to_string(&path).unwrap()).is_some() {
            continue;
        }
        for (n, line) in std::fs::read_to_string(&golden)
            .unwrap()
            .lines()
            .enumerate()
        {
            if removed_word(line).is_some() {
                red.push(format!("{}:{}: {line}", golden.display(), n + 1));
            }
        }
    }
    assert!(red.is_empty(), "{}", red.join("\n"));
}

#[test]
fn editor_assets_name_no_removed_spelling() {
    let dir = root().join("editors/vscode");
    let mut seen = 0;
    for e in walk_all(&dir) {
        let Ok(body) = std::fs::read_to_string(&e) else {
            continue;
        };
        seen += 1;
        assert_eq!(removed_word(&body), None, "{}", e.display());
    }
    assert!(seen > 0, "no editor asset was read");
}

fn walk_all(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk_all(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn cli_text_names_no_removed_spelling() {
    let out = chezzi(&["help"], &root());
    assert_eq!(removed_word(&text(&out)), None);
    let dir = std::env::temp_dir().join(format!("chezzi-removed-init-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = chezzi(&["init", "proj"], &dir);
    assert!(out.status.success(), "{}", text(&out));
    let files = walk_all(&dir.join("proj"));
    assert!(!files.is_empty());
    for f in files {
        let body = std::fs::read_to_string(&f).unwrap();
        assert_eq!(removed_word(&body), None, "{}", f.display());
    }
    let _ = std::fs::remove_dir_all(&dir);
}
