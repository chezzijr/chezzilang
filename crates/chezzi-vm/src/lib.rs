#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! `chezzi-vm`: the bytecode VM on its M:N scheduler. It runs a `base::op::Program` and never
//! depends on the front end: a driver hands it the program (`vm::run_built`). Its files live under
//! `src/vm/`, included by path.

pub use chezzi_base::base;
pub use chezzi_native::native;
pub use chezzi_syntax::{
    FRONTEND_STACK_BYTES, ast, fmtspec, lexer, on_frontend_stack, on_frontend_stack_scoped, parser,
    printable, slice,
};

// `Obj::Generator` is a `pub` field wrapping the `pub(crate)` `GeneratorCore`; the VM API is not a
// real public surface, so allow it rather than widen `GeneratorCore`.
#[allow(private_interfaces)]
#[path = "../../../src/vm/mod.rs"]
pub mod vm;

// The VM's tests compile the programs they run, through the front end (a dev-dependency).
#[cfg(test)]
pub use chezzi_front::{checker, compiler, desugar, resolver};

// Each crate's tests use a different part of the shared helpers.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../src/test_support.rs"]
pub(crate) mod test_support;
