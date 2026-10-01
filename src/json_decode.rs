//! Type-directed JSON decoding (M8): the `TypeDescriptor` that drives `json.decode[T](s)`, built
//! once from the target type `T` and then walked by the VM to coerce a parsed
//! `Json` value into a concrete struct / map / list / scalar.
//!
//! The descriptor is fully self-contained — a struct target embeds its field descriptors — so the
//! VM needs no type metadata at decode time. Recursive struct targets are therefore rejected
//! (they would make the descriptor infinite); decode them via the dynamic `Json` enum instead.

use crate::checker::Ty;

/// A resolved, self-contained description of a type `json.decode` can target. `F` is a struct
/// field's default: the checker's `ArgFill`, then the compiler's `DefaultThunk` (TICKET-198).
#[derive(Debug, Clone, PartialEq)]
pub enum TypeDescriptor<F> {
    Int,
    Float,
    Str,
    Bool,
    /// `list[T]`
    List(Box<TypeDescriptor<F>>),
    /// `map[str, V]` — JSON object with homogeneous values (keys are always strings).
    Map(Box<TypeDescriptor<F>>),
    /// `T?` — `Option[T]`; JSON `null` (or an absent object field with no default) becomes `None`.
    Option(Box<TypeDescriptor<F>>),
    /// `(A, B, …)` — a tuple; a JSON array of EXACTLY this arity (what `json.encode` emits for a
    /// tuple). A shorter or longer array is an `Err`, never padded or truncated.
    Tuple(Vec<TypeDescriptor<F>>),
    /// A concrete (non-generic) struct. ROOT REDESIGN — carries BOTH the IDENTITY KEY (the
    /// `<module-key>::Name` the runtime tags the produced `Value::Struct`/`Obj::Struct` with and looks
    /// the layout up by) AND the bare DISPLAY name (for `decode: expected object for <name>` errors).
    /// Fields are in declaration order.
    Struct {
        /// The program-global identity key (qualified) — the value tag + `struct_tid` lookup key.
        key: String,
        /// The bare user-facing name — used only in decode error messages.
        display: String,
        fields: Vec<FieldDesc<F>>,
    },
}

/// One struct field of a decode target: its name, its descriptor, and its default, if it has one.
/// A missing key takes `default` — the SAME fill `S(...)` uses for an omitted field (TICKET-198).
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc<F> {
    pub name: String,
    pub desc: TypeDescriptor<F>,
    pub default: Option<F>,
}

/// A compiled field default: a zero-arg proto the VM calls as a function homed in `module`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefaultThunk {
    pub proto: crate::vm::op::ProtoId,
    pub module: usize,
}

impl<F> TypeDescriptor<F> {
    /// The same descriptor with every field default mapped through `f`.
    pub fn try_map_defaults<G, E>(
        &self,
        f: &mut impl FnMut(&F) -> Result<G, E>,
    ) -> Result<TypeDescriptor<G>, E> {
        Ok(match self {
            TypeDescriptor::Int => TypeDescriptor::Int,
            TypeDescriptor::Float => TypeDescriptor::Float,
            TypeDescriptor::Str => TypeDescriptor::Str,
            TypeDescriptor::Bool => TypeDescriptor::Bool,
            TypeDescriptor::List(d) => TypeDescriptor::List(Box::new(d.try_map_defaults(f)?)),
            TypeDescriptor::Map(d) => TypeDescriptor::Map(Box::new(d.try_map_defaults(f)?)),
            TypeDescriptor::Option(d) => TypeDescriptor::Option(Box::new(d.try_map_defaults(f)?)),
            TypeDescriptor::Tuple(ds) => TypeDescriptor::Tuple(
                ds.iter()
                    .map(|d| d.try_map_defaults(f))
                    .collect::<Result<_, _>>()?,
            ),
            TypeDescriptor::Struct {
                key,
                display,
                fields,
            } => {
                let mut out = Vec::with_capacity(fields.len());
                for fd in fields {
                    let default = match &fd.default {
                        Some(d) => Some(f(d)?),
                        None => None,
                    };
                    out.push(FieldDesc {
                        name: fd.name.clone(),
                        desc: fd.desc.try_map_defaults(f)?,
                        default,
                    });
                }
                TypeDescriptor::Struct {
                    key: key.clone(),
                    display: display.clone(),
                    fields: out,
                }
            }
        })
    }
}

