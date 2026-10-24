//! VM: field and index reads and writes, slices, match arms.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::*;
use crate::base::hooks::Hook;

impl Vm {
    pub(super) fn get_field(
        &mut self,
        name: &str,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            ));
        };
        self.ensure_module_faulted(h); // D1: `module.member` on a not-yet-faulted worker module
        // M19 Phase 5b — inline-cache fast path: a hit collapses the struct name-probe to one pure-int
        // `tid` compare (the struct's layout id). Same `tid` ⇒ same field order ⇒ the cached `idx` is
        // the right slot, so the field-name re-verify is unnecessary. `cell.tid == TID_NONE` (empty or
        // an unregistered struct) never matches, forcing the probe below. `fields.get` stays bounds-
        // safe (defensive; same `tid` guarantees in-range). Worst case on a miss: a re-probe + refill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get(h)
                && *tid == cell.tid
                && let Some(v) = fields.get(cell.idx as usize)
            {
                let v = *v;
                self.push(v);
                return Ok(());
            }
        }
        match self.heap.get(h) {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
            Obj::Tuple(items) => {
                let v = name
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("tuple has no element '.{name}' (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Struct { tid, fields, .. } => {
                // Positional layout: the field name->index map lives in the StructDef (declaration
                // order), not the instance. Resolve the slot there, then index the flat `fields`
                // Vec. Capture both index + layout `tid` so the IC can cache them (Value is Copy,
                // `tid` is a `u32`, so the heap borrow ends here, freeing `self` for the write).
                let tid = *tid;
                let sname = self.struct_name_of_tid(tid);
                let idx = self
                    .program
                    .structs
                    .get(sname)
                    .and_then(|d| d.fields.iter().position(|f| f == name));
                let found = idx.and_then(|i| fields.get(i).map(|v| (i, *v)));
                match found {
                    Some((i, v)) => {
                        if ic != NO_IC {
                            self.field_ic[ic as usize] = IcCell { idx: i as u32, tid };
                        }
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        let shown = self.display(obj);
                        Err(self.err(format!("no field '{name}' on {shown}"), span))
                    }
                }
            }
            Obj::Module(m) => match m.index.get(name).copied() {
                Some(i) => {
                    let v = self.read_slot(h, i, span)?;
                    self.push(v);
                    Ok(())
                }
                None => Err(self.err(format!("module '{}' has no member '{name}'", m.name), span)),
            },
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            )),
        }
    }

    /// `obj[range]` read as a SLICE (TICKET-253): a copy of the elements of a list, `str`, `bytes` or
    /// `bytearray` that the `Range` value selects, or the result of a struct's `slice` hook. The
    /// shared `slice::range_indices` resolver owns all the clamp/step/reverse math. The checker chose
    /// this reading for the subscript; the VM does not choose again.
    pub(super) fn slice_range(&mut self, span: Span) -> Result<(), RuntimeError> {
        let key = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
        };
        let Some(parts) = self.as_range(key) else {
            return Err(self.err(
                format!("cannot slice by {}: expected a Range", self.type_name(key)),
                span,
            ));
        };
        // Snapshot the result kind without holding the heap borrow across the alloc / method call.
        enum Sliced {
            List(Vec<Value>),
            Str(String),
            Bytes(Vec<u8>),
            ByteArray(Vec<u8>),
            Struct,
        }
        let sliced = match self.heap.get(h) {
            Obj::List(items) => {
                let idxs = crate::slice::range_indices(&parts, items.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::List(idxs.iter().map(|&i| items.get(i)).collect())
            }
            // ASCII arm: a byte index IS a codepoint index. The `None` arm stays O(n) per slice for
            // non-ASCII text (DEC-072).
            Obj::Str(string) => match string.ascii_bytes() {
                Some(b) => {
                    let idxs = crate::slice::range_indices(&parts, b.len())
                        .map_err(|m| self.err(m.to_string(), span))?;
                    Sliced::Str(idxs.iter().map(|&i| b[i] as char).collect())
                }
                None => {
                    let chars: Vec<char> = string.chars().collect();
                    let idxs = crate::slice::range_indices(&parts, chars.len())
                        .map_err(|m| self.err(m.to_string(), span))?;
                    Sliced::Str(idxs.iter().map(|&i| chars[i]).collect())
                }
            },
            // `bytes[r]` slices over BYTE offsets and yields a new `bytes`.
            Obj::Bytes(b) => {
                let idxs = crate::slice::range_indices(&parts, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::Bytes(idxs.iter().map(|&i| b[i]).collect())
            }
            // `bytearray[r]` slices over BYTE offsets and yields a NEW `bytearray` (mutable copy).
            Obj::ByteArray(b) => {
                let idxs = crate::slice::range_indices(&parts, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::ByteArray(idxs.iter().map(|&i| b[i]).collect())
            }
            Obj::Struct { .. } | Obj::Enum { .. } => Sliced::Struct,
            Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => {
                return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
            }
        };
        match sliced {
            Sliced::List(slice) => {
                // Root the source across the alloc: the new list shares its element handles, which
                // are otherwise unreachable (the source was popped) and could be collected by a GC.
                self.push(obj);
                let nh = self.heap.alloc(Obj::List(slice.into()));
                self.pop();
                self.push(Value::obj(nh));
            }
            Sliced::Str(sub) => {
                let nh = self.heap.alloc(Obj::Str(sub.into()));
                self.push(Value::obj(nh));
            }
            Sliced::Bytes(sub) => {
                let nh = self.heap.alloc(Obj::Bytes(sub.into_boxed_slice()));
                self.push(Value::obj(nh));
            }
            Sliced::ByteArray(sub) => {
                let nh = self.heap.alloc(Obj::ByteArray(sub));
                self.push(Value::obj(nh));
            }
            Sliced::Struct => {
                let Some(v) =
                    self.reentered(|vm| vm.invoke_hook(Hook::Slice, obj, &[key], span))?
                else {
                    return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
                };
                self.push(v);
            }
        }
        Ok(())
    }

    pub(super) fn get_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        // The index is NOT pre-validated as int (the `AsInt` was removed so map keys can be
        // str/bool): pop it as a Value and validate per object kind.
        let key = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // M19 Tier-2 — Int-key fast path. A `List` or `Map` indexed by an `Int` needs no rooting:
        // `scalar_hash` on an int allocates nothing, can't GC, can't re-enter user code, so the
        // `hash_key_rooted` push/pop the general Map arm does is pure waste. The `candidates` +
        // `values_equal` probe is unchanged, so an Int key still matches a `values_equal` `Float`
        // key. `Str`/`Struct` (and any non-Int key) fall through to the general match below.
        if let Some(n) = key.as_int_inline() {
            match self.heap.get(h) {
                Obj::List(items) => {
                    return match crate::slice::norm_index(n, items.len()).map(|i| items.get(i)) {
                        Some(v) => {
                            self.push(v);
                            Ok(())
                        }
                        None => Err(self.err(
                            format!("index {n} out of bounds (len {})", items.len()),
                            span,
                        )),
                    };
                }
                // `bytes[i]`/`bytearray[i]` → `int` (0–255); out-of-range faults recoverably.
                Obj::Bytes(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::ByteArray(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::Map(_) => {
                    let hk = self.scalar_hash(key);
                    return match self.map_probe(h, hk, key, span)? {
                        Some(p) => {
                            let Obj::Map(m) = self.heap.get(h) else {
                                unreachable!()
                            };
                            let v = m.entries[p].2;
                            self.push(v);
                            Ok(())
                        }
                        // Int fast path: `key` is already the plain `n` popped above, so no
                        // rendering call (and no rooting) is needed to name it.
                        None => Err(self.err(format!("key not found: {n}"), span)),
                    };
                }
                Obj::Str(_)
                | Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::Tuple(_)
                | Obj::Set(_)
                | Obj::Struct { .. }
                | Obj::Enum { .. }
                | Obj::Cell(_)
                | Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module(_)
                | Obj::Native { .. }
                | Obj::Builtin(_)
                | Obj::Cffi(_)
                | Obj::Ptr(_)
                | Obj::Channel(_)
                | Obj::Shared(_)
                | Obj::RwShared(_)
                | Obj::Atomic(_)
                | Obj::AtomicInt(_)
                | Obj::Executor(_)
                | Obj::Socket(_)
                | Obj::Listener(_)
                | Obj::Writer(_)
                | Obj::Reader(_)
                | Obj::Generator(_) => {}
            }
        }
        // Require an int index for list/str (the message matches the old `AsInt` exactly, for parity).
        let int_idx = |vm: &Vm| -> Result<i64, RuntimeError> {
            match vm.int_val(key) {
                Some(n) => Ok(n),
                None => Err(vm.err(format!("expected int, found {}", vm.type_name(key)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::List(items) => {
                let idx = int_idx(self)?;
                let v = crate::slice::norm_index(idx, items.len()).map(|i| items.get(i));
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("index {idx} out of bounds (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Str(s) => {
                let idx = int_idx(self)?;
                // ASCII arm: a byte index IS a codepoint index, so this is O(1) per subscript.
                // The `None` arm is the pre-change code verbatim and stays O(n) per subscript.
                let (c, n) = match s.ascii_bytes() {
                    Some(b) => {
                        let n = b.len();
                        #[cfg(test)]
                        super::STR_INDEX_CHARS.with(|c| c.set(c.get() + 1));
                        (crate::slice::norm_index(idx, n).map(|i| b[i] as char), n)
                    }
                    None => {
                        let chars: Vec<char> = s.chars().collect();
                        let n = chars.len();
                        #[cfg(test)]
                        super::STR_INDEX_CHARS.with(|c| c.set(c.get() + n));
                        (crate::slice::norm_index(idx, n).map(|i| chars[i]), n)
                    }
                };
                match c {
                    Some(c) => {
                        let nh = self.alloc_char(c);
                        self.push(nh);
                        Ok(())
                    }
                    None => Err(self.err(format!("index {idx} out of bounds (len {n})"), span)),
                }
            }
            Obj::Bytes(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::ByteArray(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::Map(_) => {
                let hk = self.hash_key_rooted(key, &[obj, key], span)?;
                match self.map_probe(h, hk, key, span)? {
                    Some(p) => {
                        let Obj::Map(m) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let v = m.entries[p].2;
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        // Render the key the same way a nested container element renders (quoted
                        // `str`, bare `int`/etc — `stringify_nested_into`), rooted across the call
                        // since a struct/enum key's `str()` display hook can re-enter the VM and GC.
                        self.push(key);
                        let mut ks = String::new();
                        let r = self.stringify_nested_into(&mut ks, key, span, 0);
                        self.pop();
                        r?;
                        Err(self.err(format!("key not found: {ks}"), span))
                    }
                }
            }
            // A struct or enum with the `index` hook dispatches `obj[k]` to `index(self, k)`.
            Obj::Struct { .. } | Obj::Enum { .. } => {
                let Some(v) =
                    self.reentered(|vm| vm.invoke_hook(Hook::Index, obj, &[key], span))?
                else {
                    return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
                };
                self.push(v);
                Ok(())
            }
            Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Tuple(_)
            | Obj::Set(_)
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => {
                Err(self.err(format!("cannot index {}", self.type_name(obj)), span))
            }
        }
    }

    pub(super) fn set_field(
        &mut self,
        name: &str,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let val = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(
                format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                span,
            ));
        };
        // D4 layer C (TICKET-169): a field store on an airlock copy is a lost write, not a race.
        if self.heap.is_copied(h) {
            return Err(self.copied_write_err(None, span));
        }
        // M19 Phase 5b — IC fast path (see [`Vm::get_field`]): a hit on the `tid` guard writes straight
        // to the cached index (no field-name re-verify); a miss falls through to the probe + cache-fill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            // TICKET-262: a value outside the field's width skips the fast path; the probe path
            // below raises the fault. The result matters only on a `tid` hit, so an IC miss on a
            // struct of another layout never faults here.
            let fits =
                cell.tid == TID_NONE || self.field_fits(cell.tid, cell.idx as usize, val).is_ok();
            if cell.tid != TID_NONE
                && fits
                && let Obj::Struct { tid, fields, .. } = self.heap.get_mut(h)
                && *tid == cell.tid
                && let Some(slot) = fields.get_mut(cell.idx as usize)
            {
                *slot = val;
                return Ok(());
            }
        }
        // Positional layout: resolve the field name->index from the StructDef (declaration order)
        // BEFORE the mutable heap borrow, then write the flat `fields` slot by index.
        let stid = match self.heap.get(h) {
            Obj::Struct { tid, .. } => Some(*tid),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        };
        let idx = stid
            .map(|t| self.struct_name_of_tid(t))
            .and_then(|n| self.program.structs.get(n))
            .and_then(|d| d.fields.iter().position(|f| f == name));
        if let (Some(t), Some(i)) = (stid, idx) {
            self.field_fits(t, i, val).map_err(|m| self.err(m, span))?;
        }
        let found;
        match self.heap.get_mut(h) {
            Obj::Struct { tid, fields, .. } => {
                let tid = *tid;
                match idx.and_then(|i| fields.get_mut(i).map(|slot| (i, slot))) {
                    Some((i, slot)) => {
                        *slot = val;
                        found = (i as u32, tid);
                    }
                    None => {
                        let shown = self.display(obj);
                        return Err(self.err(format!("no field '{name}' on {shown}"), span));
                    }
                }
            }
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => {
                return Err(self.err(
                    format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                    span,
                ));
            }
        }
        if ic != NO_IC {
            self.field_ic[ic as usize] = IcCell {
                idx: found.0,
                tid: found.1,
            };
        }
        Ok(())
    }

    pub(super) fn set_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        // The index is NOT pre-validated as int (AsInt removed for map keys): pop as a Value.
        let key = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // D4 layer C (TICKET-169): an index store on an airlock copy is a lost write. A struct
        // receiver is exempt — its `set_index` protocol method runs USER code, which may write
        // only through a `Shared`/`RwShared` field (real sharing); that write is checked at
        // `set_field`/`do_method_call`, not here.
        if self.heap.is_copied(h) && !matches!(self.heap.get(h), Obj::Struct { .. }) {
            return Err(self.copied_write_err(None, span));
        }
        // M19 Tier-2 — Int-key fast path for a Map write: `scalar_hash` on an int needs no rooting
        // (it can't GC or re-enter, unlike a struct key's `hash()`), so skip `hash_key_rooted`. Same
        // `candidates`/`values_equal`/`push` as the general Map arm → byte-identical behavior. A
        // `Struct` with an Int key still falls through to its `set_index` protocol dispatch below.
        if self.is_integral(key) && matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.scalar_hash(key);
            // `val` is an in-flight Rust local: `map_probe` roots the map + key itself, but a user
            // `eq` on a stored key could otherwise collect the value being written.
            let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
            let Obj::Map(m) = self.heap.get_mut(h) else {
                unreachable!()
            };
            match pos {
                Some(i) => m.entries[i].2 = val,
                None => m.push(hk, key, val),
            }
            return Ok(());
        }
        // For a map, hash the key (rooting the map/key/value across a struct key's re-entrant
        // hash()), locate the entry, then mutate — updating the side index on insert.
        if matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.hash_key_rooted(key, &[obj, key, val], span)?;
            let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
            // On INSERT only, snapshot a struct/enum key so a later mutation of the
            // caller's live value can't corrupt the map (Go value-key model). An UPDATE reuses the
            // stored key and pays no clone. `snapshot_key` is pure alloc (no GC), so no rooting.
            match pos {
                Some(i) => {
                    let Obj::Map(m) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    m.entries[i].2 = val;
                }
                None => {
                    // Snapshot a struct/enum key on INSERT only (Go value-key model); pure
                    // alloc (no GC), so `h`/`val` stay valid across it and no rooting is needed.
                    let key = self.snapshot_key(key);
                    let Obj::Map(m) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    m.push(hk, key, val);
                }
            }
            return Ok(());
        }
        // A struct or enum with the `set_index` hook dispatches `obj[k] = v` to it.
        if matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }) {
            if self
                .reentered(|vm| vm.invoke_hook(Hook::SetIndex, obj, &[key, val], span))?
                .is_none()
            {
                return Err(self.err(format!("cannot index-assign {}", self.type_name(obj)), span));
            }
            return Ok(());
        }
        let idx = match self.int_val(key) {
            Some(n) => n,
            None => {
                return Err(self.err(format!("expected int, found {}", self.type_name(key)), span));
            }
        };
        // `bytearray[i] = x` — the NEW mutable capability `bytes` lacks. The value must be an `int`
        // in 0..=255 (validated BEFORE the in-place write); the index must be in range. Both are
        // distinct recoverable faults. Mutation flows through the heap slot, so two bindings to the
        // same `bytearray` observe it (like `list`). Validate the value up front (`&self` borrow)
        // before the `&mut self` `get_mut` below.
        if matches!(self.heap.get(h), Obj::ByteArray(_)) {
            let byte = match self.int_val(val) {
                Some(n) if (0..=255).contains(&n) => n as u8,
                Some(n) => {
                    return Err(self.err(
                        format!("byte value {n} out of range (must be 0..=255)"),
                        span,
                    ));
                }
                None => {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(val)), span)
                    );
                }
            };
            let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                unreachable!()
            };
            return match crate::slice::norm_index(idx, b.len()) {
                Some(i) => {
                    b[i] = byte;
                    Ok(())
                }
                None => {
                    let len = b.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            };
        }
        match self.heap.get_mut(h) {
            Obj::List(items) => match crate::slice::norm_index(idx, items.len()) {
                Some(i) => {
                    items.set(i, val);
                    Ok(())
                }
                None => {
                    let len = items.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            },
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => {
                Err(self.err(format!("cannot index {}", self.type_name(obj)), span))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn match_arm(
        &mut self,
        scrut: usize,
        variant: &str,
        variant_id: u32,
        enum_name: Option<&str>,
        nbind: usize,
        bind_start: usize,
        next: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let v = self.stack[self.base() + scrut];
        let h = match v.as_obj() {
            Some(h) => h,
            None => unreachable!("scrutinee ensured to be an enum"),
        };
        // M19 lever #2 — dispatch is a pure-int compare of the instance's stamped `variant_id` against
        // the arm's compile-time id (no variant-name string compare). `variant` is only the cold error.
        let (mut matches, vid, payload) = match self.heap.get(h).as_enum() {
            Some((vid, payload)) => (vid == variant_id, vid, payload.clone()),
            None => unreachable!("scrutinee ensured to be an enum"),
        };
        // SCRUTINEE-DRIVEN fallback on an id MISS. The compile-time `variant_id` can be the WRONG
        // module's id when a bare match-pattern enum qualifier (`Color.Red`) is reachable only via a
        // whole-module import and TWO whole-imported modules declare the same-named enum — the
        // construction side baked the scrutinee's correct id, but the pattern side may have guessed
        // the other module's id. Resolve from the SCRUTINEE's own `(enum_key, variant)` identity:
        // it matches iff the arm names that variant in the same (bare) enum. Built-in arms carry
        // `enum_name: None` and never enter this branch (pure-int dispatch — zero behavior change).
        if !matches && let Some(en) = enum_name {
            let (ekey, vname) = self.enum_names(vid);
            // Compare the bare display name without allocating (hot match path): strip the
            // `<module-key>::` prefix in place rather than via `bare_display`'s owned String.
            matches = vname == variant && ekey.rsplit("::").next().unwrap_or(ekey) == en;
        }
        if !matches {
            self.jump(next);
            return Ok(());
        }
        if payload.len() != nbind {
            return Err(self.err(
                format!(
                    "pattern '{variant}' binds {nbind} value(s) but variant carries {}",
                    payload.len()
                ),
                span,
            ));
        }
        let base = self.base();
        for (k, pv) in payload.into_iter().enumerate() {
            self.stack[base + bind_start + k] = pv;
        }
        Ok(())
    }

    // ----- builtins / print -----
}
