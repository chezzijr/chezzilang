//! Checker: the one table of what may cross a `spawn:`, `defer:` or `recover:` block, or sit in a
//! generator body, and the stack of blocks the walk is inside.

use super::*;

/// A block that limits the constructs inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Block {
    Spawn,
    Defer,
    Recover,
    Generator,
}

/// A construct a block can limit. A `spawn:`, `defer:` or `recover:` block limits a jump (`Break` to
/// `Try`); a generator body limits the statements `Defer` to `Wait`. An `else` guard is none: its
/// block leaves by one of these, and that statement's cell decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Construct {
    Break,
    Continue,
    Return,
    Try,
    Defer,
    Spawn,
    Parallel,
    Wait,
}

/// One cell of [`RULES`]. A construct with no cell in a block's row crosses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rule {
    /// The block is the construct's target, so outer blocks do not see it. A `T?` operand is
    /// rejected for the reason (`?` in `recover:`: the block's result is a `T!E` value).
    Target(&'static str),
    /// The block rejects the construct for the reason.
    Rejected(&'static str),
}

/// One row of [`RULES`]: the block, its name in a message, and its cells.
pub(super) struct Row {
    pub block: Block,
    pub noun: &'static str,
    pub cells: &'static [(Construct, Rule)],
}

use Construct as C;
use Rule::{Rejected, Target};

const SPAWN_LOOP: Rule = Rejected("a spawned task runs outside the loop");
const DEFER_LOOP: Rule = Rejected("a deferred block runs at scope exit, outside the loop");
const RECOVER_LOOP: Rule =
    Rejected("a recover block produces a value and cannot leave to the loop");
const GEN_CONCURRENCY: Rule =
    Rejected("a suspended generator does not manage structured-concurrency state");

/// What may cross each block, one row per [`Block`] in declaration order.
pub(super) const RULES: [Row; 4] = [
    Row {
        block: Block::Spawn,
        noun: "spawn block",
        cells: &[
            (C::Break, SPAWN_LOOP),
            (C::Continue, SPAWN_LOOP),
            (
                C::Return,
                Rejected("a spawned task has no caller to return to"),
            ),
            (
                C::Try,
                Rejected("a spawned task has no caller to propagate to"),
            ),
        ],
    },
    Row {
        block: Block::Defer,
        noun: "defer block",
        cells: &[
            (C::Break, DEFER_LOOP),
            (C::Continue, DEFER_LOOP),
            (
                C::Return,
                Rejected("a deferred block has no caller to return to"),
            ),
            (
                C::Try,
                Rejected(
                    "a deferred block has no caller to propagate to; handle the error in the block with `match` or `??`",
                ),
            ),
        ],
    },
    Row {
        block: Block::Recover,
        noun: "recover block",
        cells: &[
            (C::Break, RECOVER_LOOP),
            (C::Continue, RECOVER_LOOP),
            (
                C::Return,
                Rejected("a recover block produces a value and cannot leave the function"),
            ),
            (
                C::Try,
                Target(
                    "the operand is a `T?` value, and the block's result is a `T!E` value; use `match` instead",
                ),
            ),
        ],
    },
    Row {
        block: Block::Generator,
        noun: "generator body",
        cells: &[
            (
                C::Defer,
                Rejected("a suspended generator does not manage frame defers"),
            ),
            (C::Spawn, GEN_CONCURRENCY),
            (C::Parallel, GEN_CONCURRENCY),
            (C::Wait, GEN_CONCURRENCY),
        ],
    },
];

impl Block {
    fn row(self) -> &'static Row {
        let row = &RULES[self as usize];
        debug_assert_eq!(row.block, self, "RULES rows follow the Block order");
        row
    }

    /// The cell of `c` in this block's row; `None`: `c` crosses the block.
    pub(super) fn rule(self, c: Construct) -> Option<Rule> {
        self.row()
            .cells
            .iter()
            .find(|(cc, _)| *cc == c)
            .map(|(_, r)| *r)
    }
}

impl Construct {
    fn label(self) -> &'static str {
        match self {
            Construct::Break => "'break'",
            Construct::Continue => "'continue'",
            Construct::Return => "'return'",
            Construct::Try => "'?'",
            Construct::Defer => "'defer'",
            Construct::Spawn => "'spawn'",
            Construct::Parallel => "'parallel:'",
            Construct::Wait => "'wait:'",
        }
    }

    /// The statement construct a generator body limits, if `s` is one.
    pub(super) fn of_stmt(s: &StmtKind) -> Option<Construct> {
        match s {
            StmtKind::Defer(_) => Some(Construct::Defer),
            StmtKind::Spawn(_) => Some(Construct::Spawn),
            StmtKind::Parallel { .. } => Some(Construct::Parallel),
            StmtKind::Wait { .. } => Some(Construct::Wait),
            StmtKind::Let { .. }
            | StmtKind::Assign { .. }
            | StmtKind::Fn(_)
            | StmtKind::Struct { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Return(_)
            | StmtKind::Yield(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. }
            | StmtKind::Assert { .. }
            | StmtKind::If { .. }
            | StmtKind::For { .. }
            | StmtKind::While { .. }
            | StmtKind::Match { .. }
            | StmtKind::Expr(_) => None,
        }
    }
}

/// The one message shape: `'<construct>' is not allowed inside a <block>: <reason>`.
pub(super) fn message(block: Block, c: Construct, reason: &str) -> String {
    format!(
        "{} is not allowed inside a {}: {reason}",
        c.label(),
        block.row().noun
    )
}

/// What the blocks around a construct say about it.
pub(super) enum Verdict {
    /// No block stops it: it reaches the fn, or the loop.
    Free,
    /// A block is its target, with the `T?` reason ([`Rule::Target`]).
    Target(Block, &'static str),
    /// A block rejects it, with the message.
    Rejected(String),
}

impl Checker {
    /// Check `f` inside `block`. The entry keeps the `loop_depth` at entry, so a `break` knows
    /// whether a loop lies between it and the block.
    pub(super) fn in_block<R>(&mut self, block: Block, f: impl FnOnce(&mut Self) -> R) -> R {
        self.blocks.push((block, self.loop_depth));
        let r = f(self);
        self.blocks.pop();
        r
    }

    /// The verdict on `c` here: the blocks it crosses, innermost first, until one is its target or
    /// rejects it. A `break` or `continue` crosses only the blocks entered since its loop began, and
    /// with no loop at all it has no target to cross to.
    pub(super) fn block_verdict(&self, c: Construct) -> Verdict {
        let loop_exit = matches!(c, Construct::Break | Construct::Continue);
        if loop_exit && self.loop_depth == 0 {
            return Verdict::Free;
        }
        for &(block, depth) in self.blocks.iter().rev() {
            if loop_exit && depth != self.loop_depth {
                break;
            }
            match block.rule(c) {
                None => {}
                Some(Rule::Target(reason)) => return Verdict::Target(block, reason),
                Some(Rule::Rejected(reason)) => {
                    return Verdict::Rejected(message(block, c, reason));
                }
            }
        }
        Verdict::Free
    }

    /// Report `c` at `span` if a block around it rejects it. True when it did.
    pub(super) fn reject_in_block(&mut self, c: Construct, span: Span) -> bool {
        match self.block_verdict(c) {
            Verdict::Rejected(msg) => {
                self.error(span, msg);
                true
            }
            Verdict::Free | Verdict::Target(..) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_the_block_order() {
        for b in [Block::Spawn, Block::Defer, Block::Recover, Block::Generator] {
            assert_eq!(RULES[b as usize].block, b);
        }
    }
}
