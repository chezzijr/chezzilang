//! Shared harness for the generated-program grids: one program per cell, run through the built
//! `chezzi` binary, judged by its stdout or by a fragment of its rejection. Include it with
//! `#[path = "support/grid_cell.rs"] mod grid_cell;`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub enum Expect {
    Prints(String),
    /// A program the language rejects by a named rule; the fragment is part of the message.
    Rejects(&'static str),
}

pub struct Cell {
    pub name: String,
    pub files: Vec<(String, String)>,
    pub expect: Expect,
}

pub fn run_cell(root: &Path, idx: usize, c: &Cell) -> Result<(), String> {
    let dir: PathBuf = root.join(format!("c{idx}"));
    for (rel, src) in &c.files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, src).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("main.chz")
        .current_dir(&dir)
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ok = match &c.expect {
        Expect::Prints(want) => out.status.success() && stdout.trim_end() == want,
        Expect::Rejects(frag) => !out.status.success() && stderr.contains(frag),
    };
    if ok {
        return Ok(());
    }
    let want = match &c.expect {
        Expect::Prints(w) => format!("prints {w:?}"),
        Expect::Rejects(f) => format!("rejects {f:?}"),
    };
    let first_err = stderr.lines().next().unwrap_or("");
    Err(format!(
        "{}: want {want}; got stdout {:?} stderr {first_err:?}",
        c.name,
        stdout.trim_end()
    ))
}

/// Run every cell under a fresh temp dir and fail with the list of red cells.
pub fn run_grid(tag: &str, cells: &[Cell]) {
    let root = std::env::temp_dir().join(format!("chezzi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut fails = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if let Err(e) = run_cell(&root, i, c) {
            fails.push(e);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        fails.is_empty(),
        "{} of {} {tag} cells failed:\n{}",
        fails.len(),
        cells.len(),
        fails.join("\n")
    );
}
