//! Shared harness for the generated-program grids: one program per cell, run through the built
//! `chezzi` binary, judged by its stdout, by a fragment of its rejection, or by its twin program.
//! Include it with `#[path = "support/grid_cell.rs"] mod grid_cell;`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Each std protocol a cell program names, with the std module that owns it (TICKET-265: a
/// bound `[T: Add]` or a slot `Iterator[int]` needs the import).
const STD_PROTOCOLS: [(&str, &[&str]); 5] = [
    (
        "std.ops",
        &[
            "Add",
            "Sub",
            "Mul",
            "Div",
            "Mod",
            "Neg",
            "BitAnd",
            "BitOr",
            "BitXor",
            "Shl",
            "Shr",
            "Arithmetic",
            "Num",
            "Index",
            "IndexSet",
            "Slice",
            "Contains",
        ],
    ),
    ("std.cmp", &["Eq", "Comparable", "Hashable"]),
    ("std.iter", &["Iterator", "Iterable"]),
    ("std.string", &["Stringable"]),
    ("std.path", &["PathLike"]),
];

fn names_word(src: &str, word: &str) -> bool {
    src.match_indices(word).any(|(i, _)| {
        let before = src[..i].chars().next_back();
        let after = src[i + word.len()..].chars().next();
        let is_word =
            |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        !is_word(before) && !is_word(after)
    })
}

/// `src` with an `import` line for every std protocol it names and does not import or declare.
pub fn with_std_imports(src: &str) -> String {
    let mut head = String::new();
    for (module, names) in STD_PROTOCOLS {
        let needed: Vec<&str> = names
            .iter()
            .copied()
            .filter(|n| names_word(src, n))
            .filter(|n| {
                !src.contains(&format!("protocol {n}"))
                    && !src.contains(&format!("struct {n}"))
                    && !src.contains(&format!("enum {n}"))
            })
            .filter(|n| {
                !src.lines()
                    .any(|l| l.starts_with("import ") && names_word(l, n))
            })
            .collect();
        if !needed.is_empty() {
            head.push_str(&format!("import {} from {module}\n", needed.join(", ")));
        }
    }
    format!("{head}{src}")
}

#[derive(Clone)]
pub enum Expect {
    Prints(String),
    /// A program the language rejects by a named rule; the fragment is part of the message.
    Rejects(&'static str),
    /// Behaves as the twin program does, run at test time (TICKET-264).
    Twin(Twin),
}

/// The twin of a cell: the program it must agree with on the exit status, the stdout and the first
/// error line. The error lines compare with the location stripped, `written` (the twin's callee
/// name) replaced by `path_name` (the cell's), and every arity line read as one rejection.
#[derive(Clone)]
pub struct Twin {
    pub files: Vec<(String, String)>,
    pub written: String,
    pub path_name: String,
}

pub struct Cell {
    pub name: String,
    pub files: Vec<(String, String)>,
    pub expect: Expect,
}

/// One program's outcome.
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    /// The first stderr line, its `(file:line:col)` location stripped.
    pub first_err: String,
    pub stderr: String,
}

/// Write `files` under `dir` and run `chezzi run main.chz` there.
pub fn run_program(dir: &Path, files: &[(String, String)]) -> Out {
    for (rel, src) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let src = if rel.ends_with(".chz") {
            with_std_imports(src)
        } else {
            src.clone()
        };
        std::fs::write(&path, src).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("main.chz")
        .current_dir(dir)
        .output()
        .expect("spawn chezzi");
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        first_err: strip_loc(stderr.lines().next().unwrap_or("")),
        stderr,
    }
}

/// The verdict on one cell, given its outcome and, for a [`Expect::Twin`], the twin's.
fn judge(c: &Cell, got: &Out, twin: Option<&Out>) -> Result<(), String> {
    let (ok, want) = match &c.expect {
        Expect::Prints(w) => (got.ok && got.stdout == *w, format!("prints {w:?}")),
        Expect::Rejects(f) => (!got.ok && got.stderr.contains(f), format!("rejects {f:?}")),
        Expect::Twin(t) => {
            let tw = twin.expect("a twin cell is judged with its twin's outcome");
            let want_err = tw
                .first_err
                .replace(&format!("'{}'", t.written), &format!("'{}'", t.path_name));
            let want_err = canon(&want_err, &t.path_name);
            let ok = tw.ok == got.ok
                && tw.stdout == got.stdout
                && want_err == canon(&got.first_err, &t.path_name);
            let want = format!(
                "the twin's ok={} stdout {:?} err {want_err:?}",
                tw.ok, tw.stdout
            );
            (ok, want)
        }
    };
    if ok {
        return Ok(());
    }
    Err(format!(
        "{}: want {want}; got ok={} stdout {:?} stderr {:?}",
        c.name, got.ok, got.stdout, got.first_err
    ))
}

