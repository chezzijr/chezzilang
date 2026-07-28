//! `std.fs` — filesystem queries + mutations (M8), extending `std.io`'s whole-file read/write.
//!
//! `list_dir` returns entry names (not full paths), sorted for determinism. `exists`/`is_file`/
//! `is_dir` are booleans; `size` returns the byte length as a `Result[int]`. `glob` supports the
//! common `*` (any run) and `?` (single char) wildcards in the **final** path component only — no
//! `**`, no brace expansion (a focused, dependency-free matcher).
//!
//! Mutations (each `Result[nil]`, recoverable fault on error): `mkdir` (recursive, idempotent),
//! `remove_file`, `remove_dir` (empty-only — no recursive `rm -rf`), `rename`, `copy` (file), and
//! `append` (create-or-append, never truncates). All filesystem access is real (like
//! `std.io.read_file`).

use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic sequence for `atomic_write`'s per-write temp filename (with the pid) — keeps concurrent
/// atomic writes to the same directory from colliding on the temp name.
static ATOMIC_SEQ: AtomicU64 = AtomicU64::new(0);

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

/// Read filesystem metadata into a `FileInfo` struct. FOLLOWS symlinks for size/mtime/mode/is_dir/
/// is_file (matches `stat` / Python `os.stat`); `is_symlink` comes from a separate
/// `symlink_metadata` (best-effort `false` if that errors). A missing/unreadable path (or a broken
/// symlink, since the follow-`metadata` errors) returns a recoverable `Err`, never a fault. `mtime`
/// is UNIX-epoch seconds, `0` if the file's mtime predates the epoch or the platform can't report it.
/// `mode` is the raw unix `st_mode` (perm + type bits) on unix, `0` elsewhere.
fn stat(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "stat", 1)?;
    let path = h.arg_str(0)?;
    let m = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Ok(NativeRet::Err(format!("{path}: {e}"))),
    };
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mode: i64 = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            m.mode() as i64
        }
        #[cfg(not(unix))]
        {
            0
        }
    };
    let is_symlink = std::fs::symlink_metadata(&path)
        .map(|sm| sm.file_type().is_symlink())
        .unwrap_or(false);
    Ok(NativeRet::Ok(Box::new(NativeRet::Struct {
        name: "FileInfo".into(),
        fields: vec![
            ("size".into(), NativeRet::Int(m.len() as i64)),
            ("mtime".into(), NativeRet::Int(mtime)),
            ("mode".into(), NativeRet::Int(mode)),
            ("is_dir".into(), NativeRet::Bool(m.is_dir())),
            ("is_file".into(), NativeRet::Bool(m.is_file())),
            ("is_symlink".into(), NativeRet::Bool(is_symlink)),
        ],
    })))
}

/// Recursively list every entry (files + dirs) strictly under `path`, as a flat list of full path
/// strings. Each directory's entries are SORTED by name before pushing/recursing — this makes the
/// order deterministic (`read_dir` yields filesystem-arbitrary order), which is REQUIRED for
/// serial==M:N parity. Pre-order: a directory is listed before its children. A symlinked directory is
/// LISTED but NOT descended (cycle guard). An unreadable root (or subdir) returns a recoverable `Err`.
fn walk(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "walk", 1)?;
    let root = h.arg_str(0)?;
    let mut out: Vec<String> = Vec::new();
    if let Err(e) = walk_into(Path::new(&root), &mut out) {
        return Ok(NativeRet::Err(format!("{root}: {e}")));
    }
    let items = out.into_iter().map(NativeRet::Str).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

/// Recursion helper for [`walk`]: sort each directory's entries by file name, then push each entry's
/// full path and recurse into it only if it is a real (non-symlink) directory.
fn walk_into(dir: &Path, out: &mut Vec<String>) -> std::io::Result<()> {
    let mut entries: Vec<std::fs::DirEntry> =
        std::fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let ft = e.file_type()?; // does NOT follow symlinks — is_symlink is accurate
        let p = e.path();
        out.push(p.to_string_lossy().into_owned());
        if ft.is_dir() && !ft.is_symlink() {
            walk_into(&p, out)?;
        }
    }
    Ok(())
}

/// Resolve symlinks and `.`/`..` to an absolute, real path via `std::fs::canonicalize`. Unlike the
/// purely lexical `path.normalize` (no filesystem access), this hits the real filesystem and so
/// REQUIRES the path to exist — a nonexistent path faults (recoverable `Result[str]` error).
fn canonicalize(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "canonicalize", 1)?;
    let path = h.arg_str(0)?;
    match std::fs::canonicalize(&path) {
        Ok(p) => Ok(NativeRet::Ok(Box::new(NativeRet::Str(
            p.to_string_lossy().into_owned(),
        )))),
        Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
    }
}