/// A struct identity key's declared `(field, type, default)` list, or `None` for a key with no
/// user layout.
pub type StructShape<'a, F> = dyn Fn(&str) -> Option<Vec<(String, Ty, Option<F>)>> + 'a;

/// Build the descriptor for a `json.decode[T]` target the checker has already resolved to `ty`.
/// The checker is the only caller: it reports the `Err` as the diagnostic and records the `Ok` for
/// the compiler (TICKET-180), so what is accepted and what is lowered is one decision. `shape`
/// gives a struct identity key's declared field types and defaults (`None` for a key with no user layout).
/// Accepted: scalars, `list`/`tuple`/`map[str,_]`/`Option` of decodables, and non-generic,
/// non-recursive structs of decodable fields; `visiting` holds the struct-expansion stack.
/// An `Unknown` anywhere gives an EMPTY `Err`: the checker already reported why, and adds nothing.
pub fn from_ty<F: Clone>(
    ty: &Ty,
    shape: &StructShape<'_, F>,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor<F>, String> {
    let sub = |t: &Ty, visiting: &mut Vec<String>| from_ty(t, shape, visiting).map(Box::new);
    match ty {
        Ty::Unknown => Err(String::new()),
        Ty::Int => Ok(TypeDescriptor::Int),
        Ty::Float => Ok(TypeDescriptor::Float),
        Ty::Str => Ok(TypeDescriptor::Str),
        Ty::Bool => Ok(TypeDescriptor::Bool),
        Ty::List(t) => Ok(TypeDescriptor::List(sub(t, visiting)?)),
        Ty::Option(t) => Ok(TypeDescriptor::Option(sub(t, visiting)?)),
        Ty::Tuple(ts) => ts
            .iter()
            .map(|t| from_ty(t, shape, visiting))
            .collect::<Result<Vec<_>, _>>()
            .map(TypeDescriptor::Tuple),
        Ty::Map(k, v) => {
            if !matches!(**k, Ty::Str) {
                return Err(format!("decode: map keys must be str, found {k}"));
            }
            Ok(TypeDescriptor::Map(sub(v, visiting)?))
        }
        Ty::Struct(name, args) => {
            if !args.is_empty() {
                return Err(format!("decode: cannot decode into generic struct {ty}"));
            }
            if visiting.iter().any(|s| s == name) {
                return Err(format!(
                    "decode: recursive struct '{name}' is not decodable; use the Json enum instead"
                ));
            }
            let Some(fields) = shape(name) else {
                return Err(format!("decode: '{name}' is not a decodable type"));
            };
            visiting.push(name.clone());
            let mut descs = Vec::with_capacity(fields.len());
            for (fname, fty, default) in &fields {
                descs.push(FieldDesc {
                    name: fname.clone(),
                    desc: from_ty(fty, shape, visiting)?,
                    default: default.clone(),
                });
            }
            visiting.pop();
            Ok(TypeDescriptor::Struct {
                key: name.clone(),
                display: crate::compiler::bare_display(name),
                fields: descs,
            })
        }
        other => Err(format!("decode: cannot decode into {other}")),
    }
}

/// The human-readable kind of a parsed `Json` value, named by its enum variant — used in decode
/// error messages ("found number"). Single source of truth so error wording stays consistent.
pub fn json_kind(variant: &str) -> &'static str {
    match variant {
        "Null" => "null",
        "Bool" => "bool",
        "Int" => "number",
        "Num" => "number",
        "Str" => "string",
        "Arr" => "array",
        "Obj" => "object",
        _ => "value",
    }
}
