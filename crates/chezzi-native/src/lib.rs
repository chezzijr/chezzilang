#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! `chezzi-native`: the builtin and `std.*` native functions, the C FFI and the embedded stdlib
//! sources. Its files live under `src/native/`, included by path; every crate above re-exports
//! `native` at its old path.

pub use chezzi_syntax::{
    FRONTEND_STACK_BYTES, ast, fmtspec, lexer, on_frontend_stack, on_frontend_stack_scoped, parser,
    printable, slice,
};

#[path = "../../../src/native/mod.rs"]
pub mod native;

// Each crate's tests use a different part of the shared helpers.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../src/test_support.rs"]
pub(crate) mod test_support;
