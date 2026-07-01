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

/// Maximum depth of a transitive import chain before the resolver gives up with a clean diagnostic
/// instead of recursing further. The DFS `Builder::visit` recurses once per import *edge*; a
/// pathological linear chain (~8-10k modules deep) otherwise overflows the host stack and aborts the
/// process (SIGABRT). The limit MUST be safe on the *smallest* stack this recursion runs on — not
/// just the 8MB main thread used by `check`/`run`, but the **~2MB default tokio worker** the LSP
/// resolves on (`chezzi-lsp` → `editor::diagnostics` → `build_graph_with_entry_source`, no
/// `thread_stack_size` override), **and** debug builds whose per-frame stack use is ~3-5x a release
/// frame. Sizing for the worst case (2MB worker × ~5KB debug frame ≈ 400 frames, less the tower-lsp
/// task future's own usage): 256 clears every build×path combo with margin (≈1.3MB debug-worst,
/// ≈256KB release) while sitting far above any real project (tens/hundreds of modules — a diamond
/// re-import dedupes via `visited` and does *not* count toward depth; a 256-deep *linear* import
/// chain is not a real program). Cycles are caught separately; this backstops the
/// acyclic-but-very-deep case only.
const MAX_IMPORT_DEPTH: usize = 256;

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
    /// `Some(name)` for a **native** std module (`std.math`/`std.io`/`std.os`, M6c): a virtual
    /// module with no `.chz` file, whose members are Rust `NativeFn`s injected by each engine.
    /// `None` for an ordinary file-backed module.
    pub native: Option<&'static str>,
}

