//! TICKET-189 — the airlock's crossing policy (D4, `docs/decision-d4-airlock.md`): can the parent
//! still observe a value after it crosses into another task? The checker decides it ONCE per
//! spawn operand ([`Crossing`], recorded per call site); the compiler only encodes that decision
//! with [`Crossing::mask`]; the runtime only decodes it with [`Crossing::from_mask`]. A route whose
//! answer does not depend on the operand is one row of [`marks`]. The bit layout lives only here.

/// How one spawn operand crosses into its task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crossing {
    /// Built fresh at the call site, so no parent binding reaches it: rebuilt marked, then its ROOT
    /// is unmarked. Its children stay marked (`copy()` is shallow, DEC-160).
    Move,
    /// The parent can still observe it: every rebuilt object is marked, so a task write faults.
    Copy,
}

impl Crossing {
    /// Encode one spawn's operands: bit 0 = the receiver is `Move`, bit `j + 1` = argument slot `j`
    /// is `Move`. A slot at bit 32 or above stays `Copy` (a false fault, never a lost write).
    pub fn mask(recv: Option<Crossing>, args: &[Crossing]) -> u32 {
        let bit = |c: Crossing, b: usize| {
            if c == Crossing::Move && b < u32::BITS as usize {
                1 << b
            } else {
                0
            }
        };
        let r = recv.map_or(0, |c| bit(c, 0));
        args.iter()
            .enumerate()
            .fold(r, |m, (j, &c)| m | bit(c, j + 1))
    }

    /// Decode bit `bit` of a [`mask`](Crossing::mask). A bit at 32 or above is `Copy`.
    pub fn from_mask(mask: u32, bit: usize) -> Crossing {
        if bit < u32::BITS as usize && (mask >> bit) & 1 == 1 {
            Crossing::Move
        } else {
            Crossing::Copy
        }
    }

    /// TICKET-190: encode a generator frame: bit `k` = frame slot `k` is `Move` (private, the
    /// parent cannot reach its root). A slot at 64 or above stays `Copy` (a false fault, never a
    /// lost write).
    pub fn frame_mask(slots: &[Crossing]) -> u64 {
        slots
            .iter()
            .enumerate()
            .filter(|&(k, &c)| c == Crossing::Move && k < u64::BITS as usize)
            .fold(0, |m, (k, _)| m | 1 << k)
    }

    /// Decode frame slot `k` of a [`frame_mask`](Crossing::frame_mask). A slot at 64 or above is
    /// `Copy`.
    pub fn frame_slot(mask: u64, k: usize) -> Crossing {
        if k < u64::BITS as usize && (mask >> k) & 1 == 1 {
            Crossing::Move
        } else {
            Crossing::Copy
        }
    }
}

/// The frame-mask bits of a proto's `arity` param slots (params are slots `0..arity`).
pub fn param_bits(arity: usize) -> u64 {
    if arity >= u64::BITS as usize {
        u64::MAX
    } else {
        (1u64 << arity) - 1
    }
}

/// A runtime crossing route whose mark does not depend on the operand.
#[derive(Clone, Copy, Debug)]
pub enum Route {
    /// A spawn's callee captures, arguments and receiver: the parent keeps its bindings, so every
    /// rebuilt object is a copy (a `Move` operand is then unmarked by [`Crossing::from_mask`]).
    Spawn,
    /// A crossing closure's captures: a capture is a by-reference binding of its creator. Back in
    /// the heap it came from (a same-task round-trip), it keeps the enclosing mark (W7-4c).
    ClosureCaptures { same_heap: bool },
    /// The spawn's module snapshot: the parent keeps its globals.
    ModuleSnapshot,
    /// A Channel/Shared/RwShared/Atomic read or an Executor job root: the sender gave the value
    /// away, so the receiver owns it (DEC-179 item 6).
    Handoff,
}

/// Whether a rebuild on `route` marks what it allocates, given the `enclosing` mark in force.
pub fn marks(route: Route, enclosing: bool) -> bool {
    match route {
        Route::Spawn | Route::ModuleSnapshot | Route::ClosureCaptures { same_heap: false } => true,
        Route::ClosureCaptures { same_heap: true } => enclosing,
        Route::Handoff => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_round_trips_and_every_route_marks_as_documented() {
        use Crossing::{Copy, Move};
        assert_eq!(Crossing::mask(Some(Move), &[]), 1);
        assert_eq!(Crossing::mask(None, &[Copy, Move]), 0b100);
        assert_eq!(
            Crossing::from_mask(Crossing::mask(Some(Move), &[]), 0),
            Move
        );
        assert_eq!(
            Crossing::from_mask(Crossing::mask(Some(Copy), &[Move]), 0),
            Copy
        );
        assert_eq!(Crossing::from_mask(Crossing::mask(None, &[Move]), 1), Move);
        let mut wide = vec![Copy; 31];
        wide[30] = Move;
        assert_eq!(Crossing::from_mask(Crossing::mask(None, &wide), 31), Move);
        assert_eq!(Crossing::from_mask(Crossing::mask(None, &wide), 30), Copy);
        // Slot 31 would be bit 32: it cannot be encoded, so it stays Copy.
        wide.push(Move);
        assert_eq!(Crossing::from_mask(Crossing::mask(None, &wide), 32), Copy);
        assert_eq!(Crossing::from_mask(u32::MAX, 32), Copy);
        // TICKET-190: a generator frame mask, one bit per frame slot.
        let slots = [Move, Copy, Move];
        let fm = Crossing::frame_mask(&slots);
        assert_eq!(fm, 0b101);
        for (k, &c) in slots.iter().enumerate() {
            assert_eq!(Crossing::frame_slot(fm, k), c);
        }
        let mut frame = vec![Copy; 64];
        frame[63] = Move;
        assert_eq!(Crossing::frame_slot(Crossing::frame_mask(&frame), 63), Move);
        // Slot 64 cannot be encoded, so it stays Copy.
        frame.push(Move);
        assert_eq!(Crossing::frame_mask(&frame), 1 << 63);
        assert_eq!(Crossing::frame_slot(u64::MAX, 64), Copy);
        assert_eq!(param_bits(0), 0);
        assert_eq!(param_bits(2), 0b11);
        assert_eq!(param_bits(64), u64::MAX);
        assert_eq!(param_bits(70), u64::MAX);
        for enclosing in [false, true] {
            assert!(marks(Route::Spawn, enclosing));
            assert!(marks(Route::ModuleSnapshot, enclosing));
            assert!(marks(
                Route::ClosureCaptures { same_heap: false },
                enclosing
            ));
            assert_eq!(
                marks(Route::ClosureCaptures { same_heap: true }, enclosing),
                enclosing
            );
            assert!(!marks(Route::Handoff, enclosing));
        }
    }
}
