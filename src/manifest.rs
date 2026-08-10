//! Minimal hand-rolled `chezzi.toml` manifest parser (zero new deps).
//!
//! Chezzi's manifest is a tiny, fixed schema — not a general TOML document. We parse only what the
//! toolchain reads today:
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entrypoint = "src.main:main"   # module path + ":function" the bare `chezzi run` executes
//! ```
//!
//! The `entrypoint` value is a dotted module path, optionally suffixed with `:function`. With a
//! `:function` suffix (e.g. `"src.main:main"`) a bare `chezzi run` runs the module's top-level and
//! then calls that function — so the source needs no trailing call. Without the suffix
//! (`"src.main"`) it just runs the module top-level (scripting model). [`parse`] keeps the value
//! verbatim; [`split_entrypoint`] / [`entrypoint_file`] interpret it, and [`entry_fn_for`] answers
//! the one question every static consumer asks — "is THIS file the project's entry module, and what
//! function does it owe?".
//!
//! Recognized syntax: `[section]` headers, `key = "value"` string pairs, `#` comments, blank lines,
//! and leading/trailing whitespace. Only `[project]` keys (`name`/`version`/`entrypoint`) are
//! captured; unknown sections and unknown keys are ignored. A line that is neither blank, a comment,
//! a section header, nor a quoted `key = "value"` pair is a hard parse error (the schema is small and
//! fixed — silently skipping a malformed line would hide e.g. an `entrypoint` typo).
//!
//! An **empty** manifest parses fine to an all-`None` `Manifest` (the existing fixtures are empty
//! root markers): `entrypoint` is optional.

/// The parsed `[project]` fields the toolchain understands. All optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub entrypoint: Option<String>,
}

/// Parse a `chezzi.toml` source string into a [`Manifest`]. Returns `Err(message)` on a malformed
/// line; an empty (or comment/whitespace-only) file is `Ok(Manifest::default())`.
pub fn parse(src: &str) -> Result<Manifest, String> {
    let mut manifest = Manifest::default();
    let mut section: Option<String> = None;

    for (i, raw) in src.lines().enumerate() {
        let lineno = i + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix('[') {
            // Section header: `[name]`.
            let Some(name) = rest.strip_suffix(']') else {
                return Err(format!(
                    "chezzi.toml:{lineno}: malformed section header '{}' (expected '[name]')",
                    line
                ));
            };
            let name = name.trim();
            if name.is_empty() {
                return Err(format!("chezzi.toml:{lineno}: empty section header '[]'"));
            }
            section = Some(name.to_string());
            continue;
        }

        // Otherwise it must be a `key = "value"` pair.
        let Some((key, value_raw)) = line.split_once('=') else {
            return Err(format!(
                "chezzi.toml:{lineno}: expected 'key = \"value\"', got '{}'",
                line
            ));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("chezzi.toml:{lineno}: empty key before '='"));
        }
        let value = parse_string_value(value_raw.trim()).ok_or_else(|| {
            format!(
                "chezzi.toml:{lineno}: value for '{key}' must be a double-quoted string, got '{}'",
                value_raw.trim()
            )
        })?;

        // Only `[project]` keys are captured; everything else is ignored.
        if section.as_deref() == Some("project") {
            match key {
                "name" => manifest.name = Some(value),
                "version" => manifest.version = Some(value),
                "entrypoint" => manifest.entrypoint = Some(value),
                _ => {} // unknown key — ignore
            }
        }
    }

    Ok(manifest)
}

/// Strip an unquoted trailing `#` comment. A `#` inside a double-quoted string is preserved. The
/// scanner honors `\"` / `\\` escapes inside strings so it agrees with [`parse_string_value`] on
/// where a string literal ends (otherwise a value like `"a\"#b"` would be truncated at the `#`).
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if in_str {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
        } else {
            match ch {
                '"' => in_str = true,
                '#' => return &line[..idx],
                _ => {}
            }
        }
    }
    line
}

/// Parse a double-quoted string literal (the only value form the schema allows). Returns `None` if
/// it is not a `"..."` literal. Supports `\\` and `\"` escapes.
fn parse_string_value(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Split a manifest `entrypoint` value into its dotted module path and an optional `:function`
/// suffix. Splits on the FIRST `:` so the function name is taken verbatim; `"src.main"` →
/// `("src.main", None)`, `"src.main:main"` → `("src.main", Some("main"))`. A `:` with no function
/// after it (`"src.main:"`) is rejected — otherwise it reaches the VM as an empty name and produces a
/// baffling "function `` not found" error. Pure (no I/O) so it is unit-testable.
pub fn split_entrypoint(entrypoint: &str) -> Result<(&str, Option<&str>), String> {
    match entrypoint.split_once(':') {
        Some((_, "")) => Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; the ':' must be followed by a function name like \"src.main:main\""
        )),
        Some((module, func)) => Ok((module, Some(func))),
        None => Ok((entrypoint, None)),
    }
}

