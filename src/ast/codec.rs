//! The build-time std artifact: AST values encoded with `borsh`, their node ids shifted on decode.

use super::{NEXT_NODE_ID, NodeId};
use borsh::{BorshDeserialize, BorshSerialize};
use std::cell::Cell;
use std::sync::atomic::Ordering;

thread_local! {
    /// What [`decode`] adds to each node id it reads. 0 outside a decode.
    static ID_SHIFT: Cell<u32> = const { Cell::new(0) };
}

impl BorshSerialize for NodeId {
    fn serialize<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        self.0.serialize(w)
    }
}

/// Shifts every id, wherever it sits: the `ids_in_block` walker skips binding targets.
impl BorshDeserialize for NodeId {
    fn deserialize_reader<R: std::io::Read>(r: &mut R) -> std::io::Result<Self> {
        let raw = u32::deserialize_reader(r)?;
        if raw == NodeId::SYNTH.0 {
            return Ok(NodeId::SYNTH);
        }
        Ok(NodeId(raw.wrapping_add(ID_SHIFT.with(Cell::get))))
    }
}

/// Run `f` and return the node ids it drew with its result. Single-threaded callers only (the
/// build script): an id another thread draws meanwhile falls inside the range.
pub fn with_id_range<T>(f: impl FnOnce() -> T) -> (std::ops::Range<u32>, T) {
    let lo = NEXT_NODE_ID.load(Ordering::Relaxed);
    let v = f();
    (lo..NEXT_NODE_ID.load(Ordering::Relaxed), v)
}

/// The artifact bytes of `value`, whose node ids all lie in `ids`.
pub fn encode<T: BorshSerialize>(ids: std::ops::Range<u32>, value: &T) -> Vec<u8> {
    borsh::to_vec(&(ids.start, ids.end, value)).expect("an AST encodes")
}

/// The value of artifact `bytes`. Its node ids move into a block this process reserves, so they
/// keep their order and gaps and collide with no id this process drew or will draw.
pub fn decode<T: BorshDeserialize>(bytes: &[u8]) -> Option<T> {
    let mut r = bytes;
    let (lo, end) = <(u32, u32)>::deserialize_reader(&mut r).ok()?;
    let base = NodeId::fresh_block(end.checked_sub(lo)?).0;
    ID_SHIFT.with(|s| s.set(base.wrapping_sub(lo)));
    let v = T::deserialize_reader(&mut r);
    ID_SHIFT.with(|s| s.set(0));
    let v = v.ok()?;
    r.is_empty().then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_format_spec_round_trips() {
        let src = "x := 1234.5\ns := \"{x:*>+12,.2f}\"\n";
        let (ids, module) =
            with_id_range(|| crate::parser::parse(crate::lexer::tokenize(src).unwrap()).unwrap());
        let bytes = encode(ids, &module);
        let back = decode::<crate::ast::Module>(&bytes).expect("the artifact decodes");
        assert_eq!(back, module);
        let text = format!("{back:?}");
        for part in [
            "fill: '*'",
            "sign: Some('+')",
            "group: Some(',')",
            "ty: Some('f')",
        ] {
            assert!(text.contains(part), "decoded module lacks `{part}`: {text}");
        }
    }
}
