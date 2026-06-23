//! `std.fs` — filesystem queries (M8), extending `std.io`'s whole-file read/write.
//!
//! `list_dir` returns entry names (not full paths), sorted for determinism. `exists`/`is_file`/
//! `is_dir` are booleans; `size` returns the byte length as a `Result[int]`. `glob` supports the
//! common `*` (any run) and `?` (single char) wildcards in the **final** path component only — no
//! `**`, no brace expansion (a focused, dependency-free matcher). All filesystem access is real
//! (like `std.io.read_file`).

use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::path::Path;

/// Serializes the golden tests that drive `examples/fs_mutations.chz`, which all write the single
/// fixed scratch dir `examples/.fs_scratch`. Held across a whole round-trip so the concurrent VM and
/// interp goldens can't interleave their mkdir/remove on the shared dir (test-runner parallelism).
#[cfg(test)]
pub(crate) static FS_SCRATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn list_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "list_dir", 1)?;
    let path = h.arg_str(0)?;
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return Ok(NativeRet::Err(format!("{path}: {e}"))),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in rd {
        match entry {
            Ok(e) => names.push(e.file_name().to_string_lossy().into_owned()),
            Err(e) => return Ok(NativeRet::Err(format!("{path}: {e}"))),
        }
    }
    names.sort();
    let items = names.into_iter().map(NativeRet::Str).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

fn exists(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "exists", 1)?;
    let path = h.arg_str(0)?;
    Ok(NativeRet::Bool(Path::new(&path).exists()))
}

fn is_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_file", 1)?;
    let path = h.arg_str(0)?;
    Ok(NativeRet::Bool(Path::new(&path).is_file()))
}

fn is_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_dir", 1)?;
    let path = h.arg_str(0)?;
    Ok(NativeRet::Bool(Path::new(&path).is_dir()))
}

fn size(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "size", 1)?;
    let path = h.arg_str(0)?;
    match std::fs::metadata(&path) {
        Ok(m) => Ok(NativeRet::Ok(Box::new(NativeRet::Int(m.len() as i64)))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

fn glob(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "glob", 1)?;
    let pattern = h.arg_str(0)?;
    // Split into the directory to scan and the wildcard for the final component.
    let (dir, pat) = match pattern.rfind('/') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => (".", pattern.as_str()),
    };
    let scan = if dir.is_empty() { "/" } else { dir };
    let rd = match std::fs::read_dir(scan) {
        Ok(rd) => rd,
        Err(e) => return Ok(NativeRet::Err(format!("{pattern}: {e}"))),
    };
    let mut hits: Vec<String> = Vec::new();
    for entry in rd {
        let name = match entry {
            Ok(e) => e.file_name().to_string_lossy().into_owned(),
            Err(e) => return Ok(NativeRet::Err(format!("{pattern}: {e}"))),
        };
        if wildcard_match(pat, &name) {
            // Re-attach the directory prefix the caller wrote, so results are usable paths.
            if pattern.contains('/') {
                hits.push(format!("{dir}/{name}"));
            } else {
                hits.push(name);
            }
        }
    }
    hits.sort();
    let items = hits.into_iter().map(NativeRet::Str).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

// --- Mutations (M8+). All return `Result[nil]`, faulting (never panicking) on an I/O error,
// mirroring `std.io.write_file`'s `Ok(NativeRet::Ok(Nil))` / `Ok(NativeRet::Err("{path}: {e}"))`
// idiom so a permission-denied / missing-parent failure is a catchable Chezzi error.

/// Create a directory, recursively (like `mkdir -p`): missing parents are created and an existing
/// directory is a no-op (idempotent). Faults only on a real error (e.g. a parent component is a file).
fn mkdir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "mkdir", 1)?;
    let path = h.arg_str(0)?;
    match std::fs::create_dir_all(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Delete a single file. Faults if the path is missing or is a directory (use `remove_dir` for dirs).
fn remove_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "remove_file", 1)?;
    let path = h.arg_str(0)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Delete an EMPTY directory (non-recursive — faults on a non-empty dir, avoiding a silent `rm -rf`).
fn remove_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "remove_dir", 1)?;
    let path = h.arg_str(0)?;
    match std::fs::remove_dir(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Move/rename a path. Faults if the source is missing (or a cross-device move is unsupported).
fn rename(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "rename", 2)?;
    let from = h.arg_str(0)?;
    let to = h.arg_str(1)?;
    match std::fs::rename(&from, &to) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{from} -> {to}: {e}"))),
    }
}

/// Copy a file's contents (file-only). Faults if the source is missing. The byte count is dropped
/// for parity-simplicity with `write_file`'s `Result[nil]` shape.
fn copy(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "copy", 2)?;
    let from = h.arg_str(0)?;
    let to = h.arg_str(1)?;
    match std::fs::copy(&from, &to) {
        Ok(_) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{from} -> {to}: {e}"))),
    }
}

