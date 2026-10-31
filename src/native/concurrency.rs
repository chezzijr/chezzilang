//! `std.concurrency`'s callable members (TICKET-219, TICKET-289). The module's types (`Shared`,
//! `Executor`, ...) lower through the compiler's name->opcode dispatch and have no entry here.

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};

/// TICKET-219 — a std internal: seal `submit`'s fresh cap-1 channel `args[0]` with the job's
/// outcome `args[1]`. The first writer wins; a later seal changes nothing.
fn settle(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "_settle", 2)?;
    h.arg_settle_channel(0, 1)?;
    Ok(NativeRet::Nil)
}

/// TICKET-289 — a std internal: cancel the one job of Executor `args[0]` whose Task channel is
/// `args[1]` (`Task.cancel`).
fn cancel_job(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "_cancel_job", 2)?;
    h.arg_cancel_job(0, 1)?;
    Ok(NativeRet::Nil)
}

/// TICKET-219 — whether the handle channel `args[0]` holds its outcome: `Task.done()`. A sealed
/// channel stays sealed, so this never reads false after true.
fn is_settled(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_settled", 1)?;
    Ok(NativeRet::Bool(h.arg_channel_settled(0)?))
}

pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("_settle", settle, Kind::Inline),
    ("_cancel_job", cancel_job, Kind::Inline),
    ("is_settled", is_settled, Kind::Inline),
];
