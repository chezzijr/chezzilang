//! `std.fs` — filesystem queries + mutations (M8), extending `std.io`'s whole-file read/write.
//!
//! `list_dir` returns entry names (not full paths), sorted for determinism. `exists`/`is_file`/
//! `is_dir` are booleans; `size` returns the byte length as a `Result[int]`. `glob` is the Go
//! `filepath.Match` dialect — `*` (any run), `?` (single char), `[abc]`/`[a-z]`/`[^abc]` character
//! classes — in the **final** path component only: no `**`, no brace expansion, no escape character.
//! A malformed `[...]` class is an `Err` carrying "bad pattern", not a silent `Ok([])`.
//!
//! Mutations (each `Result[nil]`, recoverable fault on error): `mkdir` (recursive, idempotent),
//! `remove_file`, `remove_dir` (empty-only — no recursive `rm -rf`), `rename`, `copy` (file), and
//! `append` (create-or-append, never truncates). All filesystem access is real (like
//! `std.io.read_file`).

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};
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

/// W7-8 — read argument `i` as RAW OS path bytes. Every path-taking `std.fs`/`std.io`/`std.os` native
/// takes a `bytes` param now (the public `PathLike` wrapper in the `.chz` calls `p.as_path()` first),
/// so a non-UTF-8 filename reaches the syscall byte-exactly instead of through a `str` that cannot
/// represent it. On unix the bytes ARE the path (`OsStringExt`); elsewhere there is no byte-exact
/// `OsString`, so a lossy build is the only option (and non-unix is out of scope for this repo).
pub(crate) fn arg_path(h: &mut dyn Host, i: usize) -> Result<std::path::PathBuf, HostError> {
    let raw = h.arg_bytes(i)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(raw)))
    }
    #[cfg(not(unix))]
    {
        Ok(std::path::PathBuf::from(
            String::from_utf8_lossy(&raw).into_owned(),
        ))
    }
}

/// The raw OS bytes of a path, for handing one BACK to Chezzi as `path.Path`'s `raw` field.
/// Byte-exact on unix — this is the half W7-8 was losing.
pub(crate) fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

/// A path rendered for a HUMAN-facing error message. LOSSY on purpose — this is `Path::display()`'s
/// model (and `path.Path.str()`'s), NOT a missed decode: the value that gets USED is always the raw
/// bytes, and only the diagnostic text is substituted.
fn shown(p: &Path) -> std::path::Display<'_> {
    p.display()
}

fn list_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "list_dir", 1)?;
    let path = arg_path(h, 0)?;
    let rd = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => return Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    };
    // Sorted as RAW BYTES (not decoded strings) so the order stays deterministic — required for
    // byte-identical output regardless of worker count — and is well-defined for a name that is not UTF-8 at all.
    let mut names: Vec<Vec<u8>> = Vec::new();
    for entry in rd {
        match entry {
            Ok(e) => names.push(path_bytes(Path::new(&e.file_name()))),
            Err(e) => return Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
        }
    }
    names.sort();
    let items = names.into_iter().map(NativeRet::Bytes).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

fn exists(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "exists", 1)?;
    let path = arg_path(h, 0)?;
    Ok(NativeRet::Bool(path.exists()))
}

fn is_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_file", 1)?;
    let path = arg_path(h, 0)?;
    Ok(NativeRet::Bool(path.is_file()))
}

fn is_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_dir", 1)?;
    let path = arg_path(h, 0)?;
    Ok(NativeRet::Bool(path.is_dir()))
}

