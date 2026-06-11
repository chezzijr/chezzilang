//! Runtime constants and helpers shared verbatim by BOTH execution engines (the tree-walk
//! interpreter in `crate::interp` and the bytecode VM in `crate::vm`). Anything that must be
//! byte-identical across the cooperative VM, the `--parallel` VM, and the interpreter — the parity
//! oracle — lives here so there is a single source of truth.

/// The one-line observable report emitted when a `parallel:` body escapes early (`?` / `return` /
/// `break`) before its join, cancelling the `n` already-`spawn`ed-but-not-yet-started tasks.
///
/// Policy (cancel-and-report): the unstarted tasks are NOT run — they are dropped, the same
/// end-state a started sibling reaches under B3.4 when a sibling faults first — and this single line
/// is written to the engine's stdout sink (`out`), the stream every `run_capture*` parity harness
/// reads. Emitted only when `n >= 1`. The trailing newline makes it a clean line in captured output.
pub fn pending_cancel_report(n: usize) -> String {
    format!("{n} pending task(s) cancelled on early exit from parallel:\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_is_byte_stable() {
        assert_eq!(
            pending_cancel_report(2),
            "2 pending task(s) cancelled on early exit from parallel:\n"
        );
        assert_eq!(
            pending_cancel_report(1),
            "1 pending task(s) cancelled on early exit from parallel:\n"
        );
    }
}
