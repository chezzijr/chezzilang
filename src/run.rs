//! run: the CLI's run drivers — resolve and compile with the front end, then run on the VM. They sit
//! above both layers, so the VM does not depend on the checker or the compiler.

use crate::native::HostConfig;
use crate::resolver::ModuleGraph;
use crate::vm::{RunOutput, RunOutputRaw, on_vm_stack, run_built, to_str_output};
use std::path::{Path, PathBuf};

/// Resolve, compile, and run a program from its entry path on the dedicated VM thread, then — if
/// `entry_fn` is `Some` — invoke that named top-level function of the entry module (the
/// `module:function` manifest entrypoint). `None` runs the module top-level only (scripting model).
/// This is the single entry the CLI's `chezzi run` uses.
///
/// `root` pins the module-graph root (the "one root per run" invariant): the bare-`chezzi run`
/// manifest path passes `Some(root)` — the manifest that declared the entrypoint, computed ONCE by
/// the CLI — so every `import` resolves against the SAME root that located the entry file. `None`
/// (the explicit `chezzi run FILE` path) derives the root by walking up from the entry file (nearest
/// marker, unchanged).
pub fn run_file_with_entry(
    entry: &Path,
    cfg: HostConfig,
    entry_fn: Option<&str>,
    root: Option<PathBuf>,
) -> RunOutput {
    to_str_output(run_file_bytes(entry, cfg, entry_fn, root))
}

/// [`run_file_with_entry`] without the lossy decode — the entry point for any caller that needs the
/// raw bytes (see [`RunOutputRaw`]), e.g. a byte-exact test assertion or the CPython differential.
pub fn run_file_bytes(
    entry: &Path,
    cfg: HostConfig,
    entry_fn: Option<&str>,
    root: Option<PathBuf>,
) -> RunOutputRaw {
    let entry = entry.to_path_buf();
    let entry_fn = entry_fn.map(str::to_string);
    on_vm_stack(move || {
        run_built(
            || crate::compiler::compile_entry_file(&entry, root),
            cfg,
            entry_fn.as_deref(),
            false,
        )
    })
}

/// TICKET-255 — run a graph the CLI already built and checked. `chezzi run` builds the graph one time
/// for its pre-run check and hands it here, so the prelude and every import parse once. The entry
/// source the CLI read from a one-shot fd (a pipe, `/dev/stdin`) is already inside the graph.
pub fn run_graph_with_entry(
    graph: ModuleGraph,
    cfg: HostConfig,
    entry_fn: Option<&str>,
) -> RunOutput {
    let entry_fn = entry_fn.map(str::to_string);
    to_str_output(on_vm_stack(move || {
        run_built(
            || crate::compiler::compile_graph(&graph),
            cfg,
            entry_fn.as_deref(),
            false,
        )
    }))
}
