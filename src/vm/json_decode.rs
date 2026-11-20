//! VM: enum allocation, `json.decode` to values, value-to-string and object reads.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::call::DecodeFail;
use super::*;
use crate::base::json_desc::{DefaultThunk, TypeDescriptor as D};
use crate::native::std_embed::JsonVariant as J;

/// A decode target's descriptor, its field defaults compiled to thunks.
type Desc = D<DefaultThunk>;

/// The slots a decode level keeps on the operand stack before its children: `[src, prev]`, the
/// parsed array or object and the slot of the level below (`-1` under the outermost).
const LEVEL: usize = 2;

/// A decode level as the walk reads it: its slot, its container's descriptor (every `T?`
/// peeled) and the `Some` wrappers its value takes. Rebuilt from the slots when a default's
/// frame returns ([`Vm::json_default_resume`]).
struct Level<'d> {
    at: usize,
    desc: &'d Desc,
    wraps: usize,
}

/// Why a node does not decode; the decode's `Err` text once its path is known
/// ([`Vm::decode_bad`]).
enum Bad {
    NotJson,
    /// expected this, found the variant's kind
    Want(String, J),
    NotInteger(f64),
    OutOfRange(f64),
    /// a tuple of this arity, an array of that length
    Arity(usize, usize),
}

/// `d` under every `T?`, and how many there were.
fn peel(mut d: &Desc) -> (&Desc, usize) {
    let mut n = 0;
    while let D::Option(inner) = d {
        d = inner;
        n += 1;
    }
    (d, n)
}

/// The descriptor of container `d`'s child `i`.
fn child_desc(d: &Desc, i: usize) -> &Desc {
    match d {
        D::List(inner) | D::Map(inner) => inner,
        D::Tuple(elems) => &elems[i],
        D::Struct { fields, .. } => &fields[i].desc,
        D::Int | D::Float | D::Bool | D::Str | D::Option(_) => {
            unreachable!("a decode level is a container")
        }
    }
}

