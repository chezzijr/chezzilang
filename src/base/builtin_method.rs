//! base: the methods of each builtin type, one enum per type, generated from the std native decls.
//! `std/*.chz` owns the method set; `src/base/build_methods.rs` writes the enums. A VM dispatcher
//! matches its enum with every variant named, so a decl without an arm (or an arm without a decl)
//! fails the build.

include!(concat!(env!("OUT_DIR"), "/builtin_methods.rs"));

/// A literal default of a `native fn` parameter, as its decl in `std/*.chz` spells it. The build
/// script reads every native default into [`NATIVE_DEFAULTS`]; a default that is no literal of its
/// parameter's type fails the build.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum DeclDefault {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(&'static str),
    None,
}

/// The trailing defaulted parameters of a native: a call passes at least `min` arguments, and
/// `values[i]` is the default of parameter `min + i`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct DeclDefaults {
    pub min: usize,
    pub values: &'static [DeclDefault],
}

impl DeclDefaults {
    /// A native with no default.
    pub const NONE: Self = Self {
        min: 0,
        values: &[],
    };
}

/// The defaults of the native `name` of `owner` (a native std module, `"std.io"`, for a
/// module-level fn; the type for a method or static), or none. A direct call has every argument
/// (the checker's binder fills the defaults at the call site); a call through a fn value is filled
/// from this table by the callee.
pub fn native_fn_defaults(owner: &str, name: &str) -> &'static DeclDefaults {
    NATIVE_DEFAULTS
        .iter()
        .find(|(m, n, _, _)| *m == owner && *n == name)
        .map_or(&DeclDefaults::NONE, |(_, _, _, d)| d)
}

impl NativeStatic {
    /// The trailing defaults its decl declares (its `NATIVE_DEFAULTS` row): a call through the
    /// static's value is filled from them.
    pub fn defaults(self) -> &'static DeclDefaults {
        let (ty, method) = self.key();
        native_fn_defaults(ty, method)
    }
}

/// The defaults of the native decl `name` whose parameters (without `self`) are `params`, for the
/// checker's binder. The build makes that pair name one decl.
pub fn native_decl_defaults(name: &str, params: &[&str]) -> &'static DeclDefaults {
    NATIVE_DEFAULTS
        .iter()
        .find(|(_, n, ps, _)| *n == name && *ps == params)
        .map_or(&DeclDefaults::NONE, |(_, _, _, d)| d)
}

/// The `_` container primitives of an `RwShared[T]` box (`std.concurrency.collection` calls them).
/// DEC-243: the box has exactly four decls, so these have no `native fn` line; this enum owns them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RwSharedPrim {
    Len,
    At,
    Slice,
    Push,
    SetKey,
    RemoveKey,
    GetOrInsert,
    Add,
    Remove,
    Contains,
    Has,
    GetKey,
}

impl RwSharedPrim {
    pub const ALL: &'static [Self] = &[
        Self::Len,
        Self::At,
        Self::Slice,
        Self::Push,
        Self::SetKey,
        Self::RemoveKey,
        Self::GetOrInsert,
        Self::Add,
        Self::Remove,
        Self::Contains,
        Self::Has,
        Self::GetKey,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Len => "_len",
            Self::At => "_at",
            Self::Slice => "_slice",
            Self::Push => "_push",
            Self::SetKey => "_set_key",
            Self::RemoveKey => "_remove_key",
            Self::GetOrInsert => "_get_or_insert",
            Self::Add => "_add",
            Self::Remove => "_remove",
            Self::Contains => "_contains",
            Self::Has => "_has",
            Self::GetKey => "_get_key",
        }
    }
}