fn size(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "size", 1)?;
    let path = arg_path(h, 0)?;
    match std::fs::metadata(&path) {
        Ok(m) => Ok(NativeRet::Ok(Box::new(NativeRet::Int(m.len() as i64)))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
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
    let path = arg_path(h, 0)?;
    let m = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
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
/// byte-identical output regardless of worker count. Pre-order: a directory is listed before its children. A symlinked directory is
/// LISTED but NOT descended (cycle guard). The first unreadable directory, the root or any
/// descendant, aborts the walk with a recoverable `Err` NAMING THAT DIRECTORY (W8-39): `walk_into`
/// formats the message at the failing level because a bare `std::io::Error` carries no path, so it
/// must not go back to propagating one with `?`.
fn walk(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "walk", 1)?;
    let root = arg_path(h, 0)?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    if let Err(msg) = walk_into(&root, &mut out) {
        return Ok(NativeRet::Err(msg));
    }
    let items = out.into_iter().map(NativeRet::Bytes).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

/// Recursion helper for [`walk`]: sort each directory's entries by file name, then push each entry's
/// full path and recurse into it only if it is a real (non-symlink) directory. On failure, the
/// returned message names the directory that actually failed, not `dir`'s caller.
fn walk_into(dir: &Path, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", shown(dir)))?;
    let mut entries: Vec<std::fs::DirEntry> = rd
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| format!("{}: {e}", shown(dir)))?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let ft = e
            .file_type()
            .map_err(|err| format!("{}: {err}", shown(&p)))?; // does NOT follow symlinks — is_symlink is accurate
        out.push(path_bytes(&p));
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
    let path = arg_path(h, 0)?;
    match std::fs::canonicalize(&path) {
        Ok(p) => Ok(NativeRet::Ok(Box::new(NativeRet::Bytes(path_bytes(&p))))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    }
}

/// Set a file's unix permission bits (e.g. `0o755`). Unix-only: the `mode` int is passed straight to
/// `PermissionsExt::from_mode` (no masking — matches `std::fs`; caller owns out-of-range bits). On a
/// non-unix target this always faults with "chmod is unix-only".
fn chmod(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "chmod", 2)?;
    let path = arg_path(h, 0)?;
    let mode = h.arg_int(1)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode as u32)) {
            Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
            Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
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
    let path = arg_path(h, 0)?;
    let contents = h.arg_str(1)?;
    let parent = path
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
            Ok(NativeRet::Err(format!("{}: {e}", shown(&path))))
        }
    }
}

fn glob(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "glob", 1)?;
    // W7-8 — the PATTERN is raw bytes too, and matching runs over bytes: an ASCII pattern must be able
    // to match a non-UTF-8 filename (`*.txt` over `A\xffB.txt`), and re-attaching the caller's
    // directory prefix must be byte-exact or the fix would be undone one layer up.
    let pattern = h.arg_bytes(0)?;
    let shown_pat = String::from_utf8_lossy(&pattern).into_owned(); // human-facing text only
    // Split into the directory to scan and the wildcard for the final component.
    let (dir, pat): (&[u8], &[u8]) = match pattern.iter().rposition(|&b| b == b'/') {
        Some(i) => (&pattern[..i], &pattern[i + 1..]),
        None => (b".", &pattern[..]),
    };
    // W8-33 — a bad pattern is wrong regardless of whether the directory exists; `Ok([])` must never
    // again mean "your pattern was not understood".
    if let Err(msg) = check_pattern(pat) {
        return Ok(NativeRet::Err(format!("{shown_pat}: {msg}")));
    }
    let scan: &[u8] = if dir.is_empty() { b"/" } else { dir };
    let rd = match std::fs::read_dir(bytes_path(scan)) {
        Ok(rd) => rd,
        Err(e) => return Ok(NativeRet::Err(format!("{shown_pat}: {e}"))),
    };
    let has_slash = pattern.contains(&b'/');
    let mut hits: Vec<Vec<u8>> = Vec::new();
    for entry in rd {
        let name = match entry {
            Ok(e) => path_bytes(Path::new(&e.file_name())),
            Err(e) => return Ok(NativeRet::Err(format!("{shown_pat}: {e}"))),
        };
        if wildcard_match(pat, &name) {
            // Re-attach the directory prefix the caller wrote, so results are usable paths.
            if has_slash {
                let mut full = dir.to_vec();
                full.push(b'/');
                full.extend_from_slice(&name);
                hits.push(full);
            } else {
                hits.push(name);
            }
        }
    }
    hits.sort();
    let items = hits.into_iter().map(NativeRet::Bytes).collect();
    Ok(NativeRet::Ok(Box::new(NativeRet::List(items))))
}

/// Raw OS bytes → an owned `PathBuf` (byte-exact on unix, lossy elsewhere). The `&[u8]` twin of
/// [`arg_path`], for a path already in hand (a glob's scan dir, the VM-intercepted `io` openers).
pub(crate) fn bytes_path(b: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b))
    }
    #[cfg(not(unix))]
    {
        std::path::PathBuf::from(String::from_utf8_lossy(b).into_owned())
    }
}