/// Run one cell (and its twin, if it has one) under `root`.
pub fn run_cell(root: &Path, idx: usize, c: &Cell) -> Result<(), String> {
    let got = run_program(&root.join(format!("c{idx}")), &c.files);
    let twin = match &c.expect {
        Expect::Twin(t) => Some(run_program(&root.join(format!("c{idx}-twin")), &t.files)),
        Expect::Prints(_) | Expect::Rejects(_) => None,
    };
    judge(c, &got, twin.as_ref())
}

/// An arity error reads one of three ways: `missing required argument 'a'` (a call plan miss, no
/// callee name), `'k' expects 2 argument(s), got 0` and `'k' expects 1–2 argument(s), got 0` (an
/// annotated fn type carries no defaults). Every form is one arity rejection; the callee name is
/// checked wherever the message carries one. Any other line compares as written.
fn canon(line: &str, path_name: &str) -> String {
    if line.contains(" expects ") && line.contains(" argument(s), got ") {
        let named = format!("'{path_name}' expects ");
        if !line.contains(&named) {
            return format!("ARITY with the wrong callee name (want {path_name:?}): {line}");
        }
        return "ARITY".to_string();
    }
    if line.contains("missing required argument") {
        return "ARITY".to_string();
    }
    line.to_string()
}

/// Drop a `(file:line:col)` location from an error line.
fn strip_loc(line: &str) -> String {
    let mut s = line.to_string();
    while let Some(open) = s.find(" (") {
        let Some(close) = s[open..].find(')') else {
            break;
        };
        let inner = &s[open + 2..open + close];
        let tail: Vec<&str> = inner.rsplitn(3, ':').collect();
        if tail.len() == 3
            && tail[0].chars().all(|c| c.is_ascii_digit())
            && tail[1].chars().all(|c| c.is_ascii_digit())
        {
            s.replace_range(open..open + close + 1, "");
        } else {
            break;
        }
    }
    s
}

/// Run `programs` on four workers, `dir(i)` naming program `i`'s directory.
fn run_all(
    programs: &[&[(String, String)]],
    dir: &(dyn Fn(usize) -> std::path::PathBuf + Sync),
) -> Vec<Out> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Out)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= programs.len() {
                        break;
                    }
                    let o = run_program(&dir(i), programs[i]);
                    results.lock().unwrap().push((i, o));
                }
            });
        }
    });
    let mut r = results.into_inner().unwrap();
    r.sort_by_key(|(i, _)| *i);
    r.into_iter().map(|(_, o)| o).collect()
}

/// The names and reasons of the red cells. A twin shared by several cells runs once.
fn grid_fails(tag: &str, cells: &[Cell]) -> Vec<(String, String)> {
    let root = std::env::temp_dir().join(format!("chezzi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let key = |files: &[(String, String)]| {
        files
            .iter()
            .map(|(n, s)| format!("{n}\n{s}"))
            .collect::<Vec<_>>()
            .join("\n--\n")
    };
    let mut twin_ix: HashMap<String, usize> = HashMap::new();
    let mut twins: Vec<&[(String, String)]> = Vec::new();
    for c in cells {
        if let Expect::Twin(t) = &c.expect {
            twin_ix.entry(key(&t.files)).or_insert_with(|| {
                twins.push(&t.files);
                twins.len() - 1
            });
        }
    }
    let twin_out = run_all(&twins, &|i| root.join(format!("t{i}")));
    let programs: Vec<&[(String, String)]> = cells.iter().map(|c| c.files.as_slice()).collect();
    let outs = run_all(&programs, &|i| root.join(format!("c{i}")));
    let _ = std::fs::remove_dir_all(&root);
    let mut fails = Vec::new();
    for (c, got) in cells.iter().zip(&outs) {
        let twin = match &c.expect {
            Expect::Twin(t) => Some(&twin_out[twin_ix[&key(&t.files)]]),
            Expect::Prints(_) | Expect::Rejects(_) => None,
        };
        if let Err(e) = judge(c, got, twin) {
            fails.push((c.name.clone(), e));
        }
    }
    fails
}

/// Run every cell under a fresh temp dir and fail with the list of red cells. A cell named in
/// `pinned` must stay red; when one turns green, the test fails until it leaves the list.
/// `GRID_DUMP=1` prints every red cell, pinned or not.
pub fn run_grid(tag: &str, cells: &[Cell], pinned: &[&str]) {
    let fails = grid_fails(tag, cells);
    if std::env::var("GRID_DUMP").is_ok() {
        for (_, why) in &fails {
            eprintln!("RED {why}");
        }
    }
    let mut bad: Vec<String> = fails
        .iter()
        .filter(|(n, _)| !pinned.contains(&n.as_str()))
        .map(|(_, why)| why.clone())
        .collect();
    for p in pinned {
        if !fails.iter().any(|(n, _)| n == p) {
            bad.push(format!("{p}: green; remove it from the pinned list"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} {tag} cells failed:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
}
