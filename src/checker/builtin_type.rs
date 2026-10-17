//! U10 (TICKET-259): THE builtin type names. Each is a type, never a value, and each is reserved at
//! declaration. The annotation resolvers key their builtin arms on `BuiltinType`, never on a name string.

use super::{Builtin, Lowering, Ty};

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

    /// The number of type arguments the name takes, read from [`BuiltinType::applied_ty`]: the
    /// smallest count it accepts, `0` for a name that takes none.
    pub(crate) fn arity(self) -> usize {
        (1..=2)
            .find(|&n| self.applied_ty(vec![Ty::Unknown; n]).is_some())
            .unwrap_or(0)
    }
}

impl BuiltinType {
    /// The std module that owns this type, or `None` for a global builtin (in scope with no
    /// import). A module-owned type is an ordinary member of its module: a program names it through
    /// an import (`import Shared from std.concurrency`, `concurrency.Shared`), under any alias.
    pub(crate) fn owner(self) -> Option<&'static str> {
        match self {
            BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray
            | BuiltinType::List
            | BuiltinType::Set
            | BuiltinType::Map
            | BuiltinType::Channel
            | BuiltinType::Range => None,
            BuiltinType::Iterator => super::graph_pass::std_protocol_owner(self.name()),
            BuiltinType::Socket | BuiltinType::Listener => Some("std.net"),
            BuiltinType::Writer | BuiltinType::Reader => Some("std.io"),
            BuiltinType::Ptr | BuiltinType::OwnedStr => Some("std.ffi"),
            BuiltinType::Shared
            | BuiltinType::RwShared
            | BuiltinType::Atomic
            | BuiltinType::AtomicInt
            | BuiltinType::Executor => Some("std.concurrency"),
        }
    }

    /// Whether a call of the type's name constructs one through a dedicated opcode (`Channel[T]()`,
    /// `Shared(v)`, `Executor()`). The other builtin types have no constructor, or a prelude one.
    pub(crate) fn has_opcode_ctor(self) -> bool {
        Builtin::from_name(self.name()).is_some_and(|b| b.lowering() == Lowering::Opcode)
    }

    /// The global builtin type named `name` (one with no owning module).
    pub(crate) fn global(name: &str) -> Option<BuiltinType> {
        Self::from_name(name).filter(|b| b.owner().is_none())
    }
}

/// The std module that owns the builtin type named `name`: a module-owned [`BuiltinType`] or a
/// C width `std.ffi` declares. `None` for a global builtin and for every other name.
pub(crate) fn builtin_owner(name: &str) -> Option<&'static str> {
    match BuiltinType::from_name(name) {
        Some(b) => b.owner(),
        None => crate::native::ffi::is_declared_type(name).then_some("std.ffi"),
    }
}

/// Every module-owned builtin type name `module` owns (its own scope binds them).
pub(crate) fn module_builtins(module: &str) -> Vec<String> {
    let mut out: Vec<String> = BuiltinType::ALL
        .into_iter()
        .filter(|b| b.owner() == Some(module))
        .map(|b| b.name().to_string())
        .collect();
    if module == "std.ffi" {
        for d in crate::native::ffi::declared_types() {
            if !out.contains(&d.name) {
                out.push(d.name.clone());
            }
        }
    }
    out
}
