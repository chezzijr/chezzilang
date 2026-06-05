//! Module resolver (M4.5): turns an entry `.chz` file into a parsed, ordered dependency graph.
//!
//! Pure filesystem + dotted-path logic — it does **not** type-check or run. Both the checker and
//! the interpreter consume the same [`ModuleGraph`] so they agree on what gets loaded and in what
//! order.
//!
//! Resolution rules (see `docs/spec.md` §"Imports & module resolution"):
//!   1. Take the entry `.chz`. Walk *up* for `chezzi.toml` → that dir is the project root.
//!      None found → the entry's own dir is root. (Kills Python's run-relative import footgun.)
//!   2. `std.*` is reserved → always resolves under the stdlib dir ([`std_root`]).
//!   3. `a.b.c` → `<root>/a/b/c.chz`. No `./` relative imports.
//!   4. Import cycles are a clean error (Go-style), not lazy resolution.

use crate::ast::{Import, Module, Span, StmtKind};
use crate::{lexer, parser};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A module's stable identity: its canonicalized absolute path. De-dupes diamond imports and never
/// aliases `std.io` with a same-named local file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub PathBuf);

/// A resolution failure. `span` anchors at the offending `import` statement (or `{1,1}` for the
/// entry file itself). `module` is the dotted path being resolved, for context.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolveError {
    pub message: String,
    pub span: Span,
    pub module: Option<String>,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resolve error ({}): {}", self.span, self.message)
    }
}

/// One import statement resolved to a target module.
#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub target: ModuleId,
    pub import: Import,
    pub span: Span,
}

/// A parsed module plus its resolved imports.
#[derive(Debug, Clone)]
pub struct LoadedModule {
    pub id: ModuleId,
    /// Dotted path (`["core","db"]`); empty for the entry module.
    pub dotted: Vec<String>,
    pub ast: Module,
    pub imports: Vec<ResolvedImport>,
}

impl LoadedModule {
    /// Human label for messages: the dotted name, or the file stem for the entry.
    pub fn label(&self) -> String {
        if self.dotted.is_empty() {
            self.id
                .0
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<entry>".to_string())
        } else {
            self.dotted.join(".")
        }
    }
}

/// The whole dependency graph: modules in load order (dependencies first, entry last).
#[derive(Debug, Clone)]
pub struct ModuleGraph {
    pub entry: ModuleId,
    pub modules: Vec<LoadedModule>,
}

/// Walk up from `entry`'s directory looking for `chezzi.toml`; that dir is the project root. None
/// found → the entry's own directory.
pub fn find_root(entry: &Path) -> PathBuf {
    let start = entry.parent().unwrap_or_else(|| Path::new("."));
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("chezzi.toml").is_file() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    start.to_path_buf()
}

/// The stdlib directory: `$CHEZZI_STD` if set, else the compile-time `<crate>/std`. The env
/// override keeps tests deterministic (point it at a tempdir) and defers a real install story
/// (next-to-binary discovery) to M6, when `std/` actually ships content.
pub fn std_root() -> PathBuf {
    match std::env::var_os("CHEZZI_STD") {
        Some(p) => PathBuf::from(p),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("std"),
    }
}

/// Map a dotted import path to a filesystem path. `std.*` resolves under `std_root`; everything
/// else under `project_root`. The file is **not** required to exist (the std case intentionally
/// resolves to an absent file in M4.5 — content arrives in M6).
pub fn module_file(path: &[String], project_root: &Path, std_root: &Path) -> PathBuf {
    let (base, segs): (&Path, &[String]) = if path.first().map(String::as_str) == Some("std") {
        (std_root, &path[1..])
    } else {
        (project_root, path)
    };
    let mut p = base.to_path_buf();
    for seg in segs {
        p.push(seg);
    }
    p.set_extension("chz");
    p
}

