//! A std module member is an ordinary module member: every member below x every way a program can
//! name it x every position it is read in, one generated program per cell, run through the built
//! `chezzi` binary. The expected outcome of a cell is not written here: it is the outcome of the same
//! cell for a member of a USER module (`lib.chz`) of the same kind. So the rule "a std module
//! imports like any module" is derived, and a name the checker or the VM handles by its spelling
//! (an import license, a rename veto, a bare name after a whole-module import) shows up as a red cell.

use std::path::Path;
use std::process::Command;

/// The user module every reference cell imports. One member per kind a std member can have.
const LIB: &str = "struct T:\n    x: int\nstruct G[A]:\n    x: A\nfn f(n: int) -> int:\n    return n\nfn z() -> int:\n    return 0\n";

enum Kind {
    /// A function, called with `args`.
    Fn { args: &'static str },
    /// A type, written with `targs` in an annotation; `ctor` is a call that builds one (`{N}` is the
    /// name as the cell spells it), when the type has a constructor.
    Type {
        targs: &'static str,
        ctor: Option<&'static str>,
    },
}

struct Member {
    module: &'static str,
    name: &'static str,
    kind: Kind,
}

const fn ty(module: &'static str, name: &'static str, targs: &'static str) -> Member {
    Member {
        module,
        name,
        kind: Kind::Type { targs, ctor: None },
    }
}

const fn ctor(
    module: &'static str,
    name: &'static str,
    targs: &'static str,
    ctor: &'static str,
) -> Member {
    Member {
        module,
        name,
        kind: Kind::Type {
            targs,
            ctor: Some(ctor),
        },
    }
}

const fn func(module: &'static str, name: &'static str, args: &'static str) -> Member {
    Member {
        module,
        name,
        kind: Kind::Fn { args },
    }
}

const MEMBERS: &[Member] = &[
    func("std.time", "timer", "1"),
    func("std.time", "now", ""),
    func("std.math", "sqrt", "4.0"),
    ctor("std.concurrency", "Shared", "[int]", "{N}(1)"),
    ctor("std.concurrency", "RwShared", "[int]", "{N}(1)"),
    ctor("std.concurrency", "Atomic", "[int]", "{N}(1)"),
    ctor("std.concurrency", "AtomicInt", "", "{N}(1)"),
    ctor("std.concurrency", "Executor", "", "{N}()"),
    ty("std.net", "Socket", ""),
    ty("std.net", "Listener", ""),
    ty("std.io", "Reader", ""),
    ty("std.io", "Writer", ""),
    ty("std.ffi", "ptr", ""),
    ty("std.ffi", "int32", ""),
    ty("std.ffi", "float32", ""),
    ty("std.regex", "Match", ""),
    ty("std.fs", "FileInfo", ""),
    ty("std.cancel", "Token", ""),
    ty("std.collections", "Deque", "[int]"),
];

/// How the program puts the member in scope: the import lines, and the spelling a use writes.
struct Form {
    tag: &'static str,
    import: fn(module: &str, name: &str) -> String,
    spell: fn(module: &str, name: &str) -> String,
}

fn last(module: &str) -> &str {
    module.rsplit('.').next().unwrap()
}

const FORMS: &[Form] = &[
    Form {
        tag: "no import",
        import: |_, _| String::new(),
        spell: |_, n| n.to_string(),
    },
    Form {
        tag: "module import, bare",
        import: |m, _| format!("import {m}\n"),
        spell: |_, n| n.to_string(),
    },
    Form {
        tag: "module import, qualified",
        import: |m, _| format!("import {m}\n"),
        spell: |m, n| format!("{}.{n}", last(m)),
    },
    Form {
        tag: "from-import",
        import: |m, n| format!("import {n} from {m}\n"),
        spell: |_, n| n.to_string(),
    },
    Form {
        tag: "from-import with alias",
        import: |m, n| format!("import {n} as Zq from {m}\n"),
        spell: |_, _| "Zq".to_string(),
    },
];

/// The positions a member of `kind` is read in: (tag, program body), the body spelling it `p`. The
/// `declaration` position declares a struct of the bare spelling: legal unless an import binds it.
fn uses(kind: &Kind, p: &str) -> Vec<(&'static str, String)> {
    let mut out = vec![("value", format!("v := {p}\nprint(1)\n"))];
    if !p.contains('.') {
        out.push((
            "declaration",
            format!("struct {p}:\n    x: int\nprint({p}(x=1).x)\n"),
        ));
    }
    match kind {
        Kind::Fn { args } => out.push(("call", format!("v := {p}({args})\nprint(1)\n"))),
        Kind::Type { targs, ctor } => {
            out.push((
                "annotation",
                format!("struct C:\n    v: {p}{targs}\nprint(1)\n"),
            ));
            if let Some(c) = ctor {
                out.push(("ctor", format!("v := {}\nprint(1)\n", c.replace("{N}", p))));
            }
        }
    }
    out
}

/// The user-module member of the same kind as `m`, and the constructor call it answers with.
fn reference(m: &Member) -> Member {
    match m.kind {
        Kind::Fn { args: "" } => func("lib", "z", ""),
        Kind::Fn { .. } => func("lib", "f", "1"),
        Kind::Type { targs, ctor } => {
            let generic = !targs.is_empty();
            Member {
                module: "lib",
                name: if generic { "G" } else { "T" },
                kind: Kind::Type {
                    targs,
                    ctor: ctor.map(|_| if generic { "{N}[int](x=1)" } else { "{N}(x=1)" }),
                },
            }
        }
    }
}

/// Runs one program; the outcome is (exit success, stdout or the first error line). Two outcomes
/// match when both fail, or both succeed with the same stdout.
fn run(dir: &Path, src: &str) -> (bool, String) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("lib.chz"), LIB).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("main.chz")
        .current_dir(dir)
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let first_err = String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    (
        out.status.success(),
        if out.status.success() {
            stdout
        } else {
            first_err
        },
    )
}

