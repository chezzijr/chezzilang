//! Type-directed JSON decoding (M8): the `TypeDescriptor` that drives `json.decode[T](s)`, built
//! once from the target type `T` and then walked by the VM to coerce a parsed
//! `Json` value into a concrete struct / map / list / scalar.
//!
//! The descriptor is fully self-contained — a struct target embeds its field descriptors — so the
//! VM needs no type metadata at decode time. Recursive struct targets are therefore rejected
//! (they would make the descriptor infinite); decode them via the dynamic `Json` enum instead.

use crate::base::json_desc::{FieldDesc, TypeDescriptor};
use crate::checker::Ty;

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
    let ty = ty.scalar();
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
                    "decode: recursive struct '{}' is not decodable; use the Json enum instead",
                    crate::base::words::bare_display(name)
                ));
            }
            let Some(fields) = shape(name) else {
                return Err(format!(
                    "decode: '{}' is not a decodable type",
                    crate::base::words::bare_display(name)
                ));
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
                display: crate::base::words::bare_display(name),
                fields: descs,
            })
        }
        other @ Ty::Bytes
        | other @ Ty::ByteArray
        | other @ Ty::Nil
        | other @ Ty::Set(_)
        | other @ Ty::Func { .. }
        | other @ Ty::BuiltinFn { .. }
        | other @ Ty::Enum(..)
        | other @ Ty::Param(_)
        | other @ Ty::Result(..)
        | other @ Ty::Channel(_)
        | other @ Ty::Shared(_)
        | other @ Ty::Atomic(_)
        | other @ Ty::AtomicInt
        | other @ Ty::RwShared(_)
        | other @ Ty::Executor
        | other @ Ty::Socket
        | other @ Ty::Listener
        | other @ Ty::Writer
        | other @ Ty::Reader
        | other @ Ty::Ptr
        | other @ Ty::Width(_)
        | other @ Ty::Protocol(..)
        | other @ Ty::Module(_)
        | other @ Ty::Var(_) => Err(format!("decode: cannot decode into {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(key: &str, default: Option<u8>) -> TypeDescriptor<u8> {
        TypeDescriptor::Struct {
            key: key.to_string(),
            display: "S".to_string(),
            fields: vec![FieldDesc {
                name: "a".to_string(),
                desc: TypeDescriptor::Int,
                default,
            }],
        }
    }

    /// TICKET-226: the decode value memo keys on `shape()`, which drops each default's payload
    /// but keeps the target type and which fields have a default.
    #[test]
    fn shape_drops_default_payloads_only() {
        assert_eq!(s("m::S", Some(1)).shape(), s("m::S", Some(2)).shape());
        assert_ne!(s("m::S", Some(1)).shape(), s("n::S", Some(1)).shape());
        assert_ne!(s("m::S", Some(1)).shape(), s("m::S", None).shape());
        assert_ne!(
            TypeDescriptor::<u8>::List(Box::new(TypeDescriptor::Int)).shape(),
            TypeDescriptor::<u8>::List(Box::new(TypeDescriptor::Float)).shape()
        );
    }
}
