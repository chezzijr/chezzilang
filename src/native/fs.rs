//! `std.fs` — filesystem queries (M8), extending `std.io`'s whole-file read/write.
//!
//! `list_dir` returns entry names (not full paths), sorted for determinism. `exists`/`is_file`/
//! `is_dir` are booleans; `size` returns the byte length as a `Result[int]`. `glob` supports the
//! common `*` (any run) and `?` (single char) wildcards in the **final** path component only — no
//! `**`, no brace expansion (a focused, dependency-free matcher). All filesystem access is real
//! (like `std.io.read_file`).

use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::path::Path;

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
];

#[cfg(test)]
mod tests {
    use super::wildcard_match;

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
