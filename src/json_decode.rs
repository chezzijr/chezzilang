//! Type-directed JSON decoding (M8): the `TypeDescriptor` that drives `json.decode[T](s)`, built
//! once from the target type `T` and then walked by the VM to coerce a parsed
//! `Json` value into a concrete struct / map / list / scalar.
//!
//! The descriptor is fully self-contained — a struct target embeds its field descriptors — so the
//! VM needs no type metadata at decode time. Recursive struct targets are therefore rejected
//! (they would make the descriptor infinite); decode them via the dynamic `Json` enum instead.

use crate::checker::Ty;

/// A resolved, self-contained description of a type `json.decode` can target.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeDescriptor {
    Int,
    Float,
    Str,
    Bool,
    /// `list[T]`
    List(Box<TypeDescriptor>),
    /// `map[str, V]` — JSON object with homogeneous values (keys are always strings).
    Map(Box<TypeDescriptor>),
    /// `T?` — `Option[T]`; JSON `null` (or an absent object field) becomes `None`.
    Option(Box<TypeDescriptor>),
    /// `(A, B, …)` — a tuple; a JSON array of EXACTLY this arity (what `json.encode` emits for a
    /// tuple). A shorter or longer array is an `Err`, never padded or truncated.
    Tuple(Vec<TypeDescriptor>),
    /// A concrete (non-generic) struct. ROOT REDESIGN — carries BOTH the IDENTITY KEY (the
    /// `<module-key>::Name` the runtime tags the produced `Value::Struct`/`Obj::Struct` with and looks
    /// the layout up by) AND the bare DISPLAY name (for `decode: expected object for <name>` errors).
    /// Fields are each field's name + descriptor, in declaration order.
    Struct {
        /// The program-global identity key (qualified) — the value tag + `struct_tid` lookup key.
        key: String,
        /// The bare user-facing name — used only in decode error messages.
        display: String,
        fields: Vec<(String, TypeDescriptor)>,
    },
}

/// A struct identity key's declared `(field, type)` list, or `None` for a key with no user layout.
pub type StructShape<'a> = dyn Fn(&str) -> Option<Vec<(String, Ty)>> + 'a;

/// Build the descriptor for a `json.decode[T]` target the checker has already resolved to `ty`.
/// The checker is the only caller: it reports the `Err` as the diagnostic and records the `Ok` for
/// the compiler (TICKET-180), so what is accepted and what is lowered is one decision. `shape`
/// gives a struct identity key's declared field types (`None` for a key with no user layout).
/// Accepted: scalars, `list`/`tuple`/`map[str,_]`/`Option` of decodables, and non-generic,
/// non-recursive structs of decodable fields; `visiting` holds the struct-expansion stack.
/// An `Unknown` anywhere gives an EMPTY `Err`: the checker already reported why, and adds nothing.
pub fn from_ty(
    ty: &Ty,
    shape: &StructShape<'_>,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor, String> {
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
            for (fname, fty) in &fields {
                descs.push((fname.clone(), from_ty(fty, shape, visiting)?));
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
