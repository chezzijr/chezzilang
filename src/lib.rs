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
/// tripping it — that depth is what `parser::MAX_AST_DEPTH` bounds, well above a normal stack's
/// capacity. Running the front-end on its own large stack — mirroring the VM's dedicated
/// `VM_STACK_BYTES` thread — decouples front-end recursion depth from the *caller's* stack, which may
/// be small: the ~2 MiB LSP tokio worker (`editor::diagnostics`/`hover`/`semantic_tokens`) or the
/// test-harness worker, not just the 8 MiB CLI main thread.
///
/// **This 1 GiB is NOT the binding stack — `vm::VM_STACK_BYTES` (384 MiB) is.** `chezzi run` re-does
/// `build_graph` + `compile_graph` on the VM thread, so every parser depth constant is sized against
/// the smaller number; raising this one buys nothing on its own. Measured worst case at the shipped
/// constants (debug `chezzi run`, W7-50): ~16 000 AST nodes accepted against a ~33 000-node cliff.
/// The pre-W7-50 doc claimed the worst accepted AST was `MAX_DEPTH × MAX_CHAIN_DEPTH ≈ 64 × 500 ≈
/// 32 k`; that was wrong in both directions — the multiplicative fixture costs ~4 depth units per
/// level so its real ceiling was `15 × ~498 ≈ 7 500`, while the cheapest composing shape (nested
/// parens, ~2 units each, holding a 500-fold chain apiece) reached **15 000**. The product is now
/// bounded directly, by `MAX_AST_DEPTH`, instead of inferred from two constants that multiply.
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

/// [`on_frontend_stack`] for a closure that BORROWS instead of owning its inputs. `on_frontend_stack`
/// requires `F: 'static`, which is why its callers historically cloned everything they passed in —
/// fine for the CLI/editor (one `Module`/source string per call) but wrong to force on every checker
/// entry point, which would otherwise need to clone a whole `&ModuleGraph` per call just to get onto
/// the big stack. `std::thread::scope` + `Builder::spawn_scoped` lets the spawned thread borrow `'env`
/// data instead, so `f` can take `&Module` / `&ModuleGraph` directly. Same panic behaviour as
/// `on_frontend_stack`: a panic on the scoped thread is re-raised here via `resume_unwind`, never
/// swallowed — the panic-fuzz invariant does not get a second, quieter code path.
pub fn on_frontend_stack_scoped<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(FRONTEND_STACK_BYTES)
            .spawn_scoped(scope, f)
            .expect("failed to spawn front-end stack thread")
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}
