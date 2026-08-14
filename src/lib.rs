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
/// the smaller number; raising this one buys nothing on its own.
///
/// **W7-50 NARROWED the worst accepted AST, deliberately.** The pre-W7-50 doc estimated it as
/// `MAX_DEPTH × MAX_CHAIN_DEPTH ≈ 64 × 500 ≈ 32 k`, and that estimate was very nearly right:
/// measured on `b1307258`, `x := ` + 30 × `( … .f×499 +1×499 )` parses — ~**29 940** nodes, because a
/// paren level can spend BOTH fold loops (`parse_postfix`'s and `parse_bp`'s) at ~998 nodes per level.
/// Against the measured ~33 100-node walker cliff that is **1.11×** headroom: the parser was accepting
/// programs within ~10% of an uncatchable host stack overflow. `parser::MAX_AST_DEPTH` = 16 000 now
/// bounds the depth directly instead of as a product of two constants, and the worst accepted AST is
/// ~**15 968** nodes — **2.07×** headroom. Programs between those two depths no longer parse; they get
/// a clean `too deeply` diagnostic where they previously ran ~10% short of a SIGABRT. (Two earlier
/// writeups put the old ceiling at 7 500 and at 15 000 and called the change a no-regression; both
/// undercounted, having measured shapes that spend only one fold loop per level.)
///
/// **Both numbers hold across the interpolation re-parse only because a SECOND enforcement point
/// exists.** A `Parser` bounds the tree *it* builds, and an interpolated `{…}` fragment is built by
/// another one, so the budgets used to compose (measured: three nested levels type-checked clean at
/// ~46 000 nodes and SIGABRTed debug `chezzi run`). `desugar::Walker::walk_expr` re-enters its own
/// walk on a parsed fragment's subtree, so its recursion depth is the composed tree's depth, and it
/// refuses at `MAX_AST_DEPTH` too. That is now unconditional: the one seam it did not cover — an
/// interpolated literal inside a default argument spliced on `desugar`'s *second* pass, never
/// walked, reaching ~31 986 nodes — closed with W7-51, which deleted both the second pass and the
/// spliced default *expression*. Re-measured on `ed4830b3` with a probe on
/// `checker::check_interpolation`'s success arm: `925dd0f7` 1 hit / peak walk depth 15 995,
/// `ed4830b3` **0 hits** / peak 15 994. See that method and `parser::MAX_AST_DEPTH`.
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
