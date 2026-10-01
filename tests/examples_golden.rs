//! Every `examples/**/*.chz` that has a sibling `.expected` must produce it (2026-10-01).
//!
//! Before this test only hand-picked examples had a golden, so the other `.expected` files could
//! drift unseen: `examples/executor_results.chz` crashed with `empty row` for weeks after TICKET-045
//! changed `Task.done()`. The set of examples is discovered from disk, never listed here.
//!
//! Each example runs on the built `chezzi` binary from the repo root (some examples read
//! repo-relative fixtures), with stdin closed. The comparison mode is declared by the example itself
//! in a header line, so the setting lives next to the program it describes:
//! - `# golden: unordered` — compare the sorted line sets (concurrent prints whose order is not
//!   causally forced);
//! - `# golden: stderr` — compare stderr instead of stdout;
//! - no header — stdout must match byte for byte.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const LIMIT: Duration = Duration::from_secs(30);

fn goldens(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read examples dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            goldens(&path, out);
        } else if path.extension().is_some_and(|e| e == "expected") {
            let src = path.with_extension("chz");
            if src.is_file() {
                out.push(src);
            }
        }
    }
}

fn mode(src: &str) -> &'static str {
    let header = src.lines().take_while(|l| l.starts_with('#'));
    for line in header {
        if line.starts_with("# golden: unordered") {
            return "unordered";
        }
        if line.starts_with("# golden: stderr") {
            return "stderr";
        }
    }
    "exact"
}

fn run(root: &Path, src: &Path) -> Result<(String, String), String> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(src)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let start = Instant::now();
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            return Err(format!("no exit within {LIMIT:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn sorted(s: &str) -> Vec<&str> {
    let mut v: Vec<&str> = s.lines().collect();
    v.sort_unstable();
    v
}

#[test]
fn every_example_with_an_expected_file_produces_it() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut srcs = Vec::new();
    goldens(&root.join("examples"), &mut srcs);
    srcs.sort();
    assert!(
        srcs.len() > 100,
        "found only {} examples with a .expected",
        srcs.len()
    );

    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(src) = srcs.get(i) else { break };
                    let rel = src.strip_prefix(&root).unwrap_or(src).display().to_string();
                    let text = std::fs::read_to_string(src).expect("read example");
                    let want = std::fs::read_to_string(src.with_extension("expected"))
                        .expect("read .expected");
                    let miss = match run(&root, src) {
                        Err(e) => Some(e),
                        Ok((out, err)) => match mode(&text) {
                            "unordered" if sorted(&out) == sorted(&want) => None,
                            "stderr" if err == want => None,
                            "exact" if out == want => None,
                            m => Some(format!(
                                "{m} mismatch\n--- got stdout ---\n{out}--- got stderr ---\n{err}"
                            )),
                        },
                    };
                    if let Some(m) = miss {
                        misses.lock().unwrap().push(format!("{rel}: {m}"));
                    }
                }
            });
        }
    });
    let misses = misses.into_inner().unwrap();
    assert!(
        misses.is_empty(),
        "{} of {} examples differ from their .expected:\n{}",
        misses.len(),
        srcs.len(),
        misses.join("\n\n")
    );
}
