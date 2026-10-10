//! The airlock's crossing policy (D4, `docs/decision-d4-airlock.md`): can the parent still reach an
//! object after it crosses into another task? The checker decides it ONCE per crossing value, as a
//! shape over the value graph ([`Fresh`], built by `Checker::fresh_shape`, TICKET-240). The
//! compiler only stores that shape; the runtime only walks it (`Vm::unmark_fresh`). A route whose
//! answer does not depend on the operand is one row of [`marks`]. Every bit layout lives only
//! here: the spawn operand reference ([`operand_ref`], [`operand`]), the generator creation stamp
//! ([`stamp_mask`], [`param_stamp`]) and the frame slot verdict ([`frame_slot`]).
//!
//! Limits, each a false fault and never a missed mark: a shape deeper than [`MAX_DEPTH`] levels or
//! wider than [`MAX_WIDTH`] positional children is `Marked` or root-only below the limit; a spawn
//! whose table index does not fit 31 bits has every operand `Marked`; a generator param at index
//! 32 and up and a frame local at slot 64 and up are `Marked`.

/// The deepest shape level `Checker::fresh_shape` describes; below it a subtree is [`Fresh::Marked`].
pub const MAX_DEPTH: usize = 16;
/// The most positional children one [`Kids::At`] holds; a wider node falls back to [`Kids::Each`].
pub const MAX_WIDTH: usize = 64;

/// What a crossing leaves marked in one value graph (TICKET-240).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Fresh {
    /// The parent may still reach it: keep the mark, do not descend.
    Marked,
    /// Built at the crossing site: unmark this object, then apply [`Kids`] to its children.
    Node(Kids),
    /// Every object reachable from it was built at the crossing site.
    All,
}

/// The children of a [`Fresh::Node`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Kids {
    /// Every child stays marked (`copy()` is shallow, DEC-160).
    Marked,
    /// Every child takes this one shape. For a map these are its VALUES; a key stays marked.
    Each(Box<Fresh>),
    /// Child `i` takes shape `i`. The runtime applies it only when the child count matches.
    At(Box<[Fresh]>),
}

static MARKED: Fresh = Fresh::Marked;

impl Fresh {
    pub fn is_all(&self) -> bool {
        *self == Fresh::All
    }

    pub fn is_marked(&self) -> bool {
        *self == Fresh::Marked
    }

    /// A fresh object whose children all stay marked.
    pub fn root() -> Fresh {
        Fresh::Node(Kids::Marked)
    }

    /// A fresh object whose children each take shape `k`.
    pub fn each(k: Fresh) -> Fresh {
        match k {
            Fresh::All => Fresh::All,
            Fresh::Marked => Fresh::root(),
            k => Fresh::Node(Kids::Each(Box::new(k))),
        }
    }

    /// A fresh object with positional children. No child, or every child `All`, is `All`. Past
    /// [`MAX_WIDTH`] children it is `each` of their [`meet`](Fresh::meet).
    pub fn at(kids: Vec<Fresh>) -> Fresh {
        if kids.iter().all(Fresh::is_all) {
            Fresh::All
        } else if kids.len() > MAX_WIDTH {
            Fresh::each(Fresh::meet(&kids))
        } else {
            Fresh::Node(Kids::At(kids.into_boxed_slice()))
        }
    }

    /// A fresh map whose keys are not all fresh: its values each take shape `k`. It NEVER gives
    /// `All`, because `All` would unmark a key the parent still holds.
    pub fn values(k: Fresh) -> Fresh {
        match k {
            Fresh::Marked => Fresh::root(),
            k => Fresh::Node(Kids::Each(Box::new(k))),
        }
    }

    /// One shape that is sound for every one of `kids`: the shared shape when all are equal,
    /// root-only when none is `Marked`, else `Marked`.
    pub fn meet(kids: &[Fresh]) -> Fresh {
        match kids {
            [] => Fresh::All,
            [first, rest @ ..] if rest.iter().all(|k| k == first) => first.clone(),
            _ if kids.iter().any(Fresh::is_marked) => Fresh::Marked,
            _ => Fresh::root(),
        }
    }
}

/// The head-behind-an-entry-thunk bit of a spawn operand reference.
const BEHIND_ENTRY: u32 = 1 << 31;

/// The `u32` a spawn op carries for entry `index` of `Program.fresh_calls`: `0` = every operand is
/// `Marked`, else the index plus one in the low 31 bits. An index that does not fit gives `0`.
pub fn operand_ref(index: usize) -> u32 {
    match u32::try_from(index) {
        Ok(i) if i < BEHIND_ENTRY - 1 => i + 1,
        _ => 0,
    }
}

/// The head rides as argument 0 behind an entry thunk (TICKET-235): argument `i` then reads entry
/// position `i` instead of `i + 1`. `0` stays `0`.
pub fn behind_entry(r: u32) -> u32 {
    if r == 0 { 0 } else { r | BEHIND_ENTRY }
}

