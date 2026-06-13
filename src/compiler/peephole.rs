//! Peephole optimizer (M19 perf): a single relocating pass over a proto's flat instruction
//! stream, run by [`super::Compiler::finish`] just before the `Proto` is sealed.
//!
//! Jump targets in our bytecode are **absolute** indices into `code`, so any rule that removes or
//! fuses instructions has to renumber every jump. This pass does that in one shot: it builds the
//! optimized `code`/`lines` alongside an `old → new` index map, then rewrites all jump operands
//! through the map at the end.
//!
//! **Legality rule:** a multi-op window may only be rewritten if none of its *interior* positions
//! is a jump target. The window's first position may be a target — a jump landing there simply
//! lands on the fused/folded op, which is exactly equivalent. Interior positions that were jump
//! targets would otherwise be silently redirected, so we refuse those windows.
//!
//! Every rewrite must be **observably identical** to running the original ops on the VM: constant
//! folding replicates `Vm::arith`'s checked semantics (overflow / divide-by-zero are *not* folded
//! — they are left for the runtime to raise the same error).

use crate::ast::Span;
use crate::vm::op::{BinKind, Op};

/// Map a binary-operator op to its `BinKind`, or `None` if it isn't a fusable binop. `Eq`/`NotEq`
/// are deliberately excluded (they run a different VM path).
fn bin_kind(op: &Op) -> Option<BinKind> {
    Some(match op {
        Op::Add => BinKind::Add,
        Op::Sub => BinKind::Sub,
        Op::Mul => BinKind::Mul,
        Op::Div => BinKind::Div,
        Op::Mod => BinKind::Mod,
        Op::Lt => BinKind::Lt,
        Op::LtEq => BinKind::LtEq,
        Op::Gt => BinKind::Gt,
        Op::GtEq => BinKind::GtEq,
        _ => return None,
    })
}

/// Read a jump-like op's absolute target (the same op set `Compiler::patch_jump` writes). `None`
/// for non-jump ops. `MatchArm::next` is a jump target; its `scrut`/`bind_start` are slots, not.
fn jump_target(op: &Op) -> Option<usize> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfFalseKeep(t)
        | Op::JumpIfTrueKeep(t)
        | Op::PushHandler(t)
        | Op::MatchArm { next: t, .. } => Some(*t),
        _ => None,
    }
}

/// Mutable view of a jump-like op's absolute target, for relocation.
fn jump_target_mut(op: &mut Op) -> Option<&mut usize> {
    match op {
        Op::Jump(t)
        | Op::JumpIfFalse(t)
        | Op::JumpIfFalseKeep(t)
        | Op::JumpIfTrueKeep(t)
        | Op::PushHandler(t)
        | Op::MatchArm { next: t, .. } => Some(t),
        _ => None,
    }
}

/// A tail rewrite: replace the last `window` ops of the output stream with `op`.
struct Fold {
    op: Op,
    window: usize,
}

