//! Minimal hand-rolled `chezzi.toml` manifest parser (zero new deps).
//!
//! Chezzi's manifest is a tiny, fixed schema — not a general TOML document. We parse only what the
//! toolchain reads today:
//!
//! ```toml
//! [project]
//! name = "myapp"
//! version = "0.1.0"
//! entrypoint = "src.main"   # dotted module path the bare `chezzi run` executes
//! ```
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