/// Append a string to a file, creating it if absent (never truncates — complements
/// `std.io.write_file`'s overwrite). Faults on a real I/O error.
fn append(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    use std::io::Write;
    expect_args(h, "append", 2)?;
    let path = h.arg_str(0)?;
    let contents = h.arg_str(1)?;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(contents.as_bytes()));
    match result {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Match a single path component against a `*`/`?` wildcard. `*` matches any run (including empty),
/// `?` matches exactly one character; every other character is literal. Uses the classic greedy
/// two-pointer algorithm with a single backtrack mark — linear-ish, no exponential blowup on
/// adversarial patterns like `*a*a*a…b`.
fn wildcard_match(pat: &str, name: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: the last `*` swallows one more character.
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("list_dir", list_dir),
    ("exists", exists),
    ("is_file", is_file),
    ("is_dir", is_dir),
    ("size", size),
    ("glob", glob),
    ("mkdir", mkdir),
    ("remove_file", remove_file),
    ("remove_dir", remove_dir),
    ("rename", rename),
    ("copy", copy),
    ("append", append),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A minimal str-only `Host` for exercising the fs mutation natives in isolation.
    #[derive(Default)]
    struct StrHost {
        strs: Vec<String>,
    }

    impl Host for StrHost {
        fn arg_count(&self) -> usize {
            self.strs.len()
        }
        fn arg_int(&mut self, _i: usize) -> Result<i64, HostError> {
            Err(HostError {
                message: "no int args".into(),
            })
        }
        fn arg_float(&mut self, _i: usize) -> Result<f64, HostError> {
            Err(HostError {
                message: "no float args".into(),
            })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
        }
        fn arg_str(&mut self, i: usize) -> Result<String, HostError> {
            self.strs.get(i).cloned().ok_or(HostError {
                message: "missing arg".into(),
            })
        }
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError {
                message: "no map args".into(),
            })
        }
        fn write_stdout(&mut self, _s: &str) {}
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(None)
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<String, HostError> {
            Ok("/".into())
        }
    }

    fn host(strs: &[&str]) -> StrHost {
        StrHost {
            strs: strs.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A unique temp subdir per call (mirror the VM `TmpDir` counter pattern), auto-removed on drop.
    struct TmpDir(std::path::PathBuf);
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    impl TmpDir {
        fn new() -> Self {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let mut p = std::env::temp_dir();
            p.push(format!("chezzi_fs_test_{}_{n}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn join(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn is_ok(r: NativeRet) -> bool {
        matches!(r, NativeRet::Ok(_))
    }
    fn is_err(r: NativeRet) -> bool {
        matches!(r, NativeRet::Err(_))
    }

    #[test]
    fn fs_mutations_roundtrip() {
        let tmp = TmpDir::new();
        // mkdir (recursive: creates nested parents).
        let nested = tmp.join("a/b/c");
        assert!(is_ok(mkdir(&mut host(&[&nested])).unwrap()));
        assert!(Path::new(&nested).is_dir());
        // mkdir is idempotent on an existing dir.
        assert!(is_ok(mkdir(&mut host(&[&nested])).unwrap()));

        // append creates the file if absent, then appends (never truncates).
        let f = tmp.join("a/b/c/log.txt");
        assert!(is_ok(append(&mut host(&[&f, "hello"])).unwrap()));
        assert!(is_ok(append(&mut host(&[&f, " world"])).unwrap()));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello world");

        // rename moves the file.
        let renamed = tmp.join("a/b/c/moved.txt");
        assert!(is_ok(rename(&mut host(&[&f, &renamed])).unwrap()));
        assert!(!Path::new(&f).exists());
        assert!(Path::new(&renamed).exists());

        // copy duplicates the file contents.
        let copied = tmp.join("a/b/c/copy.txt");
        assert!(is_ok(copy(&mut host(&[&renamed, &copied])).unwrap()));
        assert_eq!(std::fs::read_to_string(&copied).unwrap(), "hello world");
        assert!(Path::new(&renamed).exists()); // source still there

        // remove_file deletes a file.
        assert!(is_ok(remove_file(&mut host(&[&renamed])).unwrap()));
        assert!(is_ok(remove_file(&mut host(&[&copied])).unwrap()));
        assert!(!Path::new(&renamed).exists());

        // remove_dir deletes an empty dir.
        assert!(is_ok(remove_dir(&mut host(&[&nested])).unwrap()));
        assert!(!Path::new(&nested).exists());
    }

    #[test]
    fn fs_mutation_errors_are_recoverable() {
        let tmp = TmpDir::new();
        // remove_file on a missing path → Err (not panic, not Ok).
        let missing = tmp.join("nope.txt");
        assert!(is_err(remove_file(&mut host(&[&missing])).unwrap()));
        // remove_dir on a non-empty dir → Err (non-recursive).
        let dir = tmp.join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(tmp.join("d/x"), "x").unwrap();
        assert!(is_err(remove_dir(&mut host(&[&dir])).unwrap()));
        // rename / copy from a missing source → Err.
        let dst = tmp.join("dst.txt");
        assert!(is_err(rename(&mut host(&[&missing, &dst])).unwrap()));
        assert!(is_err(copy(&mut host(&[&missing, &dst])).unwrap()));
        // mkdir where a parent component is a FILE → Err.
        let file = tmp.join("file");
        std::fs::write(&file, "x").unwrap();
        let under_file = tmp.join("file/sub");
        assert!(is_err(mkdir(&mut host(&[&under_file])).unwrap()));
    }

    #[test]
    fn wildcard_star_and_question() {
        assert!(wildcard_match("*.txt", "a.txt"));
        assert!(wildcard_match("*.txt", ".txt"));
        assert!(!wildcard_match("*.txt", "a.md"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("foo*bar", "fooXYZbar"));
        assert!(!wildcard_match("foo*bar", "fooXYZbaz"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exacted"));
    }

    /// Regression (review): a multi-star pattern against a long non-matching name must not blow up
    /// exponentially. The greedy two-pointer matcher returns near-instantly.
    #[test]
    fn wildcard_no_catastrophic_backtracking() {
        let pat = "*a*a*a*a*a*a*a*a*a*a*b";
        let name = "a".repeat(64); // no 'b' → never matches
        assert!(!wildcard_match(pat, &name));
        assert!(wildcard_match("*a*a*b", "aaaaaaab"));
    }
}
