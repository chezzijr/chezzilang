#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! Chezzi as a library — the compiler **front-end** and the engine, exposed as the crate of record.
//!
//! This is the real crate root: both binaries are thin shims that link this library. `src/main.rs`
//! (the `chezzi` CLI) and `src/bin/chezzi-lsp.rs` (the LSP server) declare no front-end modules of
//! their own — they `use chezzi::{…}`. The front-end therefore compiles **once**, and its module
//! unit tests + the golden VM tests + the grammar `conformance` suite run **once**,
//! here in the lib's test target (a plain `cargo test` no longer double-compiles/double-runs them).
//!
//! NOTE for future CLI work: `src/main.rs` is the *bin* crate, so across the crate boundary it can
//! only see `pub` items of this lib (not `pub(crate)`). If a new CLI subcommand references a lib item,
//! that item must be `pub` here — mirror the existing `pub mod` front-end surface.

// The front-end modules are `pub` so every item the binaries reach across the crate boundary is
// visible, and so the modules' own dead-code analysis stays whole-crate. `editor` is the tooling layer
// consumed by `chezzi-lsp`; the rest are the compiler/VM pipeline driven by the `chezzi` CLI.
/// `println!` / `eprintln!` / `print!` for the CLI's own messages (diagnostics, `ok: no type errors`,
/// usage). A closed pipe (`chezzi check f.chz 2>&1 | head -1`) drops the write instead of panicking
/// (rc=101); the command keeps its own exit code, as `go vet` does. A program's own output does not
/// come here: it goes through the VM's `emit_out` / `stream_halt` path.
#[macro_export]
macro_rules! outln {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stdout(), $($t)*);
    }};
}
/// See [`outln!`]; stderr.
#[macro_export]
macro_rules! errln {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}
/// See [`outln!`]; no trailing newline.
#[macro_export]
macro_rules! out {
    ($($t:tt)*) => {{
        use std::io::Write as _;
        let _ = write!(std::io::stdout(), $($t)*);
    }};
}

pub mod manifest;
pub mod run;
pub mod test_runner;

/// The only intended public surface: the editor-tooling layer consumed by `chezzi-lsp` and the asset
/// tests.
pub mod editor;

// The grammar-conformance suite (executes `docs/reference/grammar.bnf`, differential-tests vs the parser). It is
// `#[cfg(test)]`-only and its `crate::lexer`/`crate::parser` refs resolve against this lib, so it lives
// here in the crate of record — running once via the lib test target (`cargo test conformance`).
#[cfg(test)]
mod conformance;

// The shared unit-test helpers (`TmpDir`). Each crate's tests use a different part of them.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod test_support;

// Each layer is its own crate, so an edit in one never recompiles a layer it does not reach:
// `chezzi-syntax` (lexer, AST, parser), `chezzi-native` (natives, FFI, embedded std), `chezzi-base`
// (bytecode, hooks, crossing marks), `chezzi-front` (resolver, desugar, checker, compiler) and
// `chezzi-vm`. Their sources stay under `src/`, and every `crate::<module>::…` path in this crate keeps
// resolving through these re-exports.
pub use chezzi_base::base;
pub use chezzi_front::{checker, compiler, desugar, resolver};
pub use chezzi_native::native;
pub use chezzi_syntax::{
    FRONTEND_STACK_BYTES, ast, fmtspec, lexer, on_frontend_stack, on_frontend_stack_scoped, parser,
    printable, slice,
};
pub use chezzi_vm::vm;