/// If the tail of `out` is a constant expression the VM would evaluate to a single constant,
/// return its folded form. Replicates `Vm::arith` / `Vm::step`'s `Neg`/`Not` semantics **exactly**:
/// overflow and divide-by-zero are *not* folded (they stay in the stream so the runtime raises the
/// identical error). Mixed int/float pairs are left alone.
fn try_fold_tail(out: &[Op]) -> Option<Fold> {
    let m = out.len();
    // ----- binary: [Const, Const, <arith>] -----
    if m >= 3 {
        let arith = matches!(
            out[m - 1],
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod
        );
        if arith {
            match (&out[m - 3], &out[m - 2]) {
                (Op::ConstInt(a), Op::ConstInt(b)) => {
                    let (a, b) = (*a, *b);
                    let r = match &out[m - 1] {
                        Op::Add => a.checked_add(b),
                        Op::Sub => a.checked_sub(b),
                        Op::Mul => a.checked_mul(b),
                        Op::Div if b == 0 => None,
                        Op::Mod if b == 0 => None,
                        Op::Div => a.checked_div(b),
                        Op::Mod => a.checked_rem(b),
                        _ => unreachable!(),
                    };
                    // `None` ⇒ overflow or div/mod-by-zero: do NOT fold (leave the runtime error).
                    if let Some(v) = r {
                        return Some(Fold { op: Op::ConstInt(v), window: 3 });
                    }
                }
                (Op::ConstFloat(a), Op::ConstFloat(b)) => {
                    let (a, b) = (*a, *b);
                    // VM raises "by zero" for float Div/Mod with a zero divisor — don't fold those.
                    let r = match &out[m - 1] {
                        Op::Add => Some(a + b),
                        Op::Sub => Some(a - b),
                        Op::Mul => Some(a * b),
                        Op::Div if b == 0.0 => None,
                        Op::Mod if b == 0.0 => None,
                        Op::Div => Some(a / b),
                        Op::Mod => Some(a % b),
                        _ => unreachable!(),
                    };
                    if let Some(v) = r {
                        return Some(Fold { op: Op::ConstFloat(v), window: 3 });
                    }
                }
                _ => {}
            }
        }
    }
    // ----- unary: [Const, Neg|Not] -----
    if m >= 2 {
        match (&out[m - 2], &out[m - 1]) {
            (Op::ConstInt(n), Op::Neg) => {
                // `Vm::Neg` uses `checked_neg`; i64::MIN overflows — leave it for the runtime.
                if let Some(v) = n.checked_neg() {
                    return Some(Fold { op: Op::ConstInt(v), window: 2 });
                }
            }
            (Op::ConstFloat(x), Op::Neg) => {
                return Some(Fold { op: Op::ConstFloat(-x), window: 2 });
            }
            (Op::True, Op::Not) => return Some(Fold { op: Op::False, window: 2 }),
            (Op::False, Op::Not) => return Some(Fold { op: Op::True, window: 2 }),
            _ => {}
        }
    }
    None
}

/// Fuse the output tail into a superinstruction, if it matches a hot window. Tried after constant
/// folding. Order matters: the 3-op binop fusions fire when the operator is appended, then the
/// 2-op `IncLocal` collapse fires when the following `SetLocal` arrives.
fn try_fuse_tail(out: &[Op]) -> Option<Fold> {
    let m = out.len();
    // ----- IncLocal: [BinLocalConst{slot, val, Add}, SetLocal(slot)] → IncLocal{slot, val} -----
    if m >= 2
        && let Op::SetLocal(s2) = out[m - 1]
        && let Op::BinLocalConst { slot, val, kind: BinKind::Add } = out[m - 2]
        && slot == s2
    {
        return Some(Fold { op: Op::IncLocal { slot, delta: val }, window: 2 });
    }
    // ----- BinLocalLocal: [GetLocal(a), GetLocal(b), <binop>] -----
    if m >= 3
        && let Some(kind) = bin_kind(&out[m - 1])
        && let Op::GetLocal(a) = out[m - 3]
        && let Op::GetLocal(b) = out[m - 2]
    {
        return Some(Fold { op: Op::BinLocalLocal { a, b, kind }, window: 3 });
    }
    // ----- BinLocalConst: [GetLocal(slot), ConstInt(val), <binop>] -----
    if m >= 3
        && let Some(kind) = bin_kind(&out[m - 1])
        && let Op::GetLocal(slot) = out[m - 3]
        && let Op::ConstInt(val) = out[m - 2]
    {
        return Some(Fold { op: Op::BinLocalConst { slot, val, kind }, window: 3 });
    }
    None
}