// --- Mutations (M8+). All return `Result[nil]`, faulting (never panicking) on an I/O error,
// mirroring `std.io.write_file`'s `Ok(NativeRet::Ok(Nil))` / `Ok(NativeRet::Err("{path}: {e}"))`
// idiom so a permission-denied / missing-parent failure is a catchable Chezzi error.

/// Create a directory, recursively (like `mkdir -p`): missing parents are created and an existing
/// directory is a no-op (idempotent). Faults only on a real error (e.g. a parent component is a file).
fn mkdir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "mkdir", 1)?;
    let path = arg_path(h, 0)?;
    match std::fs::create_dir_all(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    }
}

/// Delete a single file. Faults if the path is missing or is a directory (use `remove_dir` for dirs).
fn remove_file(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "remove_file", 1)?;
    let path = arg_path(h, 0)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    }
}

/// Delete an EMPTY directory (non-recursive — faults on a non-empty dir, avoiding a silent `rm -rf`).
fn remove_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "remove_dir", 1)?;
    let path = arg_path(h, 0)?;
    match std::fs::remove_dir(&path) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    }
}

/// Move/rename a path. Faults if the source is missing (or a cross-device move is unsupported).
fn rename(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "rename", 2)?;
    let from = arg_path(h, 0)?;
    let to = arg_path(h, 1)?;
    match std::fs::rename(&from, &to) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!(
            "{} -> {}: {e}",
            shown(&from),
            shown(&to)
        ))),
    }
}

/// Do `a` and `b` name the SAME file? Inode identity (dev+ino), not a path-string compare — a
/// symlink or a hardlink reaches one inode under two names. A missing side is never "the same",
/// so a copy to a new destination falls straight through.
fn same_file(a: &Path, b: &Path) -> bool {
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
/// to keep the return shape simple, matching `write_file`'s `Result[nil]` shape.
///
/// Refuses a SAME-FILE copy (same path, or via a symlink/hardlink to one inode) with an `Err`,
/// leaving the file untouched — `std::fs::copy` opens the destination `O_TRUNC`, so without this
/// guard `copy(p, p)` returned `Ok` after wiping the file. Matches Python `shutil.copyfile`'s
/// `SameFileError` and coreutils `cp a a`.
fn copy(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "copy", 2)?;
    let from = arg_path(h, 0)?;
    let to = arg_path(h, 1)?;
    if same_file(&from, &to) {
        return Ok(NativeRet::Err(format!(
            "{} -> {}: are the same file",
            shown(&from),
            shown(&to)
        )));
    }
    match std::fs::copy(&from, &to) {
        Ok(_) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!(
            "{} -> {}: {e}",
            shown(&from),
            shown(&to)
        ))),
    }
}

/// Append a string to a file, creating it if absent (never truncates — complements
/// `std.io.write_file`'s overwrite). Faults on a real I/O error.
fn append(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    use std::io::Write;
    expect_args(h, "append", 2)?;
    let path = arg_path(h, 0)?;
    let contents = h.arg_str(1)?;
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(contents.as_bytes()));
    match result {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(format!("{}: {e}", shown(&path)))),
    }
}

/// Length in bytes of the UTF-8 scalar starting at `n[i]`, or `1` when `i` begins no valid sequence.
/// This is what keeps `glob`'s `?` counting CHARACTERS (Python `fnmatch` / Go `filepath.Match`) on a
/// normal filename while staying defined on a non-UTF-8 one — where "one character" has no meaning and
/// one byte is the only honest answer.
fn utf8_len_at(n: &[u8], i: usize) -> usize {
    let want = match n[i] {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        b if b >> 3 == 0b11110 => 4,
        _ => return 1, // a continuation byte or an invalid lead: not a sequence start
    };
    if i + want <= n.len() && std::str::from_utf8(&n[i..i + want]).is_ok() {
        want
    } else {
        1 // truncated or over-long: consume one byte so the walk always advances
    }
}