/// Set a file's unix permission bits (e.g. `0o755`). Unix-only: the `mode` int is passed straight to
/// `PermissionsExt::from_mode` (no masking — matches `std::fs`; caller owns out-of-range bits). On a
/// non-unix target this always faults with "chmod is unix-only".
fn chmod(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "chmod", 2)?;
    let path = h.arg_str(0)?;
    let mode = h.arg_int(1)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode as u32)) {
            Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
            Err(e) => Ok(NativeRet::Err(format!("{path}: {e}"))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        Ok(NativeRet::Err("chmod is unix-only".into()))
    }
}

/// Atomically replace a file: write the contents to a temp file in the SAME directory as the target,
/// then `rename` it over the target. `rename` is atomic only WITHIN one filesystem, so the temp being
/// a sibling (not in `/tmp`) is load-bearing. On any error the temp is cleaned up and the fault is
/// returned; a concurrent reader sees either the old contents or the new — never a half-written file.
/// (This is concurrent-observer atomicity via `rename`, NOT crash/power-loss durability: there is no
/// `fsync`, so an OS crash mid-op can still lose the write. Matching `write_file`, that is out of scope.)
/// When the target already exists its permission bits are carried onto the temp before the rename —
/// otherwise the fresh umask-default temp inode would silently widen a restrictive (e.g. `0o600`) file.
fn atomic_write(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "atomic_write", 2)?;
    let path = h.arg_str(0)?;
    let contents = h.arg_str(1)?;
    let parent = Path::new(&path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let seq = ATOMIC_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".chezzi_tmp{}_{seq}", std::process::id()));
    match std::fs::write(&tmp, contents.as_bytes()).and_then(|()| {
        // Preserve the target's mode across the inode swap. If the target does not exist yet, the
        // temp keeps the umask default (a genuinely new file).
        if let Ok(meta) = std::fs::metadata(&path) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, &path)
    }) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Ok(NativeRet::Err(format!("{path}: {e}")))
        }
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

/// Do `a` and `b` name the SAME file? Inode identity (dev+ino), not a path-string compare — a
/// symlink or a hardlink reaches one inode under two names. A missing side is never "the same",
/// so a copy to a new destination falls straight through.
fn same_file(a: &str, b: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(x), Ok(y)) => x == y,
            _ => false,
        }
    }
}

