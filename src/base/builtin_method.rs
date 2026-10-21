//! base: the methods of each builtin type, one enum per type, generated from the std native decls.
//! `std/*.chz` owns the method set; `src/base/build_methods.rs` writes the enums. A VM dispatcher
//! matches its enum with every variant named, so a decl without an arm (or an arm without a decl)
//! fails the build.

include!(concat!(env!("OUT_DIR"), "/builtin_methods.rs"));

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