impl LoadedModule {
    /// ROOT REDESIGN — whether this is a stdlib module (`std.*`, file-backed OR native). Its exported
    /// types (`Ref`, `Iterator`, the FFI widths, `Match`/`Response`) are RESERVED/NATIVE: they are NOT
    /// module-keyed (they keep their bare name, resolvable bare wherever the std module is imported), so
    /// the qualification pre-pass skips std modules exactly like the synthetic native ones.
    pub fn is_std(&self) -> bool {
        self.native.is_some()
            || self.dotted.first().map(String::as_str) == Some("std")
            // Path-aware: a std file checked/run AS THE ENTRY (`chezzi check std/foo.chz`) has an
            // empty `dotted` path, so the two checks above miss it — yet its body relies on stdlib
            // auto-privilege (e.g. bare `RwShared`/`Map` field types in std/concurrency/collection.chz).
            // Recognise it by its file location under `std_root()` so the entry `check`/`run` path
            // grants the same auto-license the import path does (no false "unknown type" diagnostics).
            || path_under_std_root(&self.id.0)
    }

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

/// ROOT REDESIGN — the canonical, per-module identity prefix used to qualify EVERY user
/// struct/enum/variant/type-alias runtime key (`<module-key>::<Name>`). The single source of truth
/// for all three engines (compiler, checker, interpreter), so they derive byte-identical keys.
///
/// Each module's key starts from its [`LoadedModule::label`] (the dotted path for an imported
/// module, the file stem for the entry — so the entry gets a real, non-empty key and a key is never
/// the malformed `::Name`). Because two modules' labels can collide (an entry file `geo.chz` whose
/// stem equals a one-segment import `geo`, or two like-named files in different dirs), any duplicate
/// label is made unique by appending `#<idx>` (the module's graph index). Graph order is identical
/// across every engine, so this tiebreak is deterministic — the parity invariant holds. Native
/// modules get a key too (harmless: they declare no user types, so it is never used to key one).
pub fn module_keys(graph: &ModuleGraph) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut keys = Vec::with_capacity(graph.modules.len());
    for (idx, lm) in graph.modules.iter().enumerate() {
        let label = lm.label();
        let count = seen.entry(label.clone()).or_insert(0);
        // First module with this label keeps the bare label; any later duplicate is disambiguated by
        // its graph index — deterministic because graph order is fixed across all engines.
        let key = if *count == 0 {
            label
        } else {
            format!("{label}#{idx}")
        };
        *count += 1;
        keys.push(key);
    }
    keys
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

/// Walk up from `start` itself (typically the current working directory) looking for `chezzi.toml`;
/// that dir is the project root. Returns `None` if no marker is found up to the filesystem root.
///
/// Unlike [`find_root`] (which begins at an entry file's *parent*), this begins AT `start` — the
/// no-file `chezzi run` case, where the project root may BE the cwd.
pub fn find_root_from_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("chezzi.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
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

/// Whether `p` lives under the stdlib directory ([`std_root`]). Used so a std file checked/run as the
/// ENTRY (with no dotted import path) is still recognised as stdlib. Canonicalizes both sides so a
/// relative entry path (`std/foo.chz`) and an absolute `std_root` compare correctly; a path that
/// fails to canonicalize (e.g. a synthetic/native id) is conservatively NOT under the std root.
fn path_under_std_root(p: &Path) -> bool {
    match (p.canonicalize(), std_root().canonicalize()) {
        (Ok(pc), Ok(rc)) => pc.starts_with(&rc),
        _ => false,
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
    build_graph_with_entry_source(entry, None)
}

/// Like [`build_graph`], but the **entry** module's source may be supplied directly (`Some`) instead
/// of being read from disk — every *imported* module still resolves from disk as usual. This lets the
/// LSP type-check the live, possibly-unsaved editor buffer while cross-module imports resolve against
/// the on-disk project, faithfully mirroring `chezzi check` (resolve → desugar → check_graph) and so
/// avoiding the single-module-check bare-key-vs-module-key pitfall. `None` reads the entry from disk
/// and is byte-for-byte equivalent to the old `build_graph`.
pub fn build_graph_with_entry_source(
    entry: &Path,
    entry_source: Option<String>,
) -> Result<ModuleGraph, ResolveError> {
    let entry_abs = abs(entry);
    let project_root = find_root(&entry_abs);
    let std_root = std_root();
    let entry_id = ModuleId(canonical_or_abs(&entry_abs));
    let entry_override = entry_source.map(|s| (entry_id.clone(), s));
    let mut b = Builder {
        project_root,
        std_root,
        visited: HashMap::new(),
        on_stack: Vec::new(),
        order: Vec::new(),
        entry_override,
        max_depth: MAX_IMPORT_DEPTH,
    };
    b.visit(&entry_id, &[], Span { line: 1, col: 1 })?;
    let mut graph = ModuleGraph {
        entry: entry_id,
        modules: b.order,
    };
    // Normalize named/default call arguments into positional ones, so the checker and both engines
    // consume an identical, already-desugared AST.
    crate::desugar::run(&mut graph)?;
    Ok(graph)
}

struct Builder {
    project_root: PathBuf,
    std_root: PathBuf,
    /// Fully emitted modules (dedup / run-once parse).
    visited: HashMap<ModuleId, ()>,
    /// Modules currently being processed (cycle detection), with their dotted labels.
    on_stack: Vec<(ModuleId, Vec<String>)>,
    order: Vec<LoadedModule>,
    /// If set, `(entry_id, source)` — the entry module reads from this string instead of disk.
    entry_override: Option<(ModuleId, String)>,
    /// Import-chain depth backstop (see [`MAX_IMPORT_DEPTH`]). Fielded so tests can inject a tiny
    /// limit and exercise the guard with trivial recursion (the cargo test-harness worker thread
    /// has a much smaller stack than the 8MB main thread — see `parser::MAX_DEPTH`).
    max_depth: usize,
}

impl Builder {
    fn visit(
        &mut self,
        id: &ModuleId,
        dotted: &[String],
        import_span: Span,
    ) -> Result<(), ResolveError> {
        // Cycle: this module is already on the active DFS stack.
        if let Some(pos) = self.on_stack.iter().position(|(sid, _)| sid == id) {
            let mut chain: Vec<String> = self.on_stack[pos..]
                .iter()
                .map(|(_, d)| dotted_label(d))
                .collect();
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
        // Depth backstop: a pathological acyclic-but-very-deep chain would otherwise recurse until
        // the host stack overflows and the process aborts (SIGABRT). Placed AFTER the cycle and
        // visited checks so a cycle at the limit still reports as a cycle and a deduped diamond node
        // returns early without tripping the guard — this bounds DEPTH (`on_stack.len()` = active
        // ancestors), not breadth. Attributed to the offending import like the cycle/missing arms.
        if self.on_stack.len() >= self.max_depth {
            let importer: Vec<String> = self
                .on_stack
                .last()
                .map(|(_, d)| d.clone())
                .unwrap_or_default();
            return Err(ResolveError {
                message: prefix(
                    &importer,
                    format!("import chain too deep (exceeds {})", self.max_depth),
                ),
                span: import_span,
                module: Some(dotted_label(dotted)),
            });
        }

        // The dotted path of the module that contains the failing `import` statement (empty for an
        // entry-level import). For the cannot-find-module path below, the TARGET is not yet on the
        // stack (its push is later), so `on_stack.last()` is the importing parent — exactly the
        // module whose `line N` the diagnostic refers to. Attributes the error like parse/type errors.
        let importer: Vec<String> = self
            .on_stack
            .last()
            .map(|(_, d)| d.clone())
            .unwrap_or_default();
        let source = match &self.entry_override {
            // The entry buffer is supplied in-memory (live LSP doc); imports still hit disk.
            Some((oid, osrc)) if oid == id => osrc.clone(),
            _ => std::fs::read_to_string(&id.0).map_err(|_| ResolveError {
                message: prefix(
                    &importer,
                    format!(
                        "cannot find module '{}' (looked for {})",
                        dotted_label(dotted),
                        id.0.display()
                    ),
                ),
                span: import_span,
                module: Some(dotted_label(dotted)),
            })?,
        };
        let ast = self.parse(&source, dotted)?;
        let imports = self.scan_imports(&ast);

        self.on_stack.push((id.clone(), dotted.to_vec()));
        let mut resolved = Vec::with_capacity(imports.len());
        for (import, span) in imports {
            let path = import_path(&import);
            // Native std modules (std.math/io/os) are virtual: no `.chz` file, members injected by
            // the engines. Bind them to a synthetic, stable id and skip the filesystem entirely.
            if let Some(name) = crate::native::native_name(&path) {
                let target = native_id(name);
                self.visit_native(&target, name);
                resolved.push(ResolvedImport {
                    target,
                    import,
                    span,
                });
                continue;
            }
            // A bare `import std` is not a real module: routing it through `module_file` yields the
            // std *directory* with a `.chz` extension (`<install>/std.chz`), so it both ignores any
            // project-local `std.chz` and leaks the internal install path. Reject it up front with a
            // filesystem-agnostic diagnostic. (Submodules like `std.math` / `std.x.y` are unaffected.)
            if path.len() == 1 && path.first().map(String::as_str) == Some("std") {
                // The current module is already on the stack (pushed above), so `on_stack.last()`
                // is the module whose import loop we are in — the importer.
                let importer: Vec<String> = self
                    .on_stack
                    .last()
                    .map(|(_, d)| d.clone())
                    .unwrap_or_default();
                return Err(ResolveError {
                    message: prefix(
                        &importer,
                        "'std' is a reserved namespace (import a submodule, e.g. 'std.math')"
                            .into(),
                    ),
                    span,
                    module: Some(dotted_label(&path)),
                });
            }
            let file = module_file(&path, &self.project_root, &self.std_root);
            let target = ModuleId(canonical_or_abs(&file));
            self.visit(&target, &path, span)?;
            resolved.push(ResolvedImport {
                target,
                import,
                span,
            });
        }
        self.on_stack.pop();

        self.visited.insert(id.clone(), ());
        self.order.push(LoadedModule {
            id: id.clone(),
            dotted: dotted.to_vec(),
            ast,
            imports: resolved,
            native: None,
        });
        Ok(())
    }

    /// Emit a native (virtual) std module: no file read, no recursion, deduped like any other.
    /// Its dotted path is the name split on `.` so [`LoadedModule::label`] reads `std.math`.
    fn visit_native(&mut self, id: &ModuleId, name: &'static str) {
        if self.visited.contains_key(id) {
            return;
        }
        self.visited.insert(id.clone(), ());
        self.order.push(LoadedModule {
            id: id.clone(),
            dotted: name.split('.').map(str::to_string).collect(),
            ast: Module { stmts: Vec::new() },
            imports: Vec::new(),
            native: Some(name),
        });
    }

    /// Lex + parse a module's source, wrapping failures with the module label (since `Span`
    /// carries no filename).
    fn parse(&self, source: &str, dotted: &[String]) -> Result<Module, ResolveError> {
        // Capture the doc-comment side-channel alongside the tokens, so the AST the checker/hover
        // sees carries each declaration's doc. The token stream is identical to `tokenize` — docs are
        // purely informational (LSP hover) and runtime-inert — so every other resolver behavior is
        // unchanged. This is the single graph seam that threads docs in.
        let (tokens, comments) =
            lexer::tokenize_with_comments(source).map_err(|e| ResolveError {
                message: prefix(dotted, e.to_string()),
                span: Span {
                    line: e.line,
                    col: 1,
                },
                module: opt_label(dotted),
            })?;
        parser::parse_with_docs(tokens, comments).map_err(|e| ResolveError {
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

/// A stable, distinct id for a native (virtual) std module — a sentinel path that can never collide
/// with a real file (it is never canonicalized / read).
fn native_id(name: &str) -> ModuleId {
    ModuleId(PathBuf::from(format!("<native:{name}>")))
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
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
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

    // 0a. The entry-source override is used verbatim instead of reading the entry from disk.
    #[test]
    fn entry_source_override_used() {
        // A path that does not exist on disk: only the override lets this resolve.
        let graph = build_graph_with_entry_source(
            Path::new("/nonexistent/chezzi_editor/x.chz"),
            Some("x = 1\n".into()),
        )
        .expect("override source should resolve without a disk read");
        // The entry module's AST came from the override (one assignment statement).
        let entry = graph.modules.last().unwrap();
        assert_eq!(entry.id, graph.entry);
        assert_eq!(entry.ast.stmts.len(), 1);
    }

    // 0b. With `None`, behavior is identical to build_graph (reads the entry from disk).
    #[test]
    fn override_none_equals_disk() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "y = 2\nz = y + 1\n");
        let a = build_graph(&entry).expect("disk build");
        let b = build_graph_with_entry_source(&entry, None).expect("delegated build");
        assert_eq!(a.entry, b.entry);
        assert_eq!(a.modules.len(), b.modules.len());
        assert_eq!(
            a.modules.last().unwrap().ast.stmts.len(),
            b.modules.last().unwrap().ast.stmts.len()
        );
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
        assert!(
            err.message.contains("a") && err.message.contains("b"),
            "got: {}",
            err.message
        );
    }

    // 6. A missing imported module is a clean error, not a panic.
    #[test]
    fn missing_module_is_clean_error() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "import nope.thing\nfn main(): print(1)\n");
        let err = build_graph(&entry).unwrap_err();
        assert!(
            err.message.contains("cannot find module"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("nope.thing"), "got: {}", err.message);
        // Entry-level import: no "in module" prefix (matches how type errors at entry level carry
        // no module attribution). Guards against over-prefixing once Bug-1 wraps with prefix().
        assert!(
            !err.message.contains("in module"),
            "entry-level import must not be module-prefixed, got: {}",
            err.message
        );
    }

    // 6b. A missing module imported from a NON-entry module names the importing module, so a
    // user invoking the entry can tell which file `line N` is in. (Bug 1.)
    #[test]
    fn missing_module_in_imported_module_names_importer() {
        let t = TmpDir::new();
        let main = t.write("main.chz", "import deep\nfn main(): print(1)\n");
        t.write(
            "deep.chz",
            "# pad\n# pad\n# pad\nimport ghost from doesnotexist\nfn f(): print(1)\n",
        );
        let err = build_graph(&main).unwrap_err();
        assert!(
            err.message.contains("in module 'deep'"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("cannot find module 'doesnotexist'"),
            "got: {}",
            err.message
        );
        assert_eq!(err.span.line, 4, "span should point at the bad import line");
    }

    // 6c. A bare `import std` inside a NON-entry module likewise names the importing module. (Bug 1.)
    #[test]
    fn bare_std_in_imported_module_names_importer() {
        let t = TmpDir::new();
        let main = t.write("main.chz", "import deep\nfn main(): print(1)\n");
        t.write("deep.chz", "import std\nfn f(): print(1)\n");
        let err = build_graph(&main).unwrap_err();
        assert!(
            err.message.contains("reserved namespace"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("in module 'deep'"),
            "got: {}",
            err.message
        );
    }

    // 7. A bare `import std` is a clear reserved-namespace diagnostic, NOT a confusing internal
    // install-path leak. A project-local `std.chz` next to main is intentionally ignored.
    #[test]
    fn bare_std_import_is_reserved_namespace() {
        let t = TmpDir::new();
        t.write("chezzi.toml", "");
        t.write("std.chz", "x = 1\n");
        let entry = t.write("main.chz", "import std\nfn main(): print(1)\n");
        let err = build_graph(&entry).unwrap_err();
        assert!(
            err.message.contains("reserved namespace"),
            "got: {}",
            err.message
        );
        // Must not leak the internal install path (`<crate>/std.chz`).
        assert!(!err.message.contains(".chz"), "got: {}", err.message);
    }

    // 8. A native std module (std.math) is virtual: resolved without any .chz file, flagged native.
    #[test]
    fn native_std_module_is_virtual() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "import std.math\nfn main(): print(1)\n");
        let graph = build_graph(&entry).unwrap();
        let m = graph
            .modules
            .iter()
            .find(|m| m.label() == "std.math")
            .expect("std.math should be in the graph");
        assert_eq!(m.native, Some("std.math"));
        assert!(m.ast.stmts.is_empty());
        // Dependencies precede dependents: the native module loads before the entry.
        assert_eq!(graph.modules.last().unwrap().id, graph.entry);
    }

    // 9. find_root_from_dir starts AT the given dir (cwd) and walks up to the chezzi.toml marker.
    #[test]
    fn find_root_from_dir_walks_up() {
        let t = TmpDir::new();
        t.write("chezzi.toml", "");
        let nested = t.path("sub/deep");
        std::fs::create_dir_all(&nested).unwrap();
        // From a nested dir we find the root.
        let root = find_root_from_dir(&nested).expect("should find root");
        assert_eq!(canonical_or_abs(&root), canonical_or_abs(&t.0));
        // From the root dir itself (the cwd case) we find it without going up.
        let root2 = find_root_from_dir(&t.0).expect("should find root at start dir");
        assert_eq!(canonical_or_abs(&root2), canonical_or_abs(&t.0));
    }

    // 10. No marker anywhere → None (so the caller can emit a clear "no chezzi.toml" error).
    #[test]
    fn find_root_from_dir_none_without_marker() {
        let t = TmpDir::new();
        let nested = t.path("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            find_root_from_dir(&nested).is_none(),
            "no marker → None, got Some"
        );
    }

    // 11. The user's verification concern: entrypoint imports resolve ROOT-relative, not
    //     file-relative. A project root with chezzi.toml + entrypoint = "src.main"; src/main.chz
    //     imports `lib` (at root) AND `src.utils.common`. All four modules must resolve.
    #[test]
    fn entrypoint_imports_are_root_relative() {
        let t = TmpDir::new();
        t.write("chezzi.toml", "[project]\nentrypoint = \"src.main\"\n");
        t.write("lib.chz", "fn helper(): print(1)\n");
        t.write(
            "src/main.chz",
            "import lib\nimport src.utils.common\nfn main(): print(1)\n",
        );
        t.write("src/utils/common.chz", "fn shared(): print(1)\n");

        // Resolve the entrypoint exactly like bare `chezzi run` will: dotted "src.main" → file.
        let root = t.0.clone();
        let entry = module_file(&["src".into(), "main".into()], &root, &std_root());
        assert_eq!(
            canonical_or_abs(&entry),
            canonical_or_abs(&t.path("src/main.chz"))
        );

        let graph = build_graph(&entry).unwrap();
        let labels: Vec<String> = graph.modules.iter().map(|m| m.label()).collect();
        // `import lib` resolves to <root>/lib.chz (root-relative, NOT src/lib.chz).
        let lib = graph
            .modules
            .iter()
            .find(|m| m.label() == "lib")
            .expect("lib must resolve");
        assert_eq!(
            canonical_or_abs(&lib.id.0),
            canonical_or_abs(&t.path("lib.chz")),
            "import lib must be root-relative; labels: {labels:?}"
        );
        // `import src.utils.common` resolves to <root>/src/utils/common.chz.
        let common = graph
            .modules
            .iter()
            .find(|m| m.label() == "src.utils.common")
            .expect("src.utils.common must resolve");
        assert_eq!(
            canonical_or_abs(&common.id.0),
            canonical_or_abs(&t.path("src/utils/common.chz")),
            "import src.utils.common must be root-relative; labels: {labels:?}"
        );
        assert!(labels.contains(&"lib".to_string()));
        assert!(labels.contains(&"src.utils.common".to_string()));
        assert_eq!(graph.modules.last().unwrap().id, graph.entry);
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
        assert_eq!(
            labels.iter().filter(|l| *l == "c").count(),
            1,
            "c loaded more than once: {labels:?}"
        );

        let pos = |name: &str| labels.iter().position(|l| l == name).unwrap();
        assert!(
            pos("c") < pos("a") && pos("c") < pos("b"),
            "deps before dependents: {labels:?}"
        );
        // Entry is last and has no dotted name.
        assert_eq!(graph.modules.last().unwrap().id, graph.entry);
    }

    // 12. A pathological acyclic-but-very-deep import chain is a clean diagnostic, not a host
    //     stack-overflow / SIGABRT. Tested at an injected tiny `max_depth` (8) over a short on-disk
    //     chain so the guard fires with trivial recursion — the real 2000 constant can only be
    //     exercised on the 8MB main thread (the test-harness worker stack is far smaller).
    fn deep_chain_builder(root: PathBuf, max_depth: usize) -> Builder {
        Builder {
            project_root: root,
            std_root: std_root(),
            visited: HashMap::new(),
            on_stack: Vec::new(),
            order: Vec::new(),
            entry_override: None,
            max_depth,
        }
    }

    #[test]
    fn deep_chain_guarded_not_crash() {
        let t = TmpDir::new();
        // Linear chain m0 -> m1 -> ... -> m11 (12 modules deep, safe to recurse on any thread).
        for k in 0..11 {
            t.write(
                &format!("m{k}.chz"),
                &format!("import m{}\nfn f(): print(1)\n", k + 1),
            );
        }
        let entry = t.write("m11.chz", "fn f(): print(1)\n");
        let entry_id = ModuleId(canonical_or_abs(&t.path("m0.chz")));
        let mut b = deep_chain_builder(t.0.clone(), 8);
        let err = b
            .visit(&entry_id, &[], Span { line: 1, col: 1 })
            .expect_err("a 12-deep chain must trip the depth-8 guard");
        assert!(
            err.message.contains("import chain too deep"),
            "got: {}",
            err.message
        );
        let _ = entry; // keep the temp file alive until here
    }

    // 12b. The depth-guard diagnostic is attributed to the offending import inside the importing
    //      module (non-{1,1} span + "in module" prefix), matching cycle/missing-module attribution.
    #[test]
    fn deep_chain_error_attributes_importer() {
        let t = TmpDir::new();
        for k in 0..11 {
            t.write(
                &format!("m{k}.chz"),
                &format!("import m{}\nfn f(): print(1)\n", k + 1),
            );
        }
        t.write("m11.chz", "fn f(): print(1)\n");
        let entry_id = ModuleId(canonical_or_abs(&t.path("m0.chz")));
        let mut b = deep_chain_builder(t.0.clone(), 8);
        let err = b
            .visit(&entry_id, &[], Span { line: 1, col: 1 })
            .unwrap_err();
        // The offending import lives inside a non-entry module, so it carries a module prefix and
        // the span of the `import` statement (line 1 of that module), never the entry's synthetic {1,1}.
        assert!(
            err.message.contains("in module"),
            "depth error must name the importing module, got: {}",
            err.message
        );
        assert_eq!(
            err.span.line, 1,
            "span should be the failing import statement"
        );
        assert!(
            err.module.is_some(),
            "depth error should carry the target dotted label"
        );
    }
}
