#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]
//! `chezzi-base`: what both the front end and the VM read (bytecode, hooks, message words, module
//! identity). Its files live under `src/base/`, included by path.

pub use chezzi_native::native;
pub use chezzi_syntax::{
    FRONTEND_STACK_BYTES, ast, fmtspec, lexer, on_frontend_stack, on_frontend_stack_scoped, parser,
    printable, slice,
};

#[path = "../../../src/base/mod.rs"]
pub mod base;
