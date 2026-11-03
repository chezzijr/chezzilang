//! A native binds its arguments the way a user fn does: named arguments in or out of order, an
//! omitted default filled from the native's declaration, and the same errors for an unknown name, a
//! duplicate name and a positional argument after a named one (owner ruling 2026-11-03, which lifts
//! DEC-182's "a native takes no named arguments").
//!
//! Axes: callee kind {native free fn, native method, native static, user fn} x call form {all
//! positional, named in order, named out of order, default omitted, default given by name, unknown
//! name, duplicate name, positional after named}. The expected result of each native cell is its
//! twin: the same call on a user fn with the same parameter list (the user-fn row), checked by the
//! same binary, with the twin's name replaced by the native's in the messages. The run cells read
//! what the default did (`io.buffered`'s `size` decides when a write reaches the file), for a
//! direct call and for a call through a fn value, which the callee fills from the decl.

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, cmd: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-ncb-{tag}-{}", std::process::id()));
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

/// The diagnostic messages of a `check` run, without their `(file:line:col)` location.
fn messages(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains("error (") || l.starts_with("help:"))
        .map(|l| match (l.find(" ("), l.find("): ")) {
            (Some(a), Some(b)) if a < b => format!("{}{}", &l[..a], &l[b + 1..]),
            _ => l.to_string(),
        })
        .collect()
}

/// One callee kind: the program head, the native call head, its user twin's head, and the call
/// arguments per form (`{A}` / `{B}` are the first and the defaulted parameter's values).
struct Kind {
    tag: &'static str,
    head: &'static str,
    native: &'static str,
    twin: &'static str,
    /// (first parameter name, its value, defaulted parameter name, its value).
    params: (&'static str, &'static str, &'static str, &'static str),
}

const KINDS: &[Kind] = &[
    // `io.buffered(w: Writer, size: int = 8192) -> Writer`.
    Kind {
        tag: "free",
        head: "import std.io\nfn twin(w: io.Writer, size: int = 8192) -> io.Writer:\n    return w\nw := io.stdout()\n",
        native: "io.buffered",
        twin: "twin",
        params: ("w", "w", "size", "16"),
    },
    // `Socket.read(self, n: int, timeout_ms: int? = None) -> str!`, checked only.
    Kind {
        tag: "method",
        head: "import std.net\nstruct Twin:\n    k: int\n    fn read(self, n: int, timeout_ms: int? = None) -> str!:\n        return ?\"\"\nfn go(s: net.Socket, t: Twin):\n",
        native: "    _ := s.read",
        twin: "    _ := t.read",
        params: ("n", "2", "timeout_ms", "0"),
    },
];

/// (form, the call arguments; `{a}`/`{av}` name and value the first parameter, `{d}`/`{dv}` the
/// defaulted one).
const FORMS: &[(&str, &str)] = &[
    ("positional", "{av}, {dv}"),
    ("named_in_order", "{a}={av}, {d}={dv}"),
    ("named_out_of_order", "{d}={dv}, {a}={av}"),
    ("default_omitted", "{av}"),
    ("default_by_name", "{av}, {d}={dv}"),
    ("unknown_name", "{av}, nope={dv}"),
    ("duplicate_name", "{av}, {d}={dv}, {d}={dv}"),
    ("positional_after_named", "{a}={av}, {dv}"),
];

fn args_for(form: &str, p: (&str, &str, &str, &str)) -> String {
    form.replace("{av}", p.1)
        .replace("{dv}", p.3)
        .replace("{a}", p.0)
        .replace("{d}", p.2)
}

#[test]
fn a_native_binds_like_its_user_twin() {
    let mut bad = Vec::new();
    for k in KINDS {
        for (form, shape) in FORMS {
            let args = args_for(shape, k.params);
            let native = format!("{}{}({args})\n", k.head, k.native);
            let twin = format!("{}{}({args})\n", k.head, k.twin);
            let (n_ok, n_out) = chezzi(&format!("{}-{form}-n", k.tag), "check", &native);
            let (t_ok, t_out) = chezzi(&format!("{}-{form}-t", k.tag), "check", &twin);
            let twin_name = k.twin.rsplit('.').next().unwrap_or(k.twin).trim();
            let native_name = k.native.rsplit('.').next().unwrap_or(k.native).trim();
            let want: Vec<String> = messages(&t_out)
                .iter()
                .map(|m| m.replace(&format!("'{twin_name}'"), &format!("'{native_name}'")))
                .collect();
            let got = messages(&n_out);
            if n_ok != t_ok || got != want {
                bad.push(format!(
                    "{} {form}: native ok={n_ok} {got:?}\n  twin ok={t_ok} {want:?}\n  native program:\n{native}",
                    k.tag
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{} cell(s):\n{}", bad.len(), bad.join("\n"));
}

/// A native static has no default today, so its rows are the named forms only: `bytes.zeroed(n)`.
#[test]
fn a_native_static_binds_named_arguments() {
    for (src, want) in [
        ("print(bytes.zeroed(n=2))\n", "b'\\x00\\x00'\n"),
        ("print(bytes.zeroed(2))\n", "b'\\x00\\x00'\n"),
    ] {
        let (ok, got) = chezzi("static", "run", src);
        assert!(ok && got == want, "{src}: {got}");
    }
    let (ok, got) = chezzi("static-bad", "check", "print(bytes.zeroed(m=2))\n");
    assert!(
        !ok && got.contains("unknown named argument 'm' (its parameters are: n)"),
        "{got}"
    );
}

/// `io.buffered(f, size)`: a write of 5 bytes reaches the file at once when `size` is below 5, and
/// waits for `flush` under the default 8192. A direct call fills the default from the decl at the
/// call site; a call through a fn value is filled by the callee from the same decl.
#[test]
fn a_native_default_is_the_decl_default_direct_and_through_a_value() {
    let prog = |call: &str| {
        format!(
            "import std.io\nf := io.create(\"out.txt\") ?? panic(\"create\")\ng := io.buffered\nw := {call}\n_ := w.write(\"hello\")\nprint(\"[{{io.read_file(\"out.txt\") ?? \"?\"}}]\")\n_ := w.close()\n"
        )
    };
    for (call, want) in [
        ("io.buffered(f)", "[]\n"),
        ("io.buffered(f, 4)", "[hello]\n"),
        ("io.buffered(f, size=4)", "[hello]\n"),
        ("io.buffered(size=4, w=f)", "[hello]\n"),
        ("g(f)", "[]\n"),
        ("g(f, 4)", "[hello]\n"),
        ("g(size=4, w=f)", "[hello]\n"),
    ] {
        let src = prog(call);
        let (ok, got) = chezzi("run", "run", &src);
        assert!(
            ok && got == want,
            "{call}: want {want:?}, got {got:?}\n{src}"
        );
    }
}