/// Build the dependency graph rooted at `entry`: read, lex, and parse the entry and every
/// transitively imported module; detect cycles; return modules in a stable load order
/// (dependencies before dependents — reverse-postorder DFS, entry last).
pub fn build_graph(entry: &Path) -> Result<ModuleGraph, ResolveError> {
    let entry_abs = abs(entry);
    let project_root = find_root(&entry_abs);
    let std_root = std_root();
    let mut b = Builder {
        project_root,
        std_root,
        visited: HashMap::new(),
        on_stack: Vec::new(),
        order: Vec::new(),
    };
    let entry_id = ModuleId(canonical_or_abs(&entry_abs));
    b.visit(&entry_id, &[], Span { line: 1, col: 1 })?;
    Ok(ModuleGraph { entry: entry_id, modules: b.order })
}

struct Builder {
    project_root: PathBuf,
    std_root: PathBuf,
    /// Fully emitted modules (dedup / run-once parse).
    visited: HashMap<ModuleId, ()>,
    /// Modules currently being processed (cycle detection), with their dotted labels.
    on_stack: Vec<(ModuleId, Vec<String>)>,
    order: Vec<LoadedModule>,
}

impl Builder {
    fn visit(&mut self, id: &ModuleId, dotted: &[String], import_span: Span) -> Result<(), ResolveError> {
        // Cycle: this module is already on the active DFS stack.
        if let Some(pos) = self.on_stack.iter().position(|(sid, _)| sid == id) {
            let mut chain: Vec<String> =
                self.on_stack[pos..].iter().map(|(_, d)| dotted_label(d)).collect();
            chain.push(dotted_label(dotted));
            return Err(ResolveError {
                message: format!("import cycle: {}", chain.join(" -> ")),
                span: import_span,
                module: Some(dotted_label(dotted)),
            });
        }
        // Already loaded (e.g. via the other arm of a diamond).
        if self.visited.contains_key(id) {
            return Ok(());
        }

        let source = std::fs::read_to_string(&id.0).map_err(|_| ResolveError {
            message: format!("cannot find module '{}' (looked for {})", dotted_label(dotted), id.0.display()),
            span: import_span,
            module: Some(dotted_label(dotted)),
        })?;
        let ast = self.parse(&source, dotted)?;
        let imports = self.scan_imports(&ast);

        self.on_stack.push((id.clone(), dotted.to_vec()));
        let mut resolved = Vec::with_capacity(imports.len());
        for (import, span) in imports {
            let path = import_path(&import);
            let file = module_file(&path, &self.project_root, &self.std_root);
            let target = ModuleId(canonical_or_abs(&file));
            self.visit(&target, &path, span)?;
            resolved.push(ResolvedImport { target, import, span });
        }
        self.on_stack.pop();

        self.visited.insert(id.clone(), ());
        self.order.push(LoadedModule {
            id: id.clone(),
            dotted: dotted.to_vec(),
            ast,
            imports: resolved,
        });
        Ok(())
    }

    /// Lex + parse a module's source, wrapping failures with the module label (since `Span`
    /// carries no filename).
    fn parse(&self, source: &str, dotted: &[String]) -> Result<Module, ResolveError> {
        let tokens = lexer::tokenize(source).map_err(|e| ResolveError {
            message: prefix(dotted, e.to_string()),
            span: Span { line: e.line, col: 1 },
            module: opt_label(dotted),
        })?;
        parser::parse(tokens).map_err(|e| ResolveError {
            message: prefix(dotted, e.to_string()),
            span: e.span,
            module: opt_label(dotted),
        })
    }

    fn scan_imports(&self, ast: &Module) -> Vec<(Import, Span)> {
        ast.stmts
            .iter()
            .filter_map(|s| match &s.kind {
                StmtKind::Import(import) => Some((import.clone(), s.span)),
                _ => None,
            })
            .collect()
    }
}

fn import_path(import: &Import) -> Vec<String> {
    match import {
        Import::Module { path, .. } => path.clone(),
        Import::From { path, .. } => path.clone(),
    }
}

fn dotted_label(dotted: &[String]) -> String {
    if dotted.is_empty() {
        "<entry>".to_string()
    } else {
        dotted.join(".")
    }
}

fn opt_label(dotted: &[String]) -> Option<String> {
    if dotted.is_empty() {
        None
    } else {
        Some(dotted.join("."))
    }
}

