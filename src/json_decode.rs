//! Type-directed JSON decoding (M8): the `TypeDescriptor` that drives `json.decode[T](s)`, built
//! once from the target type `T` and then walked by the VM to coerce a parsed
//! `Json` value into a concrete struct / map / list / scalar.
//!
//! The descriptor is fully self-contained — a struct target embeds its field descriptors — so the
//! VM needs no type metadata at decode time. Recursive struct targets are therefore rejected
//! (they would make the descriptor infinite); decode them via the dynamic `Json` enum instead.

use crate::ast::{Field, Type};

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

/// ROOT REDESIGN — module-aware resolution context for building a decode descriptor. The VM
/// implements it; it maps a syntactic struct reference (a bare `Named` or a qualified `module.Name`,
/// resolved *in a given module*) to its program-global IDENTITY KEY + declared fields + declaring
/// module — so nested field struct types expand in their OWN defining module's scope, never the call
/// site's. This is why one canonical key kills the whole "decode against the wrong layout" bug class.
pub trait DecodeEnv {
    /// Resolve a bare type `name` written in module `module_idx` to its identity key, or `None` if it
    /// is not a (visible) user struct there. Reserved scalars are handled before this is consulted.
    fn resolve_bare(&self, module_idx: usize, name: &str) -> Option<String>;
    /// Resolve a module-qualified `binder.name` written in module `module_idx` to its identity key.
    fn resolve_qualified(&self, module_idx: usize, binder: &str, name: &str) -> Option<String>;
    /// The declared fields + declaring-module index for a struct identity `key`, or `None`.
    fn struct_def(&self, key: &str) -> Option<(usize, &[Field])>;
    /// The bare display name for a struct identity `key` (`Point` for `dep::Point`).
    fn display_of(&self, key: &str) -> String;
}

/// Build a descriptor for a `json.decode[T]` target type `ty` written in module `call_module`,
/// resolving every struct reference (and the field types it transitively names) through `env`.
/// Returns a human-readable error if the type is not decodable (functions, tuples, generic/recursive
/// structs, `Result`, unknown names). `visiting` tracks the (identity-key) struct-expansion stack to
/// reject recursive targets — two modules' same-named structs are correctly distinct keys.
pub fn from_type(
    ty: &Type,
    call_module: usize,
    env: &dyn DecodeEnv,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor, String> {
    match ty {
        Type::Named { name: n, .. } => match n.as_str() {
            "int" => Ok(TypeDescriptor::Int),
            "float" => Ok(TypeDescriptor::Float),
            "str" => Ok(TypeDescriptor::Str),
            "bool" => Ok(TypeDescriptor::Bool),
            _ => {
                let key = env
                    .resolve_bare(call_module, n)
                    .ok_or_else(|| format!("decode: '{n}' is not a decodable type"))?;
                struct_descriptor(&key, env, visiting)
            }
        },
        Type::Generic(n, args, ..) => match (n.as_str(), args.as_slice()) {
            ("List", [t]) => Ok(TypeDescriptor::List(Box::new(from_type(
                t,
                call_module,
                env,
                visiting,
            )?))),
            ("Map", [k, v]) => {
                if !matches!(k, Type::Named { name: s, .. } if s == "str") {
                    return Err("decode: Map keys must be str, found a non-str key".to_string());
                }
                Ok(TypeDescriptor::Map(Box::new(from_type(
                    v,
                    call_module,
                    env,
                    visiting,
                )?)))
            }
            ("Option", [t]) => Ok(TypeDescriptor::Option(Box::new(from_type(
                t,
                call_module,
                env,
                visiting,
            )?))),
            (other, _) => Err(format!("decode: cannot decode into generic type '{other}'")),
        },
        Type::Func { .. } => Err("decode: cannot decode into a function type".to_string()),
        Type::Tuple(_) => Err("decode: cannot decode into a tuple type".to_string()),
        // A module-qualified struct target (`json.decode[geo.Point]`): resolve `module.name` to its
        // identity key in the call-site module. Generic qualified targets are not decodable.
        Type::Qualified { module, name, args } => {
            if args.is_empty() {
                let key = env
                    .resolve_qualified(call_module, module, name)
                    .ok_or_else(|| format!("decode: '{name}' is not a decodable type"))?;
                struct_descriptor(&key, env, visiting)
            } else {
                Err(format!("decode: cannot decode into generic type '{name}'"))
            }
        }
    }
}

fn struct_descriptor(
    key: &str,
    env: &dyn DecodeEnv,
    visiting: &mut Vec<String>,
) -> Result<TypeDescriptor, String> {
    let (decl_module, fields) = env
        .struct_def(key)
        .map(|(m, f)| (m, f.to_vec()))
        .ok_or_else(|| format!("decode: '{}' is not a decodable type", env.display_of(key)))?;
    if visiting.iter().any(|s| s == key) {
        return Err(format!(
            "decode: recursive struct '{}' is not decodable; use the Json enum instead",
            env.display_of(key)
        ));
    }
    visiting.push(key.to_string());
    let mut field_descs = Vec::with_capacity(fields.len());
    // ROOT REDESIGN — each field's named types expand in THIS struct's DECLARING module, not the call
    // site's, so a nested `Inner` resolves to the defining module's `Inner` (the nested-collision fix).
    for f in &fields {
        field_descs.push((
            f.name.clone(),
            from_type(&f.ty, decl_module, env, visiting)?,
        ));
    }
    visiting.pop();
    Ok(TypeDescriptor::Struct {
        key: key.to_string(),
        display: env.display_of(key),
        fields: field_descs,
    })
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
