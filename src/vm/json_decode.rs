//! VM: enum allocation, `json.decode` to values, value-to-string and object reads.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::call::DecodeFail;
use super::*;

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
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    pub(super) fn json_decode(
        &mut self,
        desc: &crate::base::json_desc::TypeDescriptor<crate::base::json_desc::DefaultThunk>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // `res` stays on the operand stack while the coercion runs: a field default re-enters the
        // VM, which collects, and the parsed tree must survive it. `base - 1` is `res`'s slot.
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
                let jv = payload[0];
                let r = self.coerce_json(jv, desc, "$", span);
                self.stack.truncate(base - 1);
                match r {
                    Ok(v) => {
                        let r = self.alloc_enum("Result", "Ok", &[v]);
                        self.push(r);
                    }
                    Err(DecodeFail::Msg(msg)) => {
                        let s = self.alloc_str(msg);
                        let r = self.alloc_enum("Result", "Err", &[s]);
                        self.push(r);
                    }
                    Err(DecodeFail::Fault(e)) => return Err(e),
                }
                Ok(())
            }
            _ => {
                self.stack.truncate(base - 1);
                Err(self.err(bad, span))
            }
        }
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    pub(super) fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        let h = v.as_obj()?;
        let (variant_id, payload) = self.heap.get(h).as_enum()?;
        // M19 lever #2 — cold path: resolve the type + variant names from the id.
        let (ty, variant) = self.enum_names(variant_id);
        Some((ty.to_string(), variant.to_string(), payload.to_vec()))
    }

    /// Run a missing field's default thunk, as `S(...)` runs it for an omitted field: afresh each
    /// time, homed in the declaring module. A fault inside it is a fault, never a decode `Err`.
    fn run_decode_default(
        &mut self,
        t: crate::base::json_desc::DefaultThunk,
        span: Span,
    ) -> Result<Value, DecodeFail> {
        let Some(&home) = self.module_objs.get(t.module) else {
            return Err(DecodeFail::Fault(self.err(
                "json.decode: the module that declares this default has not been initialized yet"
                    .to_string(),
                span,
            )));
        };
        let proto = t.proto;
        let f = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
        self.guarded(|vm| vm.invoke_value(f, Vec::new(), span))
            .map_err(DecodeFail::Fault)
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. `path` is a
    /// JSON-pointer-ish breadcrumb for error messages.
    pub(super) fn coerce_json(
        &mut self,
        jv: Value,
        desc: &crate::base::json_desc::TypeDescriptor<crate::base::json_desc::DefaultThunk>,
        path: &str,
        span: Span,
    ) -> Result<Value, DecodeFail> {
        use crate::base::json_desc::TypeDescriptor as D;
        let (_jty, variant, payload) = self
            .enum_parts(jv)
            .ok_or_else(|| format!("decode: expected a JSON value at {path}"))?;
        let mismatch = |want: &str| {
            format!(
                "decode: expected {want} at {path}, found {}",
                crate::base::json_desc::json_kind(&variant)
            )
        };
        match desc {
            D::Int => {
                // `Json.Int` carries its exact i64 payload (TICKET-013, W8-35) — take it directly,
                // never through f64, so a 19-digit id decodes byte-exact.
                if variant == "Int" {
                    let n = self.int_val(payload[0]).ok_or_else(|| mismatch("int"))?;
                    return Ok(self.make_int(n));
                }
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}").into());
                }
                // `f as i64` saturates, so range-check first. Use a strict `> 2^63` upper bound so
                // i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating
                // cast, while everything strictly above 2^63 / below -2^63 is rejected.
                if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 {
                    return Err(
                        format!("decode: integer {f} at {path} is out of range for int").into(),
                    );
                }
                Ok(self.make_int(f as i64))
            }
            D::Float => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("float"))?;
                Ok(self.box_float(f))
            }
            D::Bool => match (variant.as_str(), payload.first().and_then(|v| v.as_bool())) {
                ("Bool", Some(b)) => Ok(Value::bool(b)),
                _ => Err(mismatch("bool").into()),
            },
            D::Str => {
                if variant == "Str" {
                    let s = self.val_str(payload[0]).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str").into())
                }
            }
            D::Option(inner) => {
                if variant == "Null" {
                    Ok(self.alloc_enum("Option", "None", &[]))
                } else {
                    let v = self.coerce_json(jv, inner, path, span)?;
                    Ok(self.alloc_enum("Option", "Some", &[v]))
                }
            }
            D::List(inner) => {
                if variant != "Arr" {
                    return Err(mismatch("array").into());
                }
                let items = match self.heap.get(self.as_obj(payload[0])).as_list() {
                    Some(items) => items.clone(),
                    None => return Err(mismatch("array").into()),
                };
                // Each decoded element is rooted on the operand stack until the list owns it: a
                // later element's field default can collect (`json_decode` truncates on a failure).
                let child_base = self.stack.len();
                for (i, it) in items.into_iter().enumerate() {
                    let v = self.coerce_json(it, inner, &format!("{path}[{i}]"), span)?;
                    self.push(v);
                }
                let out = self.stack.split_off(child_base);
                Ok(Value::obj(self.heap.alloc(Obj::List(out.into()))))
            }
            D::Tuple(elems) => {
                if variant != "Arr" {
                    return Err(mismatch("array").into());
                }
                let items = match self.heap.get(self.as_obj(payload[0])).as_list() {
                    Some(items) => items.clone(),
                    None => return Err(mismatch("array").into()),
                };
                if items.len() != elems.len() {
                    return Err(format!(
                        "decode: expected an array of {} elements at {path}, found {}",
                        elems.len(),
                        items.len()
                    )
                    .into());
                }
                // Rooted on the operand stack, as in the list arm.
                let child_base = self.stack.len();
                for (i, (it, d)) in items.into_iter().zip(elems).enumerate() {
                    let v = self.coerce_json(it, d, &format!("{path}[{i}]"), span)?;
                    self.push(v);
                }
                let out = self.stack.split_off(child_base);
                Ok(Value::obj(self.heap.alloc(Obj::Tuple(out))))
            }
            D::Map(inner) => {
                if variant != "Obj" {
                    return Err(mismatch("object").into());
                }
                let entries = match self.heap.get(self.as_obj(payload[0])).as_map() {
                    Some(m) => m.entries.to_vec(),
                    None => return Err(mismatch("object").into()),
                };
                // Values are rooted on the operand stack, as in the list arm; the keys belong to
                // the parsed tree, which `json_decode` keeps rooted.
                let child_base = self.stack.len();
                let mut keys = Vec::with_capacity(entries.len());
                for (hk, k, v) in entries {
                    let key = self.val_str(k).unwrap_or_default();
                    let coerced = self.coerce_json(v, inner, &format!("{path}.{key}"), span)?;
                    self.push(coerced);
                    keys.push((hk, k));
                }
                // str keys unchanged → reuse the cached hash
                let out: Vec<MapEntry> = (keys.into_iter())
                    .zip(self.stack.split_off(child_base))
                    .map(|((hk, k), coerced)| (hk, k, coerced))
                    .collect();
                Ok(Value::obj(
                    self.heap.alloc(Obj::Map(MapData::from_entries(out))),
                ))
            }
            D::Struct {
                key,
                display,
                fields,
            } => {
                if variant != "Obj" {
                    // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
                    return Err(mismatch(&format!("object for {display}")).into());
                }
                let entries = match self.heap.get(self.as_obj(payload[0])).as_map() {
                    Some(m) => m.entries.to_vec(),
                    None => return Err(mismatch("object").into()),
                };
                // Positional layout: `fields` (the type descriptor) is already in the struct's
                // declaration order (see `json_decode::struct_descriptor`), so push values in order.
                // Each value is rooted on the operand stack, as in the list arm: a later field's
                // default can collect.
                let child_base = self.stack.len();
                for field in fields {
                    let found = entries
                        .iter()
                        .find(|(_, k, _)| self.val_str(*k).as_deref() == Some(field.name.as_str()));
                    let fpath = format!("{path}.{}", field.name);
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, &field.desc, &fpath, span)?,
                        // A missing key takes the field's default, as `S(...)` does; with no
                        // default an `Option` field is None and anything else is an error.
                        None => match (&field.default, &field.desc) {
                            (Some(t), _) => self.run_decode_default(*t, span)?,
                            (None, D::Option(_)) => self.alloc_enum("Option", "None", &[]),
                            (None, _) => {
                                return Err(format!(
                                    "decode: missing key '{}' at {path}",
                                    field.name
                                )
                                .into());
                            }
                        },
                    };
                    self.push(v);
                }
                let field_vals = self.stack.split_off(child_base);
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (so downstream
                // field/method lookups + `struct_tid` hit the right layout); display renders bare.
                let tid = self.struct_tid(key);
                // TICKET-262: a JSON number outside a width field is the decode's `Err`, as Go's
                // `json.Unmarshal` returns for `int8`; the text is the C boundary's.
                for (i, (v, field)) in field_vals.iter().zip(fields).enumerate() {
                    self.field_fits(tid, i, *v)
                        .map_err(|m| format!("decode: {m} at {path}.{}", field.name))?;
                }
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(field_vals),
                });
                Ok(Value::obj(h))
            }
        }
    }

    /// The `f64` of a JSON `Num` or `Int`, else `None`.
    pub(super) fn json_num(&self, variant: &str, payload: &[Value]) -> Option<f64> {
        if variant == "Num" || variant == "Int" {
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
    /// runtime key `std.json` registers itself under — bare, since it stays a non-native module).
    /// A missing key means `std.json` was never loaded, e.g. `json._to_json` reached through some
    /// other module.
    fn json_variant(&mut self, variant: &str, payload: Vec<Value>) -> Result<Value, String> {
        let variant_id = self
            .program
            .variants
            .get(&("Json".to_string(), variant.to_string()))
            .map(|d| d.variant_id)
            .ok_or_else(|| "json.encode: std.json is not loaded".to_string())?;
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
        self.json_variant("Arr", vec![lv])
    }

    /// The runtime walk behind `json.encode`: a `Value` -> the `Json` tree that mirrors its shape.
    /// Accumulates children into a plain `Vec<Value>`/`MapData`, same allocation idiom as
    /// `coerce_json` (no new rooting scheme).
    fn json_of(&mut self, v: Value, depth: usize) -> Result<Value, String> {
        use crate::base::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
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
            ValueView::Nil => self.json_variant("Null", Vec::new()),
            ValueView::Bool(b) => self.json_variant("Bool", vec![Value::bool(b)]),
            ValueView::Int(n) => self.json_variant("Int", vec![Value::int(n)]),
            ValueView::Obj(h) => match self.heap.get(h).clone() {
                Obj::FloatBox(f) => {
                    let fv = self.box_float(f);
                    self.json_variant("Num", vec![fv])
                }
                Obj::BigInt(n) => {
                    let iv = self.make_int(n);
                    self.json_variant("Int", vec![iv])
                }
                Obj::Str(s) => {
                    let sv = self.alloc_str(s.to_string());
                    self.json_variant("Str", vec![sv])
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
                    self.json_variant("Obj", vec![mv])
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
                    self.json_variant("Obj", vec![mv])
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
                        VID_NONE_VARIANT => self.json_variant("Null", Vec::new()),
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

    /// The heap handle of an `Obj` value (caller guarantees it is one).
    pub(super) fn as_obj(&self, v: Value) -> GcRef {
        v.as_obj().expect("as_obj on non-object")
    }
}
