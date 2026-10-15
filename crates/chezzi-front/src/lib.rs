#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! `chezzi-front`: the front end — module resolution, desugaring, the checker and the compiler. It
//! turns source into a `base::op::Program` and never depends on the VM. Its files live under
//! `src/{resolver,desugar,checker,compiler}/`, included by path.

pub use chezzi_base::base;
pub use chezzi_native::native;
pub use chezzi_syntax::{
    FRONTEND_STACK_BYTES, ast, fmtspec, lexer, on_frontend_stack, on_frontend_stack_scoped, parser,
    printable, slice,
};

#[path = "../../../src/checker/mod.rs"]
pub mod checker;
#[path = "../../../src/compiler/mod.rs"]
pub mod compiler;
#[path = "../../../src/desugar/mod.rs"]
pub mod desugar;
#[path = "../../../src/resolver/mod.rs"]
pub mod resolver;

#[cfg(test)]
pub use chezzi_vm::vm;

// Each crate's tests use a different part of the shared helpers.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../src/test_support.rs"]
pub(crate) mod test_support;