/// The shape of argument `arg` of the spawn whose reference is `r`. Entry position 0 is the
/// receiver, position `j + 1` is bound slot `j`. A miss is `Marked`.
pub fn operand(table: &[Vec<Fresh>], r: u32, arg: usize) -> &Fresh {
    let entry = match (r & !BEHIND_ENTRY) as usize {
        0 => return &MARKED,
        i => table.get(i - 1),
    };
    let pos = if r & BEHIND_ENTRY != 0 { arg } else { arg + 1 };
    entry.and_then(|e| e.get(pos)).unwrap_or(&MARKED)
}

/// What a marking crossing unmarks in one generator frame slot. A frame lives on between build
/// and crossing, so it never gets a positional shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SlotFresh {
    Marked,
    /// The slot's root object is private; its children stay marked.
    Root,
    /// The slot's whole graph is private.
    All,
}

/// The params one creation stamp holds.
const STAMP_PARAMS: usize = 32;

/// A generator's creation stamp: two bits per PARAM `j < 32`. Bit `2j` = the creating call's
/// argument is not `Marked`, bit `2j + 1` = it is `All`.
pub fn stamp_mask(args: &[Fresh]) -> u64 {
    args.iter()
        .take(STAMP_PARAMS)
        .enumerate()
        .fold(0, |m, (j, a)| {
            let bits = match a {
                Fresh::Marked => 0,
                Fresh::Node(_) => 0b01,
                Fresh::All => 0b11,
            };
            m | bits << (2 * j)
        })
}

/// The proto's own verdict for its params in the [`stamp_mask`] layout: bit `2j` = param slot `j`
/// is in `private`, bit `2j + 1` = it is in `deep`. A generator's stamp is the AND of both.
pub fn param_stamp(private: u64, deep: u64, arity: usize) -> u64 {
    (0..arity.min(STAMP_PARAMS)).fold(0, |m, j| {
        m | ((private >> j) & 1) << (2 * j) | ((deep >> j) & 1) << (2 * j + 1)
    })
}

/// The verdict for frame slot `k`. A param slot (`k < arity`) reads the generator's `stamp`; a
/// local slot reads the proto's `private` and `deep` masks.
pub fn frame_slot(private: u64, deep: u64, arity: usize, stamp: u64, k: usize) -> SlotFresh {
    let bits = if k < arity {
        if k >= STAMP_PARAMS {
            return SlotFresh::Marked;
        }
        (stamp >> (2 * k)) & 0b11
    } else if k >= u64::BITS as usize {
        return SlotFresh::Marked;
    } else {
        ((private >> k) & 1) | ((deep >> k) & 1) << 1
    };
    match bits {
        0b11 => SlotFresh::All,
        0b01 => SlotFresh::Root,
        _ => SlotFresh::Marked,
    }
}

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

    /// The head rides as argument 0 behind an entry thunk: bit 0 is the thunk and is never set, the
    /// receiver or callee moves to bit 1, bound slot `j` to bit `j + 2`; a slot shifted past the top
    /// bit reads `Copy`.
    pub fn behind_entry(mask: u32) -> u32 {
        mask << 1
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
    /// A value a task copy read through a handle whose owner aliases it: `Task.get`, a
    /// `memoize1` wrapper (TICKET-213). `std.concurrency.task_copy_of` rebuilds it through
    /// `from_wire_memo` (`Vm::airlock_native`, TICKET-220), so it takes every marking rule a
    /// rebuild has, the generator frame mask included.
    CopyRead,
}