/// Parse a `[...]` character class starting at `p[i]` (`p[i] == b'['`). Returns
/// `(index just past the closing ']', negated?, [(lo, hi), ...])`, or `None` on a malformed class
/// (unterminated, or empty after an optional leading `^`).
///
/// Entries compare as byte slices, which IS code-point order for valid UTF-8. A `]` in first
/// position (right after `[` or `[^`) is a literal, matching Go and bash. There is deliberately NO
/// escape character — this matcher has never had one, and adding it would change what every
/// existing `*`/`?` pattern containing a backslash matches.
type ClassResult<'a> = Option<(usize, bool, Vec<(&'a [u8], &'a [u8])>)>;

fn class_at(p: &[u8], i: usize) -> ClassResult<'_> {
    let mut j = i + 1;
    let neg = j < p.len() && p[j] == b'^';
    if neg {
        j += 1;
    }
    let mut items: Vec<(&[u8], &[u8])> = Vec::new();
    loop {
        if j >= p.len() {
            return None; // unterminated
        }
        if p[j] == b']' && !items.is_empty() {
            return Some((j + 1, neg, items));
        }
        let lo_len = utf8_len_at(p, j);
        let lo = &p[j..j + lo_len];
        j += lo_len;
        let mut hi = lo;
        if j < p.len() && p[j] == b'-' && j + 1 < p.len() && p[j + 1] != b']' {
            j += 1;
            let hi_len = utf8_len_at(p, j);
            hi = &p[j..j + hi_len];
            j += hi_len;
        }
        if lo > hi {
            return None;
        }
        items.push((lo, hi));
    }
}

/// Reject a malformed `[` character class anywhere in `pat` — validated BEFORE `read_dir` so the
/// verdict does not depend on whether the directory exists. `Ok([])` must never mean "your pattern
/// was not understood".
fn check_pattern(pat: &[u8]) -> Result<(), String> {
    let mut i = 0;
    while i < pat.len() {
        if pat[i] == b'[' {
            match class_at(pat, i) {
                Some((next, _, _)) => i = next,
                None => return Err("bad pattern: malformed '[' character class".to_string()),
            }
        } else {
            i += utf8_len_at(pat, i);
        }
    }
    Ok(())
}