/// Copy a file's contents (file-only). Faults if the source is missing. The byte count is dropped
/// for parity-simplicity with `write_file`'s `Result[nil]` shape.
///
/// Refuses a SAME-FILE copy (same path, or via a symlink/hardlink to one inode) with an `Err`,
/// leaving the file untouched — `std::fs::copy` opens the destination `O_TRUNC`, so without this
/// guard `copy(p, p)` returned `Ok` after wiping the file. Matches Python `shutil.copyfile`'s
/// `SameFileError` and coreutils `cp a a`.
fn copy(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "copy", 2)?;
    let from = h.arg_str(0)?;
    let to = h.arg_str(1)?;
    if same_file(&from, &to) {
        return Ok(NativeRet::Err(format!("{from} -> {to}: are the same file")));
    }
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
    ("stat", stat),
    ("walk", walk),
    ("canonicalize", canonicalize),
    ("glob", glob),
    ("chmod", chmod),
    ("atomic_write", atomic_write),
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

    /// A minimal `Host` for exercising the fs natives in isolation. `strs` holds string args by
    /// index; `ints` holds int args by index (only `chmod`'s mode arg today) so the same host serves
    /// mixed str/int calls without a second type.
    #[derive(Default)]
    struct StrHost {
        strs: Vec<String>,
        ints: std::collections::HashMap<usize, i64>,
    }

    impl Host for StrHost {
        fn arg_count(&self) -> usize {
            self.strs.len() + self.ints.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            self.ints.get(&i).copied().ok_or(HostError {
                message: "missing int arg".into(),
            })
        }
        fn arg_float(&mut self, _i: usize) -> Result<f64, HostError> {
            Err(HostError {
                message: "no float args".into(),
            })
        }
        fn arg_is_int(&self, i: usize) -> bool {
            self.ints.contains_key(&i)
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
            ints: std::collections::HashMap::new(),
        }
    }

    /// Host for `chmod(path, mode)`: str at 0, int at 1.
    fn host_path_mode(path: &str, mode: i64) -> StrHost {
        StrHost {
            strs: vec![path.to_string()],
            ints: std::collections::HashMap::from([(1, mode)]),
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

    /// Unwrap an `Ok(Str)` NativeRet's payload, else panic.
    fn ok_str(r: NativeRet) -> String {
        match r {
            NativeRet::Ok(inner) => match *inner {
                NativeRet::Str(s) => s,
                other => panic!("expected Ok(Str), got Ok({other:?})"),
            },
            other => panic!("expected Ok(Str), got {other:?}"),
        }
    }

    /// `canonicalize` resolves symlinks + `.`/`..` to the real absolute path (unlike the purely
    /// lexical `path.normalize`). Unix-only symlink fixture.
    #[cfg(unix)]
    #[test]
    fn fs_canonicalize_resolves_symlink() {
        let tmp = TmpDir::new();
        let target = tmp.join("target.txt");
        std::fs::write(&target, "x").unwrap();
        let link = tmp.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // canonicalize(link) resolves to the real target path.
        let got = ok_str(canonicalize(&mut host(&[&link])).unwrap());
        let want = std::fs::canonicalize(&target).unwrap();
        assert_eq!(got, want.to_string_lossy());
        // A nonexistent path errs (canonicalize requires the path to exist).
        assert!(is_err(
            canonicalize(&mut host(&[&tmp.join("nope")])).unwrap()
        ));
    }

    /// `chmod` sets the unix permission bits; a metadata read confirms them.
    #[cfg(unix)]
    #[test]
    fn fs_chmod_sets_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TmpDir::new();
        let f = tmp.join("f");
        std::fs::write(&f, "x").unwrap();
        assert!(is_ok(chmod(&mut host_path_mode(&f, 0o644)).unwrap()));
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(is_ok(chmod(&mut host_path_mode(&f, 0o600)).unwrap()));
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // chmod on a missing path errs.
        assert!(is_err(
            chmod(&mut host_path_mode(&tmp.join("nope"), 0o644)).unwrap()
        ));
    }

    /// `atomic_write` writes the contents via a same-dir temp + rename, leaving no stray temp file.
    #[test]
    fn fs_atomic_write_writes_and_leaves_no_temp() {
        let tmp = TmpDir::new();
        let f = tmp.join("out.txt");
        assert!(is_ok(atomic_write(&mut host(&[&f, "hello"])).unwrap()));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
        // Overwrite atomically.
        assert!(is_ok(atomic_write(&mut host(&[&f, "world"])).unwrap()));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "world");
        // Exactly one entry named out.txt — proves the temp was same-dir + got renamed, not stranded.
        let names: Vec<String> = std::fs::read_dir(&tmp.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["out.txt".to_string()]);
        // A nonexistent parent dir errs.
        assert!(is_err(
            atomic_write(&mut host(&[&tmp.join("no/dir/x.txt"), "y"])).unwrap()
        ));
    }

    // --- fs.stat/fs.walk (gaps §6 metadata READ + recursive walk) ---

    /// Pull the `Ok(Struct)` fields out of a `NativeRet`, else panic.
    fn ok_struct_fields(r: NativeRet) -> (String, Vec<(String, NativeRet)>) {
        match r {
            NativeRet::Ok(inner) => match *inner {
                NativeRet::Struct { name, fields } => (name, fields),
                other => panic!("expected Ok(Struct), got Ok({other:?})"),
            },
            other => panic!("expected Ok(Struct), got {other:?}"),
        }
    }

    /// `stat` reads real filesystem metadata into a `FileInfo` struct: known byte size, positive
    /// mtime, nonzero mode (unix), and the is_dir/is_file/is_symlink flags.
    #[test]
    fn fs_stat_reads_metadata() {
        let tmp = TmpDir::new();
        let f = tmp.join("hello.txt");
        std::fs::write(&f, "hello\n").unwrap(); // 6 bytes
        let (name, fields) = ok_struct_fields(stat(&mut host(&[&f])).unwrap());
        assert_eq!(name, "FileInfo");
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["size", "mtime", "mode", "is_dir", "is_file", "is_symlink"]
        );
        assert_eq!(fields[0].1, NativeRet::Int(6)); // size
        assert!(matches!(fields[1].1, NativeRet::Int(t) if t > 0)); // mtime
        #[cfg(unix)]
        assert!(matches!(fields[2].1, NativeRet::Int(m) if m != 0)); // mode
        assert_eq!(fields[3].1, NativeRet::Bool(false)); // is_dir
        assert_eq!(fields[4].1, NativeRet::Bool(true)); // is_file
        assert_eq!(fields[5].1, NativeRet::Bool(false)); // is_symlink

        // stat a directory → is_dir true / is_file false.
        let (_, dfields) = ok_struct_fields(stat(&mut host(&[&tmp.join(".")])).unwrap());
        assert_eq!(dfields[3].1, NativeRet::Bool(true)); // is_dir
        assert_eq!(dfields[4].1, NativeRet::Bool(false)); // is_file

        // stat a missing path → Err (recoverable, not a fault).
        assert!(is_err(stat(&mut host(&[&tmp.join("nope")])).unwrap()));
    }

    /// `stat` follows symlinks for size/is_file (matches `stat`/os.stat default) but reports
    /// is_symlink=true from a separate symlink_metadata check.
    #[cfg(unix)]
    #[test]
    fn fs_stat_follows_symlink_but_flags_it() {
        let tmp = TmpDir::new();
        let target = tmp.join("t.txt");
        std::fs::write(&target, "abcd").unwrap(); // 4 bytes
        let link = tmp.join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (_, fields) = ok_struct_fields(stat(&mut host(&[&link])).unwrap());
        assert_eq!(fields[0].1, NativeRet::Int(4)); // size = target (followed)
        assert_eq!(fields[4].1, NativeRet::Bool(true)); // is_file (target)
        assert_eq!(fields[5].1, NativeRet::Bool(true)); // is_symlink
    }

    /// Unwrap `Ok(List(Str…))` into the path strings, else panic.
    fn ok_str_list(r: NativeRet) -> Vec<String> {
        match r {
            NativeRet::Ok(inner) => match *inner {
                NativeRet::List(items) => items
                    .into_iter()
                    .map(|it| match it {
                        NativeRet::Str(s) => s,
                        other => panic!("expected Str item, got {other:?}"),
                    })
                    .collect(),
                other => panic!("expected Ok(List), got Ok({other:?})"),
            },
            other => panic!("expected Ok(List), got {other:?}"),
        }
    }

    /// `walk` recursively lists every entry under a root in a deterministic per-dir-sorted, dir-before-
    /// children order (required for serial==M:N parity). The root itself is excluded.
    #[test]
    fn fs_walk_recursive_sorted() {
        let tmp = TmpDir::new();
        let root = tmp.join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(tmp.join("root/a.txt"), "a").unwrap();
        std::fs::write(tmp.join("root/b.txt"), "b").unwrap();
        std::fs::create_dir(tmp.join("root/sub")).unwrap();
        std::fs::write(tmp.join("root/sub/c.txt"), "c").unwrap();
        let got = ok_str_list(walk(&mut host(&[&root])).unwrap());
        assert_eq!(
            got,
            vec![
                tmp.join("root/a.txt"),
                tmp.join("root/b.txt"),
                tmp.join("root/sub"),
                tmp.join("root/sub/c.txt"),
            ]
        );
        // walk a missing root → Err.
        assert!(is_err(walk(&mut host(&[&tmp.join("nope")])).unwrap()));
    }

    /// `walk` lists a symlinked directory entry but does NOT recurse into it (cycle guard).
    #[cfg(unix)]
    #[test]
    fn fs_walk_does_not_follow_symlink_dirs() {
        let tmp = TmpDir::new();
        let root = tmp.join("r");
        std::fs::create_dir(&root).unwrap();
        let real = tmp.join("r/real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(tmp.join("r/real/x.txt"), "x").unwrap();
        // A symlink to the sibling dir — must be listed but NOT descended.
        std::os::unix::fs::symlink(&real, tmp.join("r/lnk")).unwrap();
        let got = ok_str_list(walk(&mut host(&[&root])).unwrap());
        assert_eq!(
            got,
            vec![
                tmp.join("r/lnk"), // symlink entry listed, not recursed
                tmp.join("r/real"),
                tmp.join("r/real/x.txt"),
            ]
        );
    }

    /// Overwriting an existing restrictive file must PRESERVE its mode — the rename swaps in a fresh
    /// umask-default temp inode, which would otherwise silently widen a `0o600` file to `~0o644`.
    #[cfg(unix)]
    #[test]
    fn fs_atomic_write_preserves_target_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TmpDir::new();
        let f = tmp.join("secret");
        std::fs::write(&f, "old").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(is_ok(atomic_write(&mut host(&[&f, "new"])).unwrap()));
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600,
            "atomic_write widened a 0o600 target"
        );
    }
}
