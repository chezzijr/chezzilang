//! TICKET-247 (wave 23, Family "Airlock"): `?x` / `!x` hold the object bound to `x`, so a generator
//! that yields a wrapped frame-local and then writes it in a task must fault or fail to compile,
//! exactly as `yield x` does. Grid: leak form x root type; each cell is its own process at
//! `CHEZZI_THREADS` 1, 2 and default. A cell passes when the write does not run silently.

use std::process::{Command, Stdio};

const THREADS: [Option<&str>; 3] = [Some("1"), Some("2"), None];

fn run(src: &str, threads: Option<&str>) -> (i32, String, String) {
    let dir = std::env::temp_dir().join(format!("chz-t247-{}-{}", std::process::id(), src.len()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("main{}.chz", threads.unwrap_or("d")));
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path).stdin(Stdio::null());
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let out = cmd.output().expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `(root type, init, write)`.
const ROOTS: [(&str, &str, &str); 2] = [
    ("List[int]", "[0]", "buf.push(1)"),
    ("Map[int, int]", "{0: 0}", "buf[1] = 1"),
];

/// `(leak form, yielded type with `T` standing for the root type)`.
const FORMS: [(&str, &str); 4] = [
    ("buf", "T"),
    ("?buf", "T?"),
    ("[?buf]", "List[T?]"),
    ("(?buf, 1)", "tuple[T?, int]"),
];

#[test]
fn wrapped_name_yield_faults_like_the_bare_name() {
    let mut silent = Vec::new();
    for (ty, init, write) in ROOTS {
        for (form, yt) in FORMS {
            let yt = yt.replace('T', ty);
            let src = format!(
                "fn gen() -> Iterator[{yt}]:\n    buf := {init}\n    yield {form}\n    {write}\n    print(\"task wrote\")\n    yield {form}\n\nfn resume(it: Iterator[{yt}]):\n    _ := it.next()\n\nfn main():\n    it := gen()\n    seen := it.next()\n    parallel:\n        spawn resume(it)\n    print(\"parent sees\", seen)\nmain()\n"
            );
            for t in THREADS {
                let (code, out, err) = run(&src, t);
                if code == 0 && out.contains("task wrote") {
                    silent.push(format!(
                        "{ty} / yield {form} / threads {t:?}: {out:?} {err:?}"
                    ));
                }
            }
        }
    }
    assert!(
        silent.is_empty(),
        "silent lost write, no fault and no compile error:\n{}",
        silent.join("\n")
    );
}
