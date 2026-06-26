//! Chezzi as a library — the compiler **front-end** exposed for in-process tooling.
//!
//! This is an *additive* crate root that lives alongside the `chezzi` binary (`src/main.rs`). The
//! binary keeps its own private `mod` declarations untouched (including the delicate `#[cfg(test)]
//! mod interp` two-engine parity wiring); this `lib.rs` re-declares the same front-end module set as
//! `pub mod`s so the `chezzi-lsp` binary and the editor-tooling tests can reach the lexer / parser /
//! checker / resolver in-process. The front-end therefore compiles twice (once for the bin, once for
//! the lib); that is deliberate and safe — the crate exports no `#[no_mangle]`/`export_name` symbols,
//! so there is nothing to clash.
//!
//! Only `editor` (the new tooling layer) is meant to be consumed directly; the rest are re-exposed
//! purely so `editor` can wrap them.

// The front-end modules are re-declared `pub` (matching `main.rs`'s private set) so that every item
// counts as reachable — keeping the modules' own dead-code analysis identical to the binary's, rather
// than flagging the (intentionally) unused-by-`editor` compiler/VM API. The only consumer of this lib
// is `editor`; the rest are exposed solely so it can wrap them.
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