/// Whether a rebuild on `route` marks what it allocates, given the `enclosing` mark in force.
pub fn marks(route: Route, enclosing: bool) -> bool {
    match route {
        Route::Spawn
        | Route::CopyRead
        | Route::ModuleSnapshot
        | Route::ClosureCaptures { same_heap: false } => true,
        Route::ClosureCaptures { same_heap: true } => enclosing,
        Route::Handoff => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_layout_round_trips() {
        use Fresh::{All, Marked, Node};
        assert_eq!(Fresh::root(), Node(Kids::Marked));
        assert_eq!(Fresh::each(All), All);
        assert_eq!(Fresh::each(Marked), Fresh::root());
        assert_eq!(
            Fresh::each(Fresh::root()),
            Node(Kids::Each(Box::new(Fresh::root())))
        );
        // `at`: no kid and all-`All` kids are `All`; anything else keeps its positions.
        assert_eq!(Fresh::at(vec![]), All);
        assert_eq!(Fresh::at(vec![All, All]), All);
        assert_eq!(
            Fresh::at(vec![All, Marked]),
            Node(Kids::At(vec![All, Marked].into_boxed_slice()))
        );
        // The value each side of MAX_WIDTH.
        let mut wide = vec![Fresh::root(); MAX_WIDTH];
        assert!(matches!(Fresh::at(wide.clone()), Node(Kids::At(k)) if k.len() == MAX_WIDTH));
        wide.push(Fresh::root());
        assert_eq!(Fresh::at(wide.clone()), Fresh::each(Fresh::root()));
        wide.push(Marked);
        assert_eq!(Fresh::at(wide), Fresh::root());
        // `meet`.
        assert_eq!(Fresh::meet(&[Fresh::root(), Fresh::root()]), Fresh::root());
        assert_eq!(Fresh::meet(&[All, Fresh::root()]), Fresh::root());
        assert_eq!(Fresh::meet(&[Marked, All]), Marked);
        assert_eq!(Fresh::meet(&[All, All]), All);
        // A map with a key that is not fresh never becomes `All`, at any nesting.
        let v = Fresh::values(All);
        assert_eq!(v, Node(Kids::Each(Box::new(All))));
        assert!(!v.is_all());
        assert!(!Fresh::at(vec![v.clone()]).is_all());
        assert!(!Fresh::each(v).is_all());
        assert_eq!(Fresh::values(Marked), Fresh::root());
        // Spawn operands: entry position 0 is the receiver, `j + 1` is bound slot `j`.
        let table = vec![vec![Marked, All], vec![All, Marked, Fresh::root()]];
        assert_eq!(operand_ref(0), 1);
        assert_eq!(operand_ref(1), 2);
        let r = operand_ref(1);
        assert_eq!(operand(&table, r, 0), &Marked);
        assert_eq!(operand(&table, r, 1), &Fresh::root());
        assert_eq!(operand(&table, r, 2), &Marked);
        assert_eq!(operand(&table, 0, 0), &Marked);
        assert_eq!(operand(&table, 3, 0), &Marked);
        // TICKET-235: behind an entry thunk argument `i` reads position `i`.
        let b = behind_entry(r);
        assert_eq!(operand(&table, b, 0), &All);
        assert_eq!(operand(&table, b, 1), &Marked);
        assert_eq!(operand(&table, b, 2), &Fresh::root());
        assert_eq!(operand(&table, b, 3), &Marked);
        assert_eq!(behind_entry(0), 0);
        // An index that does not fit 31 bits has every operand `Marked`.
        assert_eq!(operand_ref(0x7FFF_FFFE), 0x7FFF_FFFF);
        assert_eq!(operand_ref(0x7FFF_FFFF), 0);
        assert_eq!(operand_ref(usize::MAX), 0);
        // Generator stamp: two bits per param.
        let stamp = stamp_mask(&[Marked, Fresh::root(), All]);
        assert_eq!(stamp, 0b11_01_00);
        let (private, deep) = (0b1_0110, 0b1_0100);
        assert_eq!(param_stamp(private, deep, 3), 0b11_01_00);
        assert_eq!(param_stamp(u64::MAX, u64::MAX, 2), 0b1111);
        assert_eq!(param_stamp(u64::MAX, u64::MAX, 40), u64::MAX);
        let slot = |k| frame_slot(private, deep, 3, stamp, k);
        assert_eq!(slot(0), SlotFresh::Marked);
        assert_eq!(slot(1), SlotFresh::Root);
        assert_eq!(slot(2), SlotFresh::All);
        // A local reads the proto masks; the stamp does not reach it.
        assert_eq!(slot(3), SlotFresh::Marked);
        assert_eq!(slot(4), SlotFresh::All);
        assert_eq!(frame_slot(0b1000, 0, 3, u64::MAX, 3), SlotFresh::Root);
        // A deep bit without its private bit is not a verdict.
        assert_eq!(frame_slot(0, 0b1000, 3, 0, 3), SlotFresh::Marked);
        assert_eq!(frame_slot(0, 0, 1, 0b10, 0), SlotFresh::Marked);
        // Limits: a param at index 32 and a local at slot 64 read `Marked`.
        let full = stamp_mask(&vec![All; 33]);
        assert_eq!(full, u64::MAX);
        assert_eq!(frame_slot(u64::MAX, u64::MAX, 33, full, 31), SlotFresh::All);
        assert_eq!(
            frame_slot(u64::MAX, u64::MAX, 33, full, 32),
            SlotFresh::Marked
        );
        assert_eq!(frame_slot(u64::MAX, u64::MAX, 0, 0, 63), SlotFresh::All);
        assert_eq!(frame_slot(u64::MAX, u64::MAX, 0, 0, 64), SlotFresh::Marked);
    }

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
        // TICKET-235: behind an entry thunk every operand sits one bit higher.
        let m = Crossing::behind_entry(Crossing::mask(Some(Move), &[Copy, Move]));
        assert_eq!(Crossing::from_mask(m, 0), Copy);
        assert_eq!(Crossing::from_mask(m, 1), Move);
        assert_eq!(Crossing::from_mask(m, 2), Copy);
        assert_eq!(Crossing::from_mask(m, 3), Move);
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
            assert!(marks(Route::CopyRead, enclosing));
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
