//! base: the json.decode target descriptor the checker builds and the VM walks.

/// A resolved, self-contained description of a type `json.decode` can target. `F` is a struct
/// field's default: the checker's `ArgFill`, then the compiler's `DefaultThunk` (TICKET-198).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldDesc<F> {
    pub name: String,
    pub desc: TypeDescriptor<F>,
    pub default: Option<F>,
}

/// A compiled field default: a zero-arg proto the VM calls as a function homed in `module`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DefaultThunk {
    pub proto: crate::base::op::ProtoId,
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

    /// The descriptor with every default's payload dropped (which fields HAVE a default stays):
    /// the target type alone, hashable. TICKET-226 keys the `json.decode[T]` value memo on it.
    pub fn shape(&self) -> TypeDescriptor<()> {
        self.try_map_defaults(&mut |_| Ok::<(), std::convert::Infallible>(()))
            .unwrap_or_else(|e| match e {})
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