/// Both runs failed (the error names the member, so its text differs), or both printed the same.
fn same(got: &(bool, String), want: &(bool, String)) -> bool {
    got.0 == want.0 && (!got.0 || got.1 == want.1)
}

/// The program of one cell for member `m` in `form`, at use position `tag`.
fn program(m: &Member, form: &Form, tag: &str) -> Option<String> {
    let p = (form.spell)(m.module, m.name);
    let (_, body) = uses(&m.kind, &p).into_iter().find(|(t, _)| *t == tag)?;
    Some(format!("{}{body}", (form.import)(m.module, m.name)))
}

#[test]
fn a_std_member_imports_like_a_user_module_member() {
    let root = std::env::temp_dir().join(format!("chz-module-member-grid-{}", std::process::id()));
    let mut failures = Vec::new();
    let mut cells = 0;
    for (mi, m) in MEMBERS.iter().enumerate() {
        let r = reference(m);
        for (fi, form) in FORMS.iter().enumerate() {
            for (tag, _) in uses(&m.kind, m.name) {
                let (Some(src), Some(ref_src)) = (program(m, form, tag), program(&r, form, tag))
                else {
                    continue;
                };
                cells += 1;
                let got = run(&root.join(format!("m{mi}f{fi}{tag}")), &src);
                let want = run(&root.join(format!("r{mi}f{fi}{tag}")), &ref_src);
                if !same(&got, &want) {
                    failures.push(format!(
                        "{}.{} / {} / {tag}: {} but the user-module member {}: {:?}\n  program: {src:?}",
                        m.module,
                        m.name,
                        form.tag,
                        if got.0 { "accepted" } else { "rejected" },
                        if want.0 { "is accepted" } else { "is rejected" },
                        got.1
                    ));
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(cells > 150, "the grid shrank to {cells} cells");
    assert!(
        failures.is_empty(),
        "{} of {cells} cells differ from the user-module reference:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// An import binds before a builtin of its name, for a std member as for a user-module member: a
/// type imported as `int` is that type in an annotation, and a constructor imported as `print` is
/// that constructor.
#[test]
fn an_imported_std_member_shadows_a_builtin_of_its_name() {
    let root =
        std::env::temp_dir().join(format!("chz-module-member-shadow-{}", std::process::id()));
    let mut failures = Vec::new();
    for (mi, m) in MEMBERS.iter().enumerate() {
        let Kind::Type { targs, ctor } = m.kind else {
            continue;
        };
        let r = reference(m);
        let annotate = |module: &str, name: &str| {
            format!(
                "import {name} as int from {module}\nfn f(x: int{targs}) -> None:\n    pass\nf(3)\n"
            )
        };
        let mut cells = vec![(
            "annotation",
            annotate(m.module, m.name),
            annotate(r.module, r.name),
        )];
        if let (Some(c), Kind::Type { ctor: Some(rc), .. }) = (ctor, &r.kind) {
            let call = |module: &str, name: &str, c: &str| {
                format!(
                    "import {name} as print from {module}\nv := {}\n",
                    c.replace("{N}", "print")
                )
            };
            cells.push((
                "ctor",
                call(m.module, m.name, c),
                call(r.module, r.name, rc),
            ));
        }
        for (tag, src, ref_src) in cells {
            let got = run(&root.join(format!("m{mi}{tag}")), &src);
            let want = run(&root.join(format!("r{mi}{tag}")), &ref_src);
            if !same(&got, &want) {
                failures.push(format!(
                    "{}.{} / {tag}: {:?} but the user-module member {:?}\n  program: {src:?}",
                    m.module, m.name, got, want
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
