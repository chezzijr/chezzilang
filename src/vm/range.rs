//! TICKET-253 — reading a `Range` value at run time. `Range` is the struct of `std/prelude.chz`
//! (`start: int?`, `end: int?`, `step: int`, `inclusive: bool`); a slice site hands the value to
//! [`Vm::as_range`] and resolves it against the receiver's length with the one function
//! `crate::slice::range_indices`.

use super::*;
use crate::slice::RangeParts;

impl Vm {
    /// The four fields of `v` when it is a prelude `Range`, else `None`. A value that is no heap
    /// object, or a struct of another type, answers `None` at once.
    pub(super) fn as_range(&self, v: Value) -> Option<RangeParts> {
        let h = v.as_obj()?;
        let Obj::Struct { tid, fields } = self.heap.get(h) else {
            return None;
        };
        if *tid != self.struct_tid("Range") {
            return None;
        }
        let [start, end, step, inclusive] = fields.as_slice() else {
            return None;
        };
        let bound = |v: Value| -> Option<Option<i64>> {
            match v.as_obj().map(|h| self.heap.get(h)) {
                Some(Obj::Enum {
                    variant_id,
                    payload,
                }) if *variant_id == crate::vm::op::VID_SOME => {
                    Some(Some(self.int_val(*payload.first()?)?))
                }
                Some(Obj::Enum { variant_id, .. })
                    if *variant_id == crate::vm::op::VID_NONE_VARIANT =>
                {
                    Some(None)
                }
                _ => None,
            }
        };
        Some(RangeParts {
            start: bound(*start)?,
            end: bound(*end)?,
            step: self.int_val(*step)?,
            inclusive: inclusive.as_bool()?,
        })
    }
}
