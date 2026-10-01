//! `std.concurrency`'s one callable member (TICKET-191). The module's types (`Shared`, `Executor`, ...)
//! lower through the compiler's name->opcode dispatch and have no entry here.
//!
//! `is_task_copy(v)` reads the airlock's own predicate, `Heap::is_copied`, the one D4's write check
//! (`Vm::check_copied_write`) reads. It lets std code (`std.concurrency.task`) choose between writing a
//! value and reading through shared state without attempting the write and catching the fault.

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};

fn is_task_copy(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_task_copy", 1)?;
    Ok(NativeRet::Bool(h.arg_is_task_copy(0)?))
}

pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[("is_task_copy", is_task_copy, Kind::Inline)];