fn prefix(dotted: &[String], msg: String) -> String {
    if dotted.is_empty() {
        msg
    } else {
        format!("in module '{}': {msg}", dotted.join("."))
    }
}

/// Make a path absolute (without requiring it to exist).
fn abs(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Canonicalize if the file exists; otherwise fall back to an absolute, lexically-normalized path
/// (so a missing module still gets a stable id for the error path).
fn canonical_or_abs(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| normalize(&abs(p)))
}

/// Lexical `.`/`..` normalization (no filesystem access) — a stable key for non-existent files.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// A unique temp directory, removed on drop.
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_res_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, contents).unwrap();
            p
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // 1. find_root walks up to the chezzi.toml marker.
    #[test]
    fn find_root_uses_chezzi_toml() {
        let t = TmpDir::new();
        t.write("chezzi.toml", "");
        let entry = t.write("sub/deep/main.chz", "fn main(): print(1)\n");
        let root = find_root(&entry);
        assert_eq!(canonical_or_abs(&root), canonical_or_abs(&t.0));
    }

    // 2. No marker anywhere → root is the entry's own dir (no run-relative footgun).
    #[test]
    fn find_root_falls_back_to_entry_dir() {
        let t = TmpDir::new();
        let entry = t.write("sub/main.chz", "fn main(): print(1)\n");
        let root = find_root(&entry);
        assert_eq!(canonical_or_abs(&root), canonical_or_abs(&t.path("sub")));
    }

    // 3. Local dotted path → <root>/a/b/c.chz.
    #[test]
    fn resolve_local_dotted_path() {
        let root = PathBuf::from("/proj");
        let std = PathBuf::from("/proj/std");
        let got = module_file(&["a".into(), "b".into(), "c".into()], &root, &std);
        assert_eq!(got, PathBuf::from("/proj/a/b/c.chz"));
    }

    // 4. std.* resolves under the std dir (path only — std/ ships no content in M4.5).
    #[test]
    fn resolve_std_path_points_at_std_dir() {
        let root = PathBuf::from("/proj");
        let std = PathBuf::from("/somewhere/std");
        let got = module_file(&["std".into(), "io".into()], &root, &std);
        assert_eq!(got, PathBuf::from("/somewhere/std/io.chz"));
    }

    // 5. A direct cycle is a clean error, not a stack overflow.
    #[test]
    fn cycle_detected_cleanly() {
        let t = TmpDir::new();
        let entry = t.write("a.chz", "import b\nfn main(): print(1)\n");
        t.write("b.chz", "import a\nfn f(): print(2)\n");
        let err = build_graph(&entry).unwrap_err();
        assert!(err.message.contains("cycle"), "got: {}", err.message);
        assert!(err.message.contains("a") && err.message.contains("b"), "got: {}", err.message);
    }

    // 6. A missing imported module is a clean error, not a panic.
    #[test]
    fn missing_module_is_clean_error() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "import nope.thing\nfn main(): print(1)\n");
        let err = build_graph(&entry).unwrap_err();
        assert!(err.message.contains("cannot find module"), "got: {}", err.message);
        assert!(err.message.contains("nope.thing"), "got: {}", err.message);
    }

    // 7. A diamond loads the shared module once; deps precede dependents, entry last.
    #[test]
    fn diamond_loads_each_module_once() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "import a\nimport b\nfn main(): print(1)\n");
        t.write("a.chz", "import c\nfn fa(): print(1)\n");
        t.write("b.chz", "import c\nfn fb(): print(1)\n");
        t.write("c.chz", "fn fc(): print(1)\n");
        let graph = build_graph(&entry).unwrap();

        let labels: Vec<String> = graph.modules.iter().map(|m| m.label()).collect();
        assert_eq!(labels.iter().filter(|l| *l == "c").count(), 1, "c loaded more than once: {labels:?}");

        let pos = |name: &str| labels.iter().position(|l| l == name).unwrap();
        assert!(pos("c") < pos("a") && pos("c") < pos("b"), "deps before dependents: {labels:?}");
        // Entry is last and has no dotted name.
        assert_eq!(graph.modules.last().unwrap().id, graph.entry);
    }
}