/// Match a single path component against a `*`/`?`/`[...]` wildcard (Go `filepath.Match` dialect).
/// `*` matches any run of bytes (including empty), `?` matches exactly one character, `[abc]`/
/// `[a-z]`/`[^abc]` matches one character against a POSIX character class; every other byte is
/// literal. Uses the classic greedy two-pointer algorithm with a single backtrack mark — linear-ish,
/// no exponential blowup on adversarial patterns like `*a*a*a…b`.
///
/// W7-8 — matching is over BYTES, not a decoded string: a filename need not be valid UTF-8 at all, and
/// decoding it first is exactly the bug this closes. `?` and a class's single character still consume
/// one **Unicode scalar** wherever the name actually is valid UTF-8 (Go's `filepath.Match` and
/// Python's `fnmatch` both count characters, and drifting from them would be its own bug — see
/// [`utf8_len_at`]); both fall back to one byte only for a byte that begins no valid sequence, which
/// is the only rule defined there at all.
fn wildcard_match(pat: &[u8], name: &[u8]) -> bool {
    let p = pat;
    let n = name;
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        let step: Option<(usize, usize)> = if pi < p.len() && p[pi] == b'?' {
            Some((pi + 1, utf8_len_at(n, ni)))
        } else if pi < p.len() && p[pi] == b'[' {
            match class_at(p, pi) {
                Some((next, neg, items)) => {
                    let cl = utf8_len_at(n, ni);
                    let ch = &n[ni..ni + cl];
                    let hit = items.iter().any(|&(lo, hi)| lo <= ch && ch <= hi) != neg;
                    if hit { Some((next, cl)) } else { None }
                }
                None => None,
            }
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
            continue;
        } else if pi < p.len() && p[pi] == n[ni] {
            Some((pi + 1, 1))
        } else {
            None
        };
        match step {
            Some((next_pi, consumed)) => {
                pi = next_pi;
                ni += consumed;
            }
            None => {
                if let Some(s) = star {
                    // Backtrack: the last `*` swallows one more character.
                    pi = s + 1;
                    mark += 1;
                    ni = mark;
                } else {
                    return false;
                }
            }
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Callable members. `(name, fn, kind)` — every filesystem syscall is [`Kind::Blocking`] (offloaded to
/// the dirty pool so it can't pin an M:N core worker), with no exceptions.
///
/// W7-8 — every path-taking member is `_`-prefixed and takes RAW `bytes`. It is the INTERNAL byte
/// seam: the PUBLIC name (`fs.exists`) is a bodied pure-Chezzi wrapper in `std/fs.chz` that takes a
/// `PathLike`, calls `p.as_path()`, and re-wraps a returned path into `path.Path`. The `_` is
/// convention only (there is no privacy mechanism) — documented once in docs/stdlib.md, not per name.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("_list_dir", list_dir, Kind::Blocking),
    ("_exists", exists, Kind::Blocking),
    ("_is_file", is_file, Kind::Blocking),
    ("_is_dir", is_dir, Kind::Blocking),
    ("_size", size, Kind::Blocking),
    // W7-19 — these two were never in the pre-`Kind` `is_blocking` name list (they were added after
    // it was written), so until 2026-08-05 they ran INLINE and pinned a core M:N worker for their
    // syscalls — `walk` for a whole tree walk. Off-heap-safe like their siblings: `arg_path` reads
    // `Host::arg_bytes`, and the returns are primitive `NativeRet`s already crossed by members that
    // offload today (`_list_dir` returns the same `Ok(List([Bytes…]))`; `process::run`/`run_args` the
    // same `Ok(Struct{…})`). Neither touches the heap, stdio or os state during the call.
    ("_stat", stat, Kind::Blocking),
    ("_walk", walk, Kind::Blocking),
    ("_canonicalize", canonicalize, Kind::Blocking),
    ("_glob", glob, Kind::Blocking),
    ("_chmod", chmod, Kind::Blocking),
    ("_atomic_write", atomic_write, Kind::Blocking),
    ("_mkdir", mkdir, Kind::Blocking),
    ("_remove_file", remove_file, Kind::Blocking),
    ("_remove_dir", remove_dir, Kind::Blocking),
    ("_rename", rename, Kind::Blocking),
    ("_copy", copy, Kind::Blocking),
    ("_append", append, Kind::Blocking),
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
        // W7-8 — the path-taking natives read their path through `arg_bytes` now. This unit host still
        // stores paths as `String` (every fixture it builds is UTF-8), so serve their UTF-8 bytes.
        fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, HostError> {
            self.strs
                .get(i)
                .map(|s| s.as_bytes().to_vec())
                .ok_or(HostError {
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
        fn os_getcwd(&self) -> Result<Vec<u8>, HostError> {
            Ok(b"/".to_vec())
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
    fn wildcard_matches_posix_character_classes() {
        assert!(wildcard_match(b"*.[ch]", b"a.c"));
        assert!(wildcard_match(b"*.[ch]", b"b.h"));
        assert!(!wildcard_match(b"*.[ch]", b"c.txt"));
        assert!(wildcard_match(b"[a-c]x", b"bx"));
        assert!(!wildcard_match(b"[^a-c]x", b"bx"));
        assert!(wildcard_match(b"[]a]", b"]"));
        assert!(!wildcard_match(b"[abc]", b""));
    }

    #[test]
    fn check_pattern_rejects_a_malformed_class() {
        assert!(check_pattern(b"*.[ch").is_err());
        assert!(check_pattern(b"*.[ch]").is_ok());
    }

    #[test]
    fn wildcard_star_and_question() {
        assert!(wildcard_match(b"*.txt", b"a.txt"));
        assert!(wildcard_match(b"*.txt", b".txt"));
        assert!(!wildcard_match(b"*.txt", b"a.md"));
        assert!(wildcard_match(b"a?c", b"abc"));
        assert!(!wildcard_match(b"a?c", b"ac"));
        assert!(wildcard_match(b"*", b"anything"));
        assert!(wildcard_match(b"foo*bar", b"fooXYZbar"));
        assert!(!wildcard_match(b"foo*bar", b"fooXYZbaz"));
        assert!(wildcard_match(b"exact", b"exact"));
        assert!(!wildcard_match(b"exact", b"exacted"));
    }

    /// W7-8 — the matcher runs over BYTES, so an ASCII pattern still matches a filename that is not
    /// valid UTF-8 at all (the whole point: decoding it first is the bug being closed).
    #[test]
    fn wildcard_matches_a_non_utf8_name() {
        assert!(wildcard_match(b"*.txt", b"A\xffB.txt"));
        assert!(wildcard_match(b"A?B.txt", b"A\xffB.txt"));
        assert!(!wildcard_match(b"*.md", b"A\xffB.txt"));
    }

    /// W7-8 (review) — `?` must still count one **Unicode scalar**, not one byte, on a name that IS
    /// valid UTF-8: Python `fnmatch` and Go `filepath.Match` both count characters, and a byte-counting
    /// `?` would silently stop matching every non-ASCII filename.
    #[test]
    fn wildcard_question_counts_one_character_not_one_byte() {
        assert!(wildcard_match("a?c".as_bytes(), "aéc".as_bytes())); // é is 2 bytes
        assert!(wildcard_match("?".as_bytes(), "😀".as_bytes())); // 4 bytes, one scalar
        assert!(wildcard_match("a??".as_bytes(), "aéf".as_bytes()));
        assert!(!wildcard_match("a?".as_bytes(), "aéf".as_bytes())); // one scalar left over
        // ...and it degrades to ONE byte only where no valid sequence starts.
        assert!(wildcard_match(b"a?c", b"a\xffc"));
        assert!(!wildcard_match(b"a?c", b"a\xff\xfec"));
    }

    /// Regression (review): a multi-star pattern against a long non-matching name must not blow up
    /// exponentially. The greedy two-pointer matcher returns near-instantly.
    #[test]
    fn wildcard_no_catastrophic_backtracking() {
        let pat = b"*a*a*a*a*a*a*a*a*a*a*b";
        let name = vec![b'a'; 64]; // no 'b' → never matches
        assert!(!wildcard_match(pat, &name));
        assert!(wildcard_match(b"*a*a*b", b"aaaaaaab"));
    }

    /// Unwrap an `Ok(Bytes)` NativeRet's path payload into a `String` (W7-8 — the path-returning
    /// natives hand back RAW bytes now; every fixture here is UTF-8, so the decode is exact).
    fn ok_str(r: NativeRet) -> String {
        match r {
            NativeRet::Ok(inner) => match *inner {
                NativeRet::Bytes(b) => String::from_utf8(b).expect("utf-8 fixture path"),
                other => panic!("expected Ok(Bytes), got Ok({other:?})"),
            },
            other => panic!("expected Ok(Bytes), got {other:?}"),
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

    /// Unwrap `Ok(List(Bytes…))` into the path strings, else panic (W7-8 — raw bytes now).
    fn ok_str_list(r: NativeRet) -> Vec<String> {
        match r {
            NativeRet::Ok(inner) => match *inner {
                NativeRet::List(items) => items
                    .into_iter()
                    .map(|it| match it {
                        NativeRet::Bytes(b) => String::from_utf8(b).expect("utf-8 fixture path"),
                        other => panic!("expected Bytes item, got {other:?}"),
                    })
                    .collect(),
                other => panic!("expected Ok(List), got Ok({other:?})"),
            },
            other => panic!("expected Ok(List), got {other:?}"),
        }
    }

    /// `walk` recursively lists every entry under a root in a deterministic per-dir-sorted, dir-before-
    /// children order (required for byte-identical output regardless of worker count). The root itself is excluded.
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

    /// `walk` over a readable root holding an unreadable subdirectory must name the subdirectory in
    /// the `Err`, not the root — the message is formatted at the level that actually failed.
    #[cfg(unix)]
    #[test]
    fn fs_walk_error_names_the_failing_subdir_not_the_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TmpDir::new();
        let root = tmp.join("wroot");
        let sub = tmp.join("wroot/sub");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let got = walk(&mut host(&[&root])).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        match got {
            NativeRet::Err(msg) => assert!(
                msg.starts_with(sub.as_str()),
                "expected the message to name {sub}, got {msg}"
            ),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    /// `walk` over an unreadable root must still name the root — the fix moves blame onto the
    /// failing path, it must not move blame off the root.
    #[cfg(unix)]
    #[test]
    fn fs_walk_unreadable_root_still_names_the_root() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TmpDir::new();
        let root = tmp.join("wroot2");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
        let got = walk(&mut host(&[&root])).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        match got {
            NativeRet::Err(msg) => assert!(
                msg.starts_with(root.as_str()),
                "expected the message to name {root}, got {msg}"
            ),
            other => panic!("expected Err, got {other:?}"),
        }
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
