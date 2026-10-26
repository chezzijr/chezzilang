//! The airlock's crossing policy (D4, `docs/design/adr-d4-airlock.md`): can the parent still reach an
//! object after it crosses into another task? The checker decides it ONCE per crossing value, as a
//! shape over the value graph ([`Fresh`], built by `Checker::fresh_shape`, TICKET-240). The
//! compiler only stores that shape; the test-only rebuild walks it (`Vm::unmark_fresh`, deleted
//! with the airlock in M3.5). A route whose
//! answer does not depend on the operand is one row of [`marks`]. Every bit layout lives only
//! here: the generator creation stamp ([`stamp_mask`], [`param_stamp`]) and the frame slot verdict
//! ([`frame_slot`]). M3.4: a spawn passes its operands by handle, so no spawn shape is left.
//!
//! Limits, each a false fault and never a missed mark: a shape deeper than [`MAX_DEPTH`] levels or
//! wider than [`MAX_WIDTH`] positional children is `Marked` or root-only below the limit; a
//! generator param at index 32 and up and a frame local at slot 64 and up are `Marked`.

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
            k @ Fresh::Node(_) | k @ Fresh::All => Fresh::Node(Kids::Each(Box::new(k))),
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

/// What a marking crossing unmarks in one generator frame slot. A frame lives on between build
/// and crossing, so it never gets a positional shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// A proto's frame mask from one claim per slot: bit `k` = slot `k` claims. A slot at 64 or
/// above never claims (a false fault, never a lost write).
pub fn slot_bits(claims: &[bool]) -> u64 {
    claims
        .iter()
        .take(u64::BITS as usize)
        .enumerate()
        .filter(|&(_, &c)| c)
        .fold(0, |m, (k, _)| m | 1 << k)
}

/// A runtime crossing route whose mark does not depend on the operand.
#[derive(Clone, Copy, Debug)]
pub enum Route {
    /// A crossing closure's captures: a capture is a by-reference binding of its creator, so its
    /// rebuild is a copy in every heap, the one it left included (TICKET-291).
    ClosureCaptures,
    /// A hand-off rebuild (an Executor job root): the sender gave the value away, so the receiver
    /// owns it (DEC-179 item 6). A closure inside the value still marks its captures by
    /// `ClosureCaptures`.
    Handoff,
}

/// Whether a rebuild on `route` marks what it allocates.
pub fn marks(route: Route) -> bool {
    match route {
        Route::ClosureCaptures => true,
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
        // A proto mask: one bit per slot, and slot 64 cannot claim.
        assert_eq!(slot_bits(&[true, false, true]), 0b101);
        let mut claims = vec![false; 64];
        claims[63] = true;
        claims.push(true);
        assert_eq!(slot_bits(&claims), 1 << 63);
        assert!(SlotFresh::Marked < SlotFresh::Root && SlotFresh::Root < SlotFresh::All);
    }

    #[test]
    fn every_route_marks_as_documented() {
        assert!(marks(Route::ClosureCaptures));
        assert!(!marks(Route::Handoff));
    }
}
