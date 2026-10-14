//! U10 (TICKET-259): THE builtin type names. Each is a type, never a value, and each is reserved at
//! declaration. The annotation resolvers key their builtin arms on `BuiltinType`, never on a name string.

use super::Ty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinType {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    ByteArray,
    Iterator,
    List,
    Set,
    Map,
    Channel,
    Range,
    Socket,
    Listener,
    Writer,
    Reader,
    Ptr,
    OwnedStr,
    Shared,
    RwShared,
    Atomic,
    AtomicInt,
    Executor,
}

impl BuiltinType {
    pub(crate) const ALL: [BuiltinType; 23] = [
        BuiltinType::Int,
        BuiltinType::Float,
        BuiltinType::Bool,
        BuiltinType::Str,
        BuiltinType::Bytes,
        BuiltinType::ByteArray,
        BuiltinType::Iterator,
        BuiltinType::List,
        BuiltinType::Set,
        BuiltinType::Map,
        BuiltinType::Channel,
        BuiltinType::Range,
        BuiltinType::Socket,
        BuiltinType::Listener,
        BuiltinType::Writer,
        BuiltinType::Reader,
        BuiltinType::Ptr,
        BuiltinType::OwnedStr,
        BuiltinType::Shared,
        BuiltinType::RwShared,
        BuiltinType::Atomic,
        BuiltinType::AtomicInt,
        BuiltinType::Executor,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            BuiltinType::Int => "int",
            BuiltinType::Float => "float",
            BuiltinType::Bool => "bool",
            BuiltinType::Str => "str",
            BuiltinType::Bytes => "bytes",
            BuiltinType::ByteArray => "bytearray",
            BuiltinType::Iterator => "Iterator",
            BuiltinType::List => "List",
            BuiltinType::Set => "Set",
            BuiltinType::Map => "Map",
            BuiltinType::Channel => "Channel",
            BuiltinType::Range => "Range",
            BuiltinType::Socket => "Socket",
            BuiltinType::Listener => "Listener",
            BuiltinType::Writer => "Writer",
            BuiltinType::Reader => "Reader",
            BuiltinType::Ptr => "ptr",
            BuiltinType::OwnedStr => "owned_str",
            BuiltinType::Shared => "Shared",
            BuiltinType::RwShared => "RwShared",
            BuiltinType::Atomic => "Atomic",
            BuiltinType::AtomicInt => "AtomicInt",
            BuiltinType::Executor => "Executor",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<BuiltinType> {
        Self::ALL.into_iter().find(|b| b.name() == name)
    }

    /// The scalar type a name spells, or `None` for every other builtin type.
    pub(crate) fn scalar_ty(self) -> Option<Ty> {
        match self {
            BuiltinType::Int => Some(Ty::Int),
            BuiltinType::Float => Some(Ty::Float),
            BuiltinType::Bool => Some(Ty::Bool),
            BuiltinType::Str => Some(Ty::Str),
            BuiltinType::Bytes => Some(Ty::Bytes),
            BuiltinType::ByteArray => Some(Ty::ByteArray),
            BuiltinType::Iterator
            | BuiltinType::List
            | BuiltinType::Set
            | BuiltinType::Map
            | BuiltinType::Channel
            | BuiltinType::Range
            | BuiltinType::Socket
            | BuiltinType::Listener
            | BuiltinType::Writer
            | BuiltinType::Reader
            | BuiltinType::Ptr
            | BuiltinType::OwnedStr
            | BuiltinType::Shared
            | BuiltinType::RwShared
            | BuiltinType::Atomic
            | BuiltinType::AtomicInt
            | BuiltinType::Executor => None,
        }
    }

    /// The type a bare (no type argument) name spells, before any import gate.
    pub(crate) fn plain_ty(self) -> Option<Ty> {
        match self {
            BuiltinType::Socket => Some(Ty::Socket),
            BuiltinType::Listener => Some(Ty::Listener),
            BuiltinType::Writer => Some(Ty::Writer),
            BuiltinType::Reader => Some(Ty::Reader),
            BuiltinType::Ptr => Some(Ty::Ptr),
            BuiltinType::AtomicInt => Some(Ty::AtomicInt),
            BuiltinType::Executor => Some(Ty::Executor),
            BuiltinType::OwnedStr => Some(Ty::Str),
            BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray => self.scalar_ty(),
            BuiltinType::Iterator
            | BuiltinType::List
            | BuiltinType::Set
            | BuiltinType::Map
            | BuiltinType::Channel
            | BuiltinType::Range
            | BuiltinType::Shared
            | BuiltinType::RwShared
            | BuiltinType::Atomic => None,
        }
    }

    /// The type a name applied to already-resolved type arguments spells, or `None` when the name
    /// takes no such arguments or the count is wrong.
    pub(crate) fn applied_ty(self, args: Vec<Ty>) -> Option<Ty> {
        let one = |a: Vec<Ty>| <[Ty; 1]>::try_from(a).ok().map(|[t]| t);
        match self {
            BuiltinType::List => Some(Ty::list(one(args)?)),
            BuiltinType::Set => Some(Ty::set(one(args)?)),
            BuiltinType::Channel => Some(Ty::channel(one(args)?)),
            BuiltinType::Shared => Some(Ty::shared(one(args)?)),
            BuiltinType::RwShared => Some(Ty::rwshared(one(args)?)),
            BuiltinType::Atomic => Some(Ty::atomic(one(args)?)),
            BuiltinType::Iterator => Some(Ty::Struct(self.name().to_string(), vec![one(args)?])),
            BuiltinType::Map => {
                let [k, v] = <[Ty; 2]>::try_from(args).ok()?;
                Some(Ty::map(k, v))
            }
            BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray
            | BuiltinType::Range
            | BuiltinType::Socket
            | BuiltinType::Listener
            | BuiltinType::Writer
            | BuiltinType::Reader
            | BuiltinType::Ptr
            | BuiltinType::OwnedStr
            | BuiltinType::AtomicInt
            | BuiltinType::Executor => None,
        }
    }
}

/// U10: a `BuiltinType` or a type `std.ffi` declares (`ptr`, the widths).
pub(crate) fn is_builtin_type_name(name: &str) -> bool {
    BuiltinType::from_name(name).is_some() || crate::native::ffi::is_declared_type(name)
}
