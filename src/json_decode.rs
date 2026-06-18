//! Type-directed JSON decoding (M8): the `TypeDescriptor` that drives `json.decode[T](s)`, built
//! once from the target type `T` and then walked identically by both engines to coerce a parsed
//! `Json` value into a concrete struct / map / list / scalar.
//!
//! The descriptor is fully self-contained — a struct target embeds its field descriptors — so an
//! engine needs no type metadata at decode time. Recursive struct targets are therefore rejected
//! (they would make the descriptor infinite); decode them via the dynamic `Json` enum instead.

use crate::ast::{Field, Type};
use std::collections::HashMap;

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
    /// A concrete (non-generic) struct: its name plus each field's name and descriptor, in
    /// declaration order.
    Struct {
        name: String,
        fields: Vec<(String, TypeDescriptor)>,
    },
}

/// Build a descriptor from a syntactic target type, resolving struct field types from `structs`
/// (struct name → declared fields). Returns a human-readable error if the type is not decodable
/// (functions, tuples, generic/recursive structs, `Result`, unknown names). `visiting` tracks the
/// struct-expansion stack to reject recursive targets.
pub fn from_type(
    ty: &Type,
    structs: &HashMap<String, Vec<Field>>,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor, String> {
    match ty {
        Type::Named(n) => match n.as_str() {
            "int" => Ok(TypeDescriptor::Int),
            "float" => Ok(TypeDescriptor::Float),
            "str" => Ok(TypeDescriptor::Str),
            "bool" => Ok(TypeDescriptor::Bool),
            _ => struct_descriptor(n, structs, visiting),
        },
        Type::Generic(n, args) => match (n.as_str(), args.as_slice()) {
            ("list", [t]) => Ok(TypeDescriptor::List(Box::new(from_type(
                t, structs, visiting,
            )?))),
            ("map", [k, v]) => {
                if !matches!(k, Type::Named(s) if s == "str") {
                    return Err("decode: map keys must be str, found a non-str key".to_string());
                }
                Ok(TypeDescriptor::Map(Box::new(from_type(
                    v, structs, visiting,
                )?)))
            }
            ("Option", [t]) => Ok(TypeDescriptor::Option(Box::new(from_type(
                t, structs, visiting,
            )?))),
            (other, _) => Err(format!("decode: cannot decode into generic type '{other}'")),
        },
        Type::Func { .. } => Err("decode: cannot decode into a function type".to_string()),
        Type::Tuple(_) => Err("decode: cannot decode into a tuple type".to_string()),
    }
}

fn struct_descriptor(
    name: &str,
    structs: &HashMap<String, Vec<Field>>,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor, String> {
    let fields = structs
        .get(name)
        .ok_or_else(|| format!("decode: '{name}' is not a decodable type"))?;
    if visiting.iter().any(|s| s == name) {
        return Err(format!(
            "decode: recursive struct '{name}' is not decodable; use the Json enum instead"
        ));
    }
    visiting.push(name.to_string());
    let mut field_descs = Vec::with_capacity(fields.len());
    for f in fields {
        field_descs.push((f.name.clone(), from_type(&f.ty, structs, visiting)?));
    }
    visiting.pop();
    Ok(TypeDescriptor::Struct {
        name: name.to_string(),
        fields: field_descs,
    })
}

/// The human-readable kind of a parsed `Json` value, named by its enum variant — used in decode
/// error messages ("found number"). Shared so both engines word errors identically.
pub fn json_kind(variant: &str) -> &'static str {
    match variant {
        "Null" => "null",
        "Bool" => "bool",
        "Num" => "number",
        "Str" => "string",
        "Arr" => "array",
        "Obj" => "object",
        _ => "value",
    }
}