impl Vm {
    /// Build a NATIVE `Result`/`Option` enum instance (the native / std construction path: `Ok`/`Err`/
    /// `Some`/`None`, list `pop`, regex/request/json/fs returns). M19 lever #2 — stamps the FIXED native
    /// `VID_OK`/`VID_ERR`/`VID_SOME`/`VID_NONE_VARIANT` constant DIRECTLY, never a name lookup through
    /// `Program::variants`: a user enum declaring a variant named `Ok`/`Err`/`Some`/`None` SHADOWS that
    /// name in the `variants` map (its own dense id at `4..`), so resolving by name here would stamp the
    /// user's id onto a genuine native value — collapsing native-vs-user identity (broken `==`) and
    /// missing `?`'s `variant_id == VID_SOME`/`VID_OK` gate. The reserved 0..=3 ids are disjoint from
    /// every user id, so stamping the constant keeps native and user variants distinguishable. `ty` is
    /// retained in the signature so call sites read self-documentingly, but it is not stored.
    pub(super) fn alloc_enum(&mut self, ty: &str, variant: &str, payload: &[Value]) -> Value {
        let _ = ty;
        use crate::base::op::{VID_ERR, VID_NONE, VID_NONE_VARIANT, VID_OK, VID_SOME};
        let variant_id = match variant {
            "Ok" => VID_OK,
            "Err" => VID_ERR,
            "Some" => VID_SOME,
            "None" => VID_NONE_VARIANT,
            // `alloc_enum` is the NATIVE construction path; it is only ever called with the four
            // reserved names above. The fallback is defensive only.
            _ => VID_NONE,
        };
        Value::obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload: RVec::from_slice(payload),
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`. A field default runs
    /// as a frame at its field ([`Vm::coerce_json`]); its return resumes the decode
    /// ([`Vm::json_default_resume`]).
    pub(super) fn json_decode(&mut self, desc: &Desc, span: Span) -> Result<(), RuntimeError> {
        // `res` stays on the operand stack while the coercion runs: a field default's frame can
        // park, and the parsed tree must survive a collection. `base - 1` is `res`'s slot.
        let res = self.stack[self.stack.len() - 1];
        let base = self.stack.len();
        let bad = "decode: parse did not return a `T!E` value".to_string();
        let Some((rty, variant, payload)) = self.enum_parts(res) else {
            self.stack.truncate(base - 1);
            return Err(self.err(bad, span));
        };
        if rty != "Result" {
            self.stack.truncate(base - 1);
            return Err(self.err(bad, span));
        }
        match variant.as_str() {
            // a Result Err(str) is already a valid Result[T]; it stays where it is
            "Err" => Ok(()),
            "Ok" if payload.len() == 1 => {
                let mut levels = Vec::new();
                let r = match self.decode_enter(payload[0], desc, &mut levels) {
                    Ok(None) => self.coerce_json(&mut levels, None, span),
                    r @ (Ok(Some(_)) | Err(_)) => r,
                };
                self.decode_end(r, base)
            }
            _ => {
                self.stack.truncate(base - 1);
                Err(self.err(bad, span))
            }
        }
    }

    /// The end of a decode whose levels start at `base` (`res` at `base - 1`): push its
    /// `Result[T]` in `res`'s place, or leave a parked decode (`Ok(None)`) as it is.
    fn decode_end(
        &mut self,
        r: Result<Option<Value>, DecodeFail>,
        base: usize,
    ) -> Result<(), RuntimeError> {
        let out = match r {
            Ok(None) => return Ok(()),
            Ok(Some(v)) => self.alloc_enum("Result", "Ok", &[v]),
            Err(DecodeFail::Msg(msg)) => {
                let s = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", &[s])
            }
            Err(DecodeFail::Fault(e)) => {
                self.stack.truncate(base - 1);
                return Err(e);
            }
        };
        self.stack.truncate(base - 1);
        self.push(out);
        Ok(())
    }

    /// A field default's frame returned ([`RetAction::JsonDefault`]): its value on the stack top,
    /// over the slot of the decode's top level. Rebuild the levels from their slots and the
    /// descriptor of the caller's `Op::JsonDecode`, and run the decode on from that field.
    pub(super) fn json_default_resume(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        let top = self.pop().as_int() as usize;
        let program = Arc::clone(&self.program);
        let caller = self.frames.last().expect("a default returns to its decode");
        let Op::JsonDecode(root) = &program.protos[caller.proto].code[caller.ip - 1] else {
            unreachable!("a default frame is pushed only by Op::JsonDecode")
        };
        let mut ats = Vec::new();
        let mut at = top as i64;
        while at >= 0 {
            ats.push(at as usize);
            at = self.stack[at as usize + 1].as_int();
        }
        let mut levels: Vec<Level> = Vec::with_capacity(ats.len());
        for &at in ats.iter().rev() {
            let child = match levels.last() {
                Some(p) => child_desc(p.desc, at - p.at - LEVEL),
                None => root,
            };
            let (desc, wraps) = peel(child);
            levels.push(Level { at, desc, wraps });
        }
        let base = levels[0].at;
        let r = self.coerce_json(&mut levels, Some(v), span);
        self.decode_end(r, base)
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    pub(super) fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        let h = v.as_obj()?;
        let (variant_id, payload) = self.heap.get(h).as_enum()?;
        // M19 lever #2 — cold path: resolve the type + variant names from the id.
        let (ty, variant) = self.enum_names(variant_id);
        Some((ty.to_string(), variant.to_string(), payload.to_vec()))
    }

    /// The `Json` variant of a parsed value and its payload's first value; `None` if it is not
    /// a `Json` value.
    fn json_parts(&self, jv: Value) -> Option<(J, Value)> {
        let (variant_id, payload) = self.heap.get(jv.as_obj()?).as_enum()?;
        let variant = J::from_name(self.enum_names(variant_id).1)?;
        let p0 = match payload.len() {
            0 => Value::nil(),
            _ => payload.get(0),
        };
        Some((variant, p0))
    }

    /// The child count of a parsed array (a list) or object (a map) payload, else `None`.
    fn json_len(&self, p0: Value) -> Option<usize> {
        let o = self.heap.get(p0.as_obj()?);
        (o.as_list().map(|items| items.len())).or_else(|| o.as_map().map(|m| m.entries.len()))
    }

    /// Run a missing field's default, as `S(...)` runs it for an omitted field: afresh each time,
    /// homed in the declaring module, as a frame over the slot of the top level `top`, so a wait
    /// in it parks (`Ok(None)`). A fault inside it is a fault, never a decode `Err`.
    fn decode_default(
        &mut self,
        t: DefaultThunk,
        top: usize,
        span: Span,
    ) -> Result<Option<Value>, DecodeFail> {
        let Some(home) = self.module_objs.get(t.module) else {
            return Err(DecodeFail::Fault(self.err(
                "json.decode: the module that declares this default has not been initialized yet"
                    .to_string(),
                span,
            )));
        };
        // The per-default cancel point: the decode loop's back-edge.
        self.guarded_checkpoint().map_err(DecodeFail::Fault)?;
        self.frame_depth_guard(true, span)
            .map_err(DecodeFail::Fault)?;
        self.push(Value::int(top as i64));
        let base = self.stack.len();
        self.finish_frame(
            t.proto,
            home,
            None,
            base,
            true,
            false,
            span,
            RetAction::JsonDefault,
        );
        Ok(None)
    }

    /// The decode of `jv` against `desc`, under `levels` (empty at the root): its value, or
    /// `Ok(None)` when `jv` is a container, whose level is now on top.
    fn decode_enter<'d>(
        &mut self,
        jv: Value,
        desc: &'d Desc,
        levels: &mut Vec<Level<'d>>,
    ) -> Result<Option<Value>, DecodeFail> {
        let Some((variant, p0)) = self.json_parts(jv) else {
            return Err(self.decode_bad(levels, Bad::NotJson));
        };
        // `T?`: `null` is None; anything else decodes as `T`, wrapped in `Some`.
        if variant == J::Null && matches!(desc, D::Option(_)) {
            return Ok(Some(self.alloc_enum("Option", "None", &[])));
        }
        let (desc, wraps) = peel(desc);
        // What the node must be, when it is not.
        let want = match desc {
            D::Int | D::Float | D::Bool | D::Str => {
                let v = match self.json_scalar(variant, p0, desc) {
                    Ok(v) => v,
                    Err(b) => return Err(self.decode_bad(levels, b)),
                };
                return Ok(Some(self.wrap_some(v, wraps)));
            }
            D::List(_) => match (variant, self.json_len(p0)) {
                (J::Arr, Some(_)) => None,
                _ => Some("array".to_string()),
            },
            D::Tuple(elems) => match (variant, self.json_len(p0)) {
                (J::Arr, Some(n)) if n != elems.len() => {
                    return Err(self.decode_bad(levels, Bad::Arity(elems.len(), n)));
                }
                (J::Arr, Some(_)) => None,
                _ => Some("array".to_string()),
            },
            D::Map(_) => match (variant, self.json_len(p0)) {
                (J::Obj, Some(_)) => None,
                _ => Some("object".to_string()),
            },
            // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
            D::Struct { display, .. } if variant != J::Obj => Some(format!("object for {display}")),
            D::Struct { .. } => match self.json_len(p0) {
                Some(_) => None,
                None => Some("object".to_string()),
            },
            D::Option(_) => unreachable!("peel removes every Option"),
        };
        if let Some(want) = want {
            return Err(self.decode_bad(levels, Bad::Want(want, variant)));
        }
        let prev = levels.last().map_or(-1, |l| l.at as i64);
        let at = self.stack.len();
        self.stack.extend([p0, Value::int(prev)]);
        levels.push(Level { at, desc, wraps });
        Ok(None)
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type: the decode
    /// walk over `levels`, on the operand stack, from `done`, the value of the top level's next
    /// child (`None` to pick it). Each level is `[src, prev]` and then its decoded children,
    /// rooted until their container owns them. `Ok(None)`: a field default's frame was pushed
    /// ([`Vm::decode_default`]); its return resumes the walk at that field.
    fn coerce_json<'d>(
        &mut self,
        levels: &mut Vec<Level<'d>>,
        mut done: Option<Value>,
        span: Span,
    ) -> Result<Option<Value>, DecodeFail> {
        loop {
            let Some(top) = levels.last() else {
                return Ok(done);
            };
            let (at, desc) = (top.at, top.desc);
            if let Some(v) = done.take() {
                self.push(v);
            }
            let i = self.stack.len() - at - LEVEL;
            let src = self.stack[at].as_gcref();
            let child = match desc {
                D::List(_) | D::Tuple(_) => match self.heap.get(src).as_list() {
                    Some(items) if i < items.len() => Some(items.get(i)),
                    Some(_) | None => None,
                },
                D::Map(_) => match self.heap.get(src).as_map() {
                    Some(m) if i < m.entries.len() => Some(m.entries.get(i).2),
                    Some(_) | None => None,
                },
                D::Struct { fields, .. } => match fields.get(i) {
                    None => None,
                    Some(field) => match self.json_field(src, &field.name) {
                        Some(jv) => Some(jv),
                        // A missing key takes the field's default, as `S(...)` does; with no
                        // default an `Option` field is None and anything else is an error.
                        None => {
                            done = match (&field.default, &field.desc) {
                                (Some(t), _) => match self.decode_default(*t, at, span)? {
                                    Some(v) => Some(v),
                                    None => return Ok(None),
                                },
                                (None, D::Option(_)) => {
                                    Some(self.alloc_enum("Option", "None", &[]))
                                }
                                (None, _) => {
                                    let path = self.decode_path(levels, false);
                                    return Err(format!(
                                        "decode: missing key '{}' at {path}",
                                        field.name
                                    )
                                    .into());
                                }
                            };
                            continue;
                        }
                    },
                },
                D::Int | D::Float | D::Bool | D::Str | D::Option(_) => {
                    unreachable!("a decode level is a container")
                }
            };
            done = match child {
                Some(jv) => self.decode_enter(jv, child_desc(desc, i), levels)?,
                None => Some(self.decode_finish(levels)?),
            };
        }
    }

    /// The top level's container, from its children; the level goes.
    fn decode_finish(&mut self, levels: &mut Vec<Level<'_>>) -> Result<Value, DecodeFail> {
        let top = levels.last().expect("a level to finish");
        let (at, desc, wraps) = (top.at, top.desc, top.wraps);
        let src = self.stack[at].as_gcref();
        let v = match desc {
            D::List(_) => {
                let out = self.stack.split_off(at + LEVEL);
                Value::obj(self.heap.alloc(Obj::List(out.into())))
            }
            D::Tuple(_) => {
                let out = self.stack.split_off(at + LEVEL);
                Value::obj(self.heap.alloc(Obj::Tuple(out)))
            }
            D::Map(_) => {
                let vals = self.stack.split_off(at + LEVEL);
                // str keys unchanged → reuse the cached hash; the keys belong to the parsed
                // tree, which `json_decode` keeps rooted.
                let out: Vec<MapEntry> = match self.heap.get(src).as_map() {
                    Some(m) => (m.entries.iter())
                        .zip(vals)
                        .map(|((hk, k, _), coerced)| (hk, k, coerced))
                        .collect(),
                    None => unreachable!("a map level's source is an object"),
                };
                Value::obj(self.heap.alloc(Obj::Map(MapData::from_entries(out))))
            }
            D::Struct { key, fields, .. } => {
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (so downstream
                // field/method lookups + `struct_tid` hit the right layout); display renders bare.
                let tid = self.struct_tid(key);
                // TICKET-262: a JSON number outside a width field is the decode's `Err`, as Go's
                // `json.Unmarshal` returns for `int8`; the text is the C boundary's.
                for (i, field) in fields.iter().enumerate() {
                    if let Err(m) = self.field_fits(tid, i, self.stack[at + LEVEL + i]) {
                        let path = self.decode_path(levels, false);
                        return Err(format!("decode: {m} at {path}.{}", field.name).into());
                    }
                }
                // Positional layout: `fields` is in the struct's declaration order (see
                // `json_decode::struct_descriptor`), and so are the values.
                let field_vals = self.stack.split_off(at + LEVEL);
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(field_vals),
                });
                Value::obj(h)
            }
            D::Int | D::Float | D::Bool | D::Str | D::Option(_) => {
                unreachable!("a decode level is a container")
            }
        };
        self.stack.truncate(at);
        levels.pop();
        Ok(self.wrap_some(v, wraps))
    }

    /// `v` in `n` `Some` wrappers, innermost first.
    fn wrap_some(&mut self, mut v: Value, n: usize) -> Value {
        for _ in 0..n {
            v = self.alloc_enum("Option", "Some", &[v]);
        }
        v
    }

    /// The value of the parsed object `src`'s key `name`, if it has one.
    fn json_field(&self, src: GcRef, name: &str) -> Option<Value> {
        let m = self.heap.get(src).as_map()?;
        m.entries
            .iter()
            .find(|(_, k, _)| self.val_str(*k).as_deref() == Some(name))
            .map(|(_, _, v)| v)
    }

    /// The JSON-pointer-ish breadcrumb of the top level's node (`child`: of its next child) for
    /// error messages: `$`, then `[i]` per array element and `.key` per object key.
    fn decode_path(&self, levels: &[Level<'_>], child: bool) -> String {
        use std::fmt::Write as _;
        let mut path = String::from("$");
        let n = levels.len() - usize::from(!child);
        for (k, l) in levels[..n].iter().enumerate() {
            let end = levels.get(k + 1).map_or(self.stack.len(), |x| x.at);
            let i = end - l.at - LEVEL;
            let _ = match l.desc {
                D::List(_) | D::Tuple(_) => write!(path, "[{i}]"),
                D::Map(_) => {
                    let key = match self.heap.get(self.stack[l.at].as_gcref()).as_map() {
                        Some(m) => self.val_str(m.entries.get(i).1).unwrap_or_default(),
                        None => String::new(),
                    };
                    write!(path, ".{key}")
                }
                D::Struct { fields, .. } => write!(path, ".{}", fields[i].name),
                D::Int | D::Float | D::Bool | D::Str | D::Option(_) => {
                    unreachable!("a decode level is a container")
                }
            };
        }
        path
    }

    /// The decode's `Err` for the node being entered under `levels`.
    fn decode_bad(&self, levels: &[Level<'_>], bad: Bad) -> DecodeFail {
        let path = self.decode_path(levels, true);
        DecodeFail::Msg(match bad {
            Bad::NotJson => format!("decode: expected a JSON value at {path}"),
            Bad::Want(want, variant) => format!(
                "decode: expected {want} at {path}, found {}",
                crate::base::json_desc::json_kind(variant)
            ),
            Bad::NotInteger(f) => format!("decode: expected an integer at {path}, found {f}"),
            Bad::OutOfRange(f) => {
                format!("decode: integer {f} at {path} is out of range for int")
            }
            Bad::Arity(want, n) => {
                format!("decode: expected an array of {want} elements at {path}, found {n}")
            }
        })
    }

    /// A scalar's decode: `variant` and its payload `p0` as `desc`'s type.
    fn json_scalar(&mut self, variant: J, p0: Value, desc: &Desc) -> Result<Value, Bad> {
        let mismatch = |want: &str| Bad::Want(want.to_string(), variant);
        let payload = [p0];
        match desc {
            D::Int => {
                // `Json.Int` carries its exact i64 payload (TICKET-013, W8-35) — take it directly,
                // never through f64, so a 19-digit id decodes byte-exact.
                if variant == J::Int {
                    let n = self.int_val(p0).ok_or_else(|| mismatch("int"))?;
                    return Ok(self.make_int(n));
                }
                let f = self
                    .json_num(variant, &payload)
                    .ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(Bad::NotInteger(f));
                }
                // `f as i64` saturates, so range-check first. Use a strict `> 2^63` upper bound so
                // i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating
                // cast, while everything strictly above 2^63 / below -2^63 is rejected.
                if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 {
                    return Err(Bad::OutOfRange(f));
                }
                Ok(self.make_int(f as i64))
            }
            D::Float => {
                let f = self
                    .json_num(variant, &payload)
                    .ok_or_else(|| mismatch("float"))?;
                Ok(self.box_float(f))
            }
            D::Bool => match (variant, p0.as_bool()) {
                (J::Bool, Some(b)) => Ok(Value::bool(b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => {
                if variant == J::Str {
                    let s = self.val_str(p0).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str"))
                }
            }
            D::List(_) | D::Tuple(_) | D::Map(_) | D::Struct { .. } | D::Option(_) => {
                unreachable!("a scalar descriptor")
            }
        }
    }

    /// The `f64` of a JSON `Num` or `Int`, else `None`.
    pub(super) fn json_num(
        &self,
        variant: crate::native::std_embed::JsonVariant,
        payload: &[Value],
    ) -> Option<f64> {
        use crate::native::std_embed::JsonVariant as J;
        if matches!(variant, J::Num | J::Int) {
            payload.first().and_then(|&v| {
                if v.is_float() {
                    Some(self.float_of(v))
                } else {
                    self.int_val(v).map(|n| n as f64)
                }
            })
        } else {
            None
        }
    }

    /// Depth cap for `json.encode`'s runtime walk (`json_of`) — independent of `stringify`'s own
    /// cap in `std/json.chz`, which only guards a `Json` tree that already exists. A Chezzi struct
    /// is a reference value and may be cyclic, so this is what stops `json.encode` on one from
    /// recursing forever. Tied to `std/json.chz`'s `MAX_NEST_DEPTH`.
    const JSON_ENCODE_MAX_DEPTH: usize = 2000;

    /// Enter one more bracket level of `json.encode`'s walk, counting exactly as `json.parse`/
    /// `json.stringify` do: the outermost container is level 1, and only a value that actually
    /// emits a JSON bracket (`List`/`Tuple`/`Map`/`Struct`) calls this. A scalar, `None`, an
    /// already-built `Json` value and a `Some` wrapper add no level (W13-19).
    fn json_enter(depth: usize) -> Result<usize, String> {
        if depth >= Self::JSON_ENCODE_MAX_DEPTH {
            Err("json.encode: exceeded max depth".to_string())
        } else {
            Ok(depth + 1)
        }
    }

    /// `Op::JsonToValue`: pop one runtime value, push the `Json` tree that `std.json`'s own
    /// `stringify` then renders — `json.encode(x)` is `stringify(_to_json(x))` in Chezzi.
    pub(super) fn json_to_value(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        match self.json_of(v, 0) {
            Ok(jv) => {
                self.push(jv);
                Ok(())
            }
            Err(msg) => Err(self.err(msg, span)),
        }
    }

    /// Build a `Json.<variant>` enum value, keyed by `Program::variants[("Json", variant)]` (the
    /// runtime key `std.json` registers itself under — bare, as every std type is). Only
    /// `std.json`'s own `_to_json` emits the op that reaches here, so a miss is an internal fault.
    fn json_variant(
        &mut self,
        variant: crate::native::std_embed::JsonVariant,
        payload: Vec<Value>,
    ) -> Result<Value, String> {
        let variant_id = self
            .program
            .variants
            .get(&("Json".to_string(), variant.name().to_string()))
            .map(|d| d.variant_id)
            .ok_or_else(|| format!("internal: Json.{} is not registered", variant.name()))?;
        Ok(Value::obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload: payload.into(),
        })))
    }

    /// The `Arr` node of a list's or a tuple's elements, one bracket level down.
    fn json_arr(&mut self, items: Vec<Value>, depth: usize) -> Result<Value, String> {
        let inner = Self::json_enter(depth)?;
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            out.push(self.json_of(it, inner)?);
        }
        let lv = Value::obj(self.heap.alloc(Obj::List(out.into())));
        self.json_variant(crate::native::std_embed::JsonVariant::Arr, vec![lv])
    }

    /// The runtime walk behind `json.encode`: a `Value` -> the `Json` tree that mirrors its shape.
    /// Accumulates children into a plain `Vec<Value>`/`MapData`, same allocation idiom as
    /// `coerce_json` (no new rooting scheme).
    fn json_of(&mut self, v: Value, depth: usize) -> Result<Value, String> {
        use crate::base::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
        use crate::native::std_embed::JsonVariant as J;
        // Peel every `Some` wrapper iteratively: it emits no JSON bracket, so it must charge
        // neither a bracket level (W13-19) nor a Rust stack frame (a recursive arm here would let
        // an `Option`-linked chain reach two Rust frames per bracket level, defeating the reason
        // DEC-110 wanted a depth guard at all).
        let mut v = v;
        while let Some(h) = v.as_obj() {
            let Obj::Enum {
                variant_id,
                payload,
            } = self.heap.get(h)
            else {
                break;
            };
            if *variant_id != VID_SOME {
                break;
            }
            v = payload.get(0);
        }
        match v.view() {
            ValueView::Nil => self.json_variant(J::Null, Vec::new()),
            ValueView::Bool(b) => self.json_variant(J::Bool, vec![Value::bool(b)]),
            ValueView::Int(n) => self.json_variant(J::Int, vec![Value::int(n)]),
            ValueView::Obj(h) => match self.heap.get(h).clone() {
                Obj::FloatBox(f) => {
                    let fv = self.box_float(f);
                    self.json_variant(J::Num, vec![fv])
                }
                Obj::BigInt(n) => {
                    let iv = self.make_int(n);
                    self.json_variant(J::Int, vec![iv])
                }
                Obj::Str(s) => {
                    let sv = self.alloc_str(s.to_string());
                    self.json_variant(J::Str, vec![sv])
                }
                Obj::List(items) => self.json_arr(items.to_vec(), depth),
                Obj::Tuple(items) => self.json_arr(items, depth),
                Obj::Map(m) => {
                    let inner = Self::json_enter(depth)?;
                    let out = MapData::default();
                    for (hk, k, val) in &m.entries {
                        if self.val_str(k).is_none() {
                            return Err(format!(
                                "json.encode: object keys must be str, found {}",
                                self.type_name(k)
                            ));
                        }
                        let jv = self.json_of(val, inner)?;
                        out.push(&self.heap, hk, k, jv);
                    }
                    let mv = Value::obj(self.heap.alloc(Obj::Map(out)));
                    self.json_variant(J::Obj, vec![mv])
                }
                Obj::Struct { tid, fields } => {
                    let inner = Self::json_enter(depth)?;
                    let key = self.struct_name_of_tid(tid).to_string();
                    let field_names = self
                        .program
                        .structs
                        .get(&key)
                        .map(|d| d.fields.clone())
                        .unwrap_or_default();
                    let field_vals: Vec<Value> = fields.to_vec();
                    let out = MapData::default();
                    for (fname, fval) in field_names.into_iter().zip(field_vals) {
                        let key_v = self.alloc_str(fname);
                        let hk = self.scalar_hash(key_v);
                        let jv = self.json_of(fval, inner)?;
                        out.push(&self.heap, hk, key_v, jv);
                    }
                    let mv = Value::obj(self.heap.alloc(Obj::Map(out)));
                    self.json_variant(J::Obj, vec![mv])
                }
                Obj::Enum {
                    variant_id,
                    payload: _,
                } => {
                    let (ty, _) = self.enum_names(variant_id);
                    if ty == "Json" {
                        return Ok(v);
                    }
                    match variant_id {
                        // `VID_SOME` is peeled at the top of this fn, before this match.
                        VID_NONE_VARIANT => self.json_variant(J::Null, Vec::new()),
                        VID_OK | VID_ERR => {
                            Err(format!("json.encode: cannot encode {}", self.type_name(v)))
                        }
                        _ => Err(format!("json.encode: cannot encode enum {ty}")),
                    }
                }
                Obj::Bytes(_)
                | Obj::ByteArray(_)
                | Obj::Iter { .. }
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
                    Err(format!("json.encode: cannot encode {}", self.type_name(v)))
                }
            },
        }
    }

    /// The owned text of a str value, else `None`.
    pub(super) fn val_str(&self, v: Value) -> Option<String> {
        let h = v.as_obj()?;
        match self.heap.get(h) {
            Obj::Str(s) => Some(s.to_string()),
            Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
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
            | Obj::Generator(_) => None,
        }
    }
}
