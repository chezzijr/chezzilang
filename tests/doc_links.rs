//! The docs are many small files that link to each other, and a move or a split breaks a link
//! without failing any build. This test is the guard for the live docs (everything but
//! `docs/history/`, which keeps its records as they were written):
//!
//! 1. every relative markdown link resolves to a file, and its `#anchor` to a heading in that file;
//! 2. no live doc grows past `MAX_LINES`, so each file stays small enough to read;
//! 3. `docs/README.md` links every live doc, so no doc is an orphan.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_LINES: usize = 600;

/// The root docs plus every `.md` under `docs/` except `docs/history/`.
fn live_docs() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = ["README.md", "PROGRESS.md", "CLAUDE.md"]
        .iter()
        .map(PathBuf::from)
        .collect();
    walk(Path::new("docs"), &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path != Path::new("docs/history") {
                walk(&path, out);
            }
        } else if path.extension().is_some_and(|x| x == "md") {
            out.push(path);
        }
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines outside fenced code blocks, so a `](x)` or `# x` inside an example is not a link or a heading.
fn prose_lines(text: &str) -> Vec<&str> {
    let mut in_fence = false;
    text.lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                return false;
            }
            !in_fence
        })
        .collect()
}

/// GitHub's heading anchors: lowercase, drop punctuation except `-` and `_`, spaces to `-`, and a
/// `-N` suffix on the Nth repeat of a slug.
fn anchors(text: &str) -> HashSet<String> {
    let mut seen = HashSet::new();
    for line in prose_lines(text) {
        if let Some(id) = line
            .split("<a id=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
        {
            seen.insert(id.to_string());
        }
        let Some(title) = line.strip_prefix('#') else {
            continue;
        };
        let title = title.trim_start_matches('#');
        if !title.starts_with(' ') {
            continue;
        }
        let slug: String = title
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
            .map(|c| if c == ' ' { '-' } else { c })
            .collect();
        let mut candidate = slug.clone();
        let mut n = 0;
        while seen.contains(&candidate) {
            n += 1;
            candidate = format!("{slug}-{n}");
        }
        seen.insert(candidate);
    }
    seen
}

/// The `](target)` link targets in a doc's prose.
fn link_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in prose_lines(text) {
        // Odd-indexed pieces of a backtick split are inline code, where `](` is code, not a link.
        let prose: String = line.split('`').step_by(2).collect::<Vec<_>>().join(" ");
        let mut rest = prose.as_str();
        while let Some(idx) = rest.find("](") {
            rest = &rest[idx + 2..];
            let Some(end) = rest.find(')') else { break };
            let target = rest[..end]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            out.push(target);
            rest = &rest[end..];
        }
    }
    out
}

/// A target that is not a local doc link: a URL, or unfenced code such as `f[T](x)` or `xs[0](1)`.
/// A local link is an `#anchor`, a directory path ending in `/`, or a path whose last segment has a
/// `name.ext` file name.
fn is_external(target: &str) -> bool {
    if target.contains("://") || target.starts_with("mailto:") {
        return true;
    }
    if target.starts_with('#') || target.split('#').next().unwrap().ends_with('/') {
        return false;
    }
    let file = target.split('#').next().unwrap();
    let name = file.rsplit('/').next().unwrap();
    !matches!(name.rsplit_once('.'), Some((stem, ext))
        if !stem.is_empty() && !stem.ends_with('.') && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()))
}

#[test]
fn every_relative_link_resolves() {
    let mut broken = Vec::new();
    for doc in live_docs() {
        let text = read(&doc);
        for target in link_targets(&text) {
            if is_external(&target) {
                continue;
            }
            let (file, anchor) = match target.split_once('#') {
                Some((f, a)) => (f, Some(a)),
                None => (target.as_str(), None),
            };
            let resolved = if file.is_empty() {
                doc.clone()
            } else {
                doc.parent().unwrap().join(file)
            };
            if !resolved.exists() {
                broken.push(format!(
                    "{}: {target} (no file {})",
                    doc.display(),
                    resolved.display()
                ));
                continue;
            }
            if let Some(anchor) = anchor {
                if resolved.extension().is_some_and(|x| x == "md")
                    && !anchors(&read(&resolved)).contains(anchor)
                {
                    broken.push(format!(
                        "{}: {target} (no heading #{anchor})",
                        doc.display()
                    ));
                }
            }
        }
    }
    assert!(
        broken.is_empty(),
        "{} broken doc links:\n{}",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn every_live_doc_stays_small() {
    let big: Vec<String> = live_docs()
        .iter()
        .map(|d| (d, read(d).lines().count()))
        .filter(|(_, n)| *n > MAX_LINES)
        .map(|(d, n)| format!("{}: {n} lines", d.display()))
        .collect();
    assert!(
        big.is_empty(),
        "docs over {MAX_LINES} lines:\n{}",
        big.join("\n")
    );
}

#[test]
fn the_docs_index_links_every_live_doc() {
    let index = Path::new("docs/README.md");
    let linked: HashSet<PathBuf> = link_targets(&read(index))
        .iter()
        .map(|t| index.parent().unwrap().join(t.split('#').next().unwrap()))
        .collect();
    let missing: Vec<String> = live_docs()
        .iter()
        .filter(|d| d.starts_with("docs") && d.as_path() != index && !linked.contains(*d))
        .map(|d| d.display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "docs/README.md does not link:\n{}",
        missing.join("\n")
    );
}

#[test]
fn anchors_follow_the_github_slug_rules() {
    let a = anchors(
        "# Hello, World\n## 6b. Indexing & slicing\n# Hello, World\n<a id=\"raw\"></a>\n```\n# not a heading\n```\n",
    );
    assert!(a.contains("raw"));
    assert!(a.contains("hello-world"));
    assert!(a.contains("6b-indexing--slicing"));
    assert!(a.contains("hello-world-1"));
    assert!(!a.contains("not-a-heading"));
}

#[test]
fn directory_and_file_links_are_checked_and_code_is_not() {
    assert!(!is_external("history/progress/"));
    assert!(!is_external("../PROGRESS.md#status"));
    assert!(!is_external("#anchor"));
    assert!(is_external("https://example.com/a.md"));
    assert!(is_external("1"));
    assert!(is_external("..."));
}
