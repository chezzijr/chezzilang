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
// The tree-walk interpreter is compiled only under test (it is the `#[cfg(test)]`-only parity oracle,
// exactly as in `main.rs`). The front-end never references it in non-test code, so omitting it from
// release builds is sound; the `#[cfg(test)]` modules inside `vm`/`native` do reference it, so it must
// be present when the lib's own tests compile.
#[cfg(test)]
pub mod interp;
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