/// Run the peephole pass over one proto body. Returns the optimized `(code, lines)`.
///
/// One forward walk copies each op into `out`, then collapses the output tail while it forms a
/// foldable window (so `1 + 2 + 3` folds fully). Old→new indices are tracked in `map`; jump
/// operands are relocated through it at the end. A fold is refused if any *interior* boundary of
/// its window was a jump target (a jump to the window's first op is fine — it lands on the result).
pub fn optimize(code: Vec<Op>, lines: Vec<Span>) -> (Vec<Op>, Vec<Span>) {
    let n = code.len();
    if n == 0 {
        return (code, lines);
    }

    // Absolute jump targets in the *original* stream (indices into `code`, may equal `n`).
    let mut is_target = vec![false; n + 1];
    for op in &code {
        if let Some(t) = jump_target(op)
            && t <= n
        {
            is_target[t] = true;
        }
        // `WaitPoll` carries N arm targets + an optional `else` target (all absolute jump targets);
        // each marks an entry point a fold must not erase, exactly like a `Jump` target.
        if let Op::WaitPoll(m) = op {
            for &t in m.arm_targets.iter().chain(m.else_target.iter()) {
                if t <= n {
                    is_target[t] = true;
                }
            }
        }
    }

    let mut out: Vec<Op> = Vec::with_capacity(n);
    let mut out_lines: Vec<Span> = Vec::with_capacity(n);
    let mut src_start: Vec<usize> = Vec::with_capacity(n); // old start index per out entry
    let mut map = vec![0usize; n + 1]; // old index → new index

    for i in 0..n {
        map[i] = out.len();
        out.push(code[i].clone());
        out_lines.push(lines[i]);
        src_start.push(i);

        // Collapse the output tail as long as it folds/fuses and the rewrite is legal.
        while let Some(fold) = try_fold_tail(&out).or_else(|| try_fuse_tail(&out)) {
            let m = out.len();
            let first = m - fold.window;
            // Interior boundaries (every folded entry after the first) must not be jump targets.
            if !(first + 1..m).all(|p| !is_target[src_start[p]]) {
                break;
            }
            let old_lo = src_start[first];
            let new_idx = first;
            map[old_lo..=i].fill(new_idx);
            let span = out_lines[m - 1]; // operator's span
            out.truncate(first);
            out_lines.truncate(first);
            src_start.truncate(first);
            out.push(fold.op);
            out_lines.push(span);
            src_start.push(old_lo);
        }
    }
    map[n] = out.len(); // forward jumps to one-past-end

    for op in &mut out {
        if let Some(t) = jump_target_mut(op) {
            *t = map[*t];
        }
        // Relocate `WaitPoll`'s arm + else targets through the same `map` (they are absolute jump
        // targets the VM dispatches to in `take_wait_arm` / the `else` jump).
        if let Op::WaitPoll(m) = op {
            for t in &mut m.arm_targets {
                *t = map[*t];
            }
            if let Some(t) = &mut m.else_target {
                *t = map[*t];
            }
        }
    }
    (out, out_lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span { line: 1, col: 1 }
    }

    fn opt(code: Vec<Op>) -> Vec<Op> {
        let lines = vec![span(); code.len()];
        let (out, out_lines) = optimize(code, lines);
        assert_eq!(out_lines.len(), out.len(), "code/lines must stay parallel");
        out
    }

    #[test]
    fn folds_int_add() {
        let out = opt(vec![Op::ConstInt(2), Op::ConstInt(3), Op::Add]);
        assert_eq!(out.len(), 1, "expected fold to one op, got {out:?}");
        assert!(matches!(out[0], Op::ConstInt(5)), "got {:?}", out[0]);
    }

    #[test]
    fn folds_cascade_left_assoc() {
        // 1 + 2 + 3 → ConstInt(1),ConstInt(2),Add,ConstInt(3),Add → ConstInt(6)
        let out = opt(vec![
            Op::ConstInt(1),
            Op::ConstInt(2),
            Op::Add,
            Op::ConstInt(3),
            Op::Add,
        ]);
        assert_eq!(out.len(), 1, "expected full cascade fold, got {out:?}");
        assert!(matches!(out[0], Op::ConstInt(6)), "got {:?}", out[0]);
    }

    #[test]
    fn relocates_jump_past_a_fold() {
        // [CI2, CI3, Add, Jump(5), Pop, Return]; fold removes 2 ops, so Return moves 5→3 and the
        // Jump operand must be renumbered to land on it.
        let out = opt(vec![
            Op::ConstInt(2),
            Op::ConstInt(3),
            Op::Add,
            Op::Jump(5),
            Op::Pop,
            Op::Return,
        ]);
        assert_eq!(out.len(), 4, "fold should drop 2 ops, got {out:?}");
        assert!(matches!(out[0], Op::ConstInt(5)));
        assert!(matches!(out[1], Op::Jump(3)), "jump must relocate to 3, got {:?}", out[1]);
        assert!(matches!(out[3], Op::Return));
    }

    /// §6d regression — `WaitPoll`'s `arm_targets`/`else_target` are absolute jump targets and MUST
    /// be relocated like `Jump`. A fold before the arm bodies shifts their indices; without
    /// relocation the arm target lands one or more ops past the bind prologue (the cooperative `wait`
    /// arm-body parity bug: VM 65 vs interp 66). `[CI2, CI3, Add, WaitPoll{arms:[5,7]}, body@5..]` —
    /// the fold removes 2 ops so the arm bodies move 5→3 and 7→5.
    fn waitpoll(arm_targets: Vec<usize>, else_target: Option<usize>) -> Op {
        Op::WaitPoll(Box::new(crate::vm::op::WaitMeta { n: 1, arm_targets, else_target }))
    }

    #[test]
    fn relocates_waitpoll_arm_and_else_targets_past_a_fold() {
        let out = opt(vec![
            Op::ConstInt(2),
            Op::ConstInt(3),
            Op::Add,                       // [0..2] fold to one ConstInt(5)
            waitpoll(vec![5, 7], Some(9)), // arm/else absolute targets in the ORIGINAL stream
            Op::Nil,                       // [4]
            Op::SetLocal(0),               // [5] arm 0 body
            Op::Return,                    // [6]
            Op::SetLocal(1),               // [7] arm 1 body
            Op::Return,                    // [8]
            Op::Pop,                       // [9] else body
            Op::Return,                    // [10]
        ]);
        // The fold removes 2 ops, so every index ≥3 shifts down by 2: 5→3, 7→5, 9→7.
        match &out[1] {
            Op::WaitPoll(m) => {
                assert_eq!(m.arm_targets, vec![3, 5], "arm targets must relocate, got {:?}", m.arm_targets);
                assert_eq!(m.else_target, Some(7), "else target must relocate, got {:?}", m.else_target);
            }
            other => panic!("expected WaitPoll at out[1], got {other:?}"),
        }
    }

    #[test]
    fn does_not_fold_across_interior_waitpoll_arm_target() {
        // A WaitPoll arm target pointing at the interior of a foldable window must block the fold
        // (folding would erase that entry point), exactly like an interior `Jump` target.
        let out = opt(vec![
            waitpoll(vec![2], None), // [0] arm 0 targets index 2 (the interior `ConstInt(3)`)
            Op::ConstInt(2),         // [1]
            Op::ConstInt(3),         // [2] ← arm target — folding [1,2,3] would erase it
            Op::Add,                 // [3]
            Op::Return,              // [4]
        ]);
        assert!(matches!(out[3], Op::Add), "fold must be refused, got {out:?}");
        match &out[0] {
            Op::WaitPoll(m) => assert_eq!(m.arm_targets, vec![2], "interior target unchanged, got {:?}", m.arm_targets),
            other => panic!("expected WaitPoll, got {other:?}"),
        }
    }

    #[test]
    fn does_not_fold_across_interior_jump_target() {
        // Jump(1) targets the *interior* second const — folding would erase that entry point.
        let out = opt(vec![Op::ConstInt(2), Op::ConstInt(3), Op::Add, Op::Jump(1)]);
        assert_eq!(out.len(), 4, "must refuse the fold, got {out:?}");
        assert!(matches!(out[2], Op::Add));
        assert!(matches!(out[3], Op::Jump(1)), "target unchanged, got {:?}", out[3]);
    }

    #[test]
    fn does_not_fold_on_int_overflow() {
        let out = opt(vec![Op::ConstInt(i64::MAX), Op::ConstInt(1), Op::Add]);
        assert_eq!(out.len(), 3, "overflow must stay unfolded for the runtime error");
    }

    #[test]
    fn does_not_fold_int_div_by_zero() {
        let out = opt(vec![Op::ConstInt(1), Op::ConstInt(0), Op::Div]);
        assert_eq!(out.len(), 3, "div-by-zero must stay unfolded");
    }

    #[test]
    fn folds_unary_neg_and_not() {
        let out = opt(vec![Op::ConstInt(5), Op::Neg]);
        assert!(matches!(out.as_slice(), [Op::ConstInt(-5)]), "got {out:?}");
        let out = opt(vec![Op::True, Op::Not]);
        assert!(matches!(out.as_slice(), [Op::False]), "got {out:?}");
    }

    #[test]
    fn does_not_fold_neg_i64_min() {
        let out = opt(vec![Op::ConstInt(i64::MIN), Op::Neg]);
        assert_eq!(out.len(), 2, "negating i64::MIN overflows — leave for runtime");
    }

    #[test]
    fn folds_float_mul() {
        let out = opt(vec![Op::ConstFloat(2.0), Op::ConstFloat(3.0), Op::Mul]);
        assert!(matches!(out.as_slice(), [Op::ConstFloat(v)] if *v == 6.0), "got {out:?}");
    }

    #[test]
    fn fuses_inc_local() {
        // `i += 1` → GetLocal,ConstInt,Add,SetLocal → BinLocalConst then collapse to IncLocal.
        let out = opt(vec![Op::GetLocal(1), Op::ConstInt(1), Op::Add, Op::SetLocal(1)]);
        assert!(
            matches!(out.as_slice(), [Op::IncLocal { slot: 1, delta: 1 }]),
            "got {out:?}"
        );
    }

    #[test]
    fn does_not_inc_when_load_store_slots_differ() {
        // `total += i` (load 0, load 1, add, store 0) is NOT an IncLocal — only a BinLocalLocal + store.
        let out = opt(vec![Op::GetLocal(0), Op::GetLocal(1), Op::Add, Op::SetLocal(0)]);
        assert!(
            matches!(
                out.as_slice(),
                [Op::BinLocalLocal { a: 0, b: 1, kind: BinKind::Add }, Op::SetLocal(0)]
            ),
            "got {out:?}"
        );
    }

    #[test]
    fn fuses_bin_local_local() {
        let out = opt(vec![Op::GetLocal(1), Op::GetLocal(1), Op::Mul]);
        assert!(
            matches!(out.as_slice(), [Op::BinLocalLocal { a: 1, b: 1, kind: BinKind::Mul }]),
            "got {out:?}"
        );
    }

    #[test]
    fn fuses_bin_local_const_compare() {
        let out = opt(vec![Op::GetLocal(0), Op::ConstInt(2), Op::Lt]);
        assert!(
            matches!(out.as_slice(), [Op::BinLocalConst { slot: 0, val: 2, kind: BinKind::Lt }]),
            "got {out:?}"
        );
    }

    #[test]
    fn does_not_fuse_eq_into_binop() {
        // Eq is not a fusable BinKind — it stays as ConstInt then Eq.
        let out = opt(vec![Op::GetLocal(0), Op::ConstInt(0), Op::Eq]);
        assert_eq!(out.len(), 3, "Eq must not fuse, got {out:?}");
    }

    #[test]
    fn does_not_fuse_across_interior_jump_target() {
        // Jump(2) targets the operator — fusing would erase that entry point.
        let out = opt(vec![Op::GetLocal(0), Op::ConstInt(2), Op::Lt, Op::Jump(2)]);
        assert_eq!(out.len(), 4, "fusion must be refused, got {out:?}");
        assert!(matches!(out[2], Op::Lt));
    }
}
