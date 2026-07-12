//! Chezzi as a library — the compiler **front-end** and the engine, exposed as the crate of record.
//!
//! This is the real crate root: both binaries are thin shims that link this library. `src/main.rs`
//! (the `chezzi` CLI) and `src/bin/chezzi-lsp.rs` (the LSP server) declare no front-end modules of
//! their own — they `use chezzi::{…}`. The front-end therefore compiles **once**, and its module
//! unit tests + the two-engine VM/interp parity tests + the grammar `conformance` suite run **once**,
//! here in the lib's test target (a plain `cargo test` no longer double-compiles/double-runs them).
//!
//! NOTE for future CLI work: `src/main.rs` is the *bin* crate, so across the crate boundary it can
//! only see `pub` items of this lib (not `pub(crate)`). If a new CLI subcommand references a lib item,
//! that item must be `pub` here — mirror the existing `pub mod` front-end surface.

// The front-end modules are `pub` so every item the binaries reach across the crate boundary is
// visible, and so the modules' own dead-code analysis stays whole-crate. `editor` is the tooling layer
// consumed by `chezzi-lsp`; the rest are the compiler/VM pipeline driven by the `chezzi` CLI.
pub mod ast;
pub mod checker;
pub mod compiler;
pub mod desugar;
pub mod fmtspec;
pub mod interpolation;
pub mod json_decode;
pub mod lexer;
pub mod manifest;
pub mod native;
pub mod parser;
pub mod resolver;
pub mod runtime;
pub mod slice;
pub mod test_runner;
// `Obj::Generator` is a `pub` field wrapping the `pub(crate)` `GeneratorCore`; that is fine for the
// binary (whose `mod vm` is private) but trips `private_interfaces` once `vm` is `pub`. The VM API is
// not a real public surface here (only `editor` is), so allow it rather than widen `GeneratorCore`.
#[allow(private_interfaces)]
pub mod vm;

/// The only intended public surface: the editor-tooling layer consumed by `chezzi-lsp` and the asset
/// tests.
pub mod editor;

// The grammar-conformance suite (executes `docs/grammar.bnf`, differential-tests vs the parser). It is
// `#[cfg(test)]`-only and its `crate::lexer`/`crate::parser` refs resolve against this lib, so it lives
// here in the crate of record — running once via the lib test target (`cargo test conformance`).
#[cfg(test)]
mod conformance;

/// Stack size for the front-end (resolve → desugar → check → compile) thread.
///
/// The front-end walks the AST with several *recursive* tree-walkers — `desugar::walk_expr`, the
/// checker's type-inference walk, and the compiler's lowering — each recursing once per AST node.
/// A **deep-but-valid** AST therefore overflows the host stack (SIGABRT, uncatchable by `recover:`).
/// The parser's `MAX_DEPTH` recursion guard bounds *recursively*-parsed nesting, but left-associative
/// binary chains (`a + b + c …`) and postfix chains (`x.f.f…`, `a[0][0]…`, `f().g()…`) parse in
/// *iterative* loops, so they build a left-leaning AST far deeper than `MAX_DEPTH` without ever
/// tripping it (the parser's `MAX_CHAIN_DEPTH` cap bounds that depth, but well above a normal stack's
/// capacity). Running the front-end on its own large stack — mirroring the VM's dedicated
/// `VM_STACK_BYTES` thread — decouples front-end recursion depth from the *caller's* stack, which may
/// be small: the ~2 MiB LSP tokio worker (`editor::diagnostics`) or the test-harness worker, not just
/// the 8 MiB CLI main thread. Sized (1 GiB, virtual/lazily-committed) so the worst parser-accepted AST
/// depth — `MAX_DEPTH` (64) paren levels each nesting a `MAX_CHAIN_DEPTH` chain — fits with headroom on
/// a debug build (whose frames are far larger than release).
pub const FRONTEND_STACK_BYTES: usize = 1024 * 1024 * 1024;

/// Run a front-end pass on a dedicated [`FRONTEND_STACK_BYTES`] stack; see that constant for why.
/// The closure owns its inputs (`'static`) and returns the owned result across the join. A panic in
/// the pass is transparently re-raised on the caller's thread (never swallowed), so panic-as-bug
/// behaviour and the panic-fuzz invariant are preserved.
pub fn on_frontend_stack<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(FRONTEND_STACK_BYTES)
        .spawn(f)
        .expect("failed to spawn front-end stack thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
}