/// Map a manifest `[project] entrypoint` (a dotted module path) to its `.chz` file, root-relatively.
/// Validates the path FIRST: an empty / whitespace / leading- or trailing-dot / doubled-dot value
/// would otherwise feed empty path segments to [`crate::resolver::module_file`], whose `push("")` +
/// `set_extension` rewrites the project-root dir's own extension and escapes the root (e.g.
/// `<root>.chz`), producing a baffling "cannot read" error. Pure (no cwd/env) so it is unit-testable.
pub fn entrypoint_file(
    entrypoint: &str,
    root: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    // Trim surrounding whitespace on EACH segment before building the path, so a padded value like
    // `" app "` resolves to `app.chz` rather than the baffling `<root>/ app .chz` ("cannot read").
    // An embedded path separator would resolve by accident via `PathBuf::push` instead of the
    // documented dotted form, so reject it up front.
    if entrypoint.contains('/') || entrypoint.contains('\\') {
        return Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; the module path must use '.' separators, not '/'"
        ));
    }
    let segs: Vec<String> = entrypoint
        .split('.')
        .map(|s| s.trim().to_string())
        .collect();
    if segs.iter().any(String::is_empty) {
        return Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; expected a dotted module path like \"src.main\""
        ));
    }
    Ok(crate::resolver::module_file(
        &segs,
        root,
        &crate::resolver::std_root(),
    ))
}

/// M24 — the manifest entry FUNCTION `file` is declared to provide, or `None`.
///
/// `Some("main")` exactly when the project `file` belongs to has `entrypoint = "<path>:main"` AND
/// `<path>` resolves to `file` itself. That is a property of the PROJECT, not of one CLI invocation:
/// the manifest declares the function's required shape (invoked by name, with no arguments), so a
/// generic that would take a hidden type witness is broken in that file however you reach it — the
/// same way Go rejects a `func main` with parameters at build time. So this is the ONE derivation
/// every consumer that statically checks a file uses (`chezzi check`, `chezzi run`, the editor /
/// LSP); bare `chezzi run` passes the name it already resolved and gets the same answer.
///
/// Silent (`None`) on every failure — no manifest, unreadable, malformed, no `entrypoint`, no
/// `:function` suffix, a different module. Reporting those is the run path's job; a file with no
/// project around it must still check.
pub fn entry_fn_for(file: &std::path::Path) -> Option<String> {
    // Canonicalize FIRST: the caller's path is often relative to the cwd, and `find_root_from_dir`
    // walks `Path::parent`, which on a relative path stops at `""` — it would then read the cwd's
    // manifest for a file two directories up. Both sides of the identity compare are canonical too,
    // so a symlinked or `./`-prefixed spelling of the entry module still matches.
    let file = std::fs::canonicalize(file).ok()?;
    let root = crate::resolver::find_root_from_dir(file.parent()?)?;
    let src = std::fs::read_to_string(root.join("chezzi.toml")).ok()?;
    let entrypoint = parse(&src).ok()?.entrypoint?;
    let (module_path, entry_fn) = split_entrypoint(&entrypoint).ok()?;
    let entry_fn = entry_fn?;
    let declared = std::fs::canonicalize(entrypoint_file(module_path, &root).ok()?).ok()?;
    (declared == file).then(|| entry_fn.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full() {
        let src = "[project]\nname = \"myapp\"\nversion = \"0.1.0\"\nentrypoint = \"src.main\"\n";
        let m = parse(src).expect("should parse");
        assert_eq!(m.name.as_deref(), Some("myapp"));
        assert_eq!(m.version.as_deref(), Some("0.1.0"));
        assert_eq!(m.entrypoint.as_deref(), Some("src.main"));
    }

    #[test]
    fn parse_empty() {
        // Empty manifest (the existing root-marker fixtures) → all None, not an error.
        assert_eq!(parse("").unwrap(), Manifest::default());
        assert_eq!(parse("\n\n").unwrap(), Manifest::default());
    }

    #[test]
    fn parse_comments_ws() {
        let src = "\
# top comment
   \t
[project]
   name = \"spaced\"      # trailing comment
\tversion = \"1.2.3\"
# entrypoint = \"unused.commented\"
";
        let m = parse(src).expect("should parse");
        assert_eq!(m.name.as_deref(), Some("spaced"));
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        // The commented-out entrypoint must NOT be captured.
        assert_eq!(m.entrypoint, None);
    }

    #[test]
    fn parse_malformed_line_errors() {
        // A non-blank, non-comment, non-section line that is not `key = "value"`.
        let err = parse("[project]\nthis is not valid\n").unwrap_err();
        assert!(err.contains("chezzi.toml:2"), "got: {err}");
    }

    #[test]
    fn parse_unquoted_value_errors() {
        let err = parse("[project]\nname = bare\n").unwrap_err();
        assert!(err.contains("double-quoted"), "got: {err}");
    }

    #[test]
    fn escaped_quote_before_hash_is_preserved() {
        // strip_comment must honor `\"` so the `#` inside the value is not treated as a comment.
        let m = parse("[project]\nname = \"a\\\"#b\"\n").expect("should parse");
        assert_eq!(m.name.as_deref(), Some("a\"#b"));
    }

    #[test]
    fn unknown_sections_and_keys_ignored() {
        let src = "[other]\nname = \"ignored\"\n[project]\nunknown = \"x\"\nname = \"real\"\n";
        let m = parse(src).expect("should parse");
        assert_eq!(m.name.as_deref(), Some("real"));
        assert_eq!(m.version, None);
    }
}
