//! `std.concurrency`'s callable members (TICKET-191, TICKET-213, TICKET-219). The module's types (`Shared`, `Executor`, ...)
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

fn mark_task_copy(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "mark_task_copy", 1)?;
    h.arg_mark_task_copy(0)?;
    Ok(NativeRet::Nil)
}

/// TICKET-219 — a std internal: seal `submit_result`'s fresh cap-1 channel `args[0]` with the job's
/// outcome `args[1]`. The first writer wins; a later seal changes nothing.
fn settle(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "_settle", 2)?;
    h.arg_settle_channel(0, 1)?;
    Ok(NativeRet::Nil)
}

/// TICKET-219 — whether the handle channel `args[0]` holds its outcome: `Task.done()`. A sealed
/// channel stays sealed, so this never reads false after true.
fn is_settled(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_settled", 1)?;
    Ok(NativeRet::Bool(h.arg_channel_settled(0)?))
}

pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("is_task_copy", is_task_copy, Kind::Inline),
    ("mark_task_copy", mark_task_copy, Kind::Inline),
    ("_settle", settle, Kind::Inline),
    ("is_settled", is_settled, Kind::Inline),
];
