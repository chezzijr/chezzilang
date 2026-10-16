//! base: the builtins a program calls by a bare name, one id each. The checker resolves a name to
//! a [`Builtin`] once; the compiler lowers it and the VM dispatches on it, and neither compares the
//! name again.

/// A builtin fn or constructor the checker resolves a call head or a value to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Builtin {
    Print,
    Ord,
    Chr,
    Panic,
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    ByteArray,
    List,
    Map,
    Set,
    Channel,
    Shared,
    RwShared,
    Atomic,
    AtomicInt,
    Executor,
}

/// How a direct call of a builtin lowers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lowering {
    /// `Op::CallPrint` / `Op::CallPrintSep` (variadic, with `sep=` / `end=`).
    Print,
    /// `Op::CallBuiltin`, run by the VM's `do_builtin`.
    Call,
    /// An allocation opcode of its own (`Op::NewChannel`, `Op::NewShared`, …).
    Opcode,
}

impl Builtin {
    pub const ALL: [Builtin; 19] = [
        Builtin::Print,
        Builtin::Ord,
        Builtin::Chr,
        Builtin::Panic,
        Builtin::Int,
        Builtin::Float,
        Builtin::Bool,
        Builtin::Str,
        Builtin::Bytes,
        Builtin::ByteArray,
        Builtin::List,
        Builtin::Map,
        Builtin::Set,
        Builtin::Channel,
        Builtin::Shared,
        Builtin::RwShared,
        Builtin::Atomic,
        Builtin::AtomicInt,
        Builtin::Executor,
    ];

    /// The name a program writes.
    pub const fn name(self) -> &'static str {
        match self {
            Builtin::Print => "print",
            Builtin::Ord => "ord",
            Builtin::Chr => "chr",
            Builtin::Panic => "panic",
            Builtin::Int => "int",
            Builtin::Float => "float",
            Builtin::Bool => "bool",
            Builtin::Str => "str",
            Builtin::Bytes => "bytes",
            Builtin::ByteArray => "bytearray",
            Builtin::List => "List",
            Builtin::Map => "Map",
            Builtin::Set => "Set",
            Builtin::Channel => "Channel",
            Builtin::Shared => "Shared",
            Builtin::RwShared => "RwShared",
            Builtin::Atomic => "Atomic",
            Builtin::AtomicInt => "AtomicInt",
            Builtin::Executor => "Executor",
        }
    }

    pub fn from_name(name: &str) -> Option<Builtin> {
        Builtin::ALL.into_iter().find(|b| b.name() == name)
    }

    pub const fn lowering(self) -> Lowering {
        match self {
            Builtin::Print => Lowering::Print,
            Builtin::Ord
            | Builtin::Chr
            | Builtin::Panic
            | Builtin::Int
            | Builtin::Float
            | Builtin::Bool
            | Builtin::Str
            | Builtin::Bytes
            | Builtin::ByteArray
            | Builtin::List
            | Builtin::Map
            | Builtin::Set => Lowering::Call,
            Builtin::Channel
            | Builtin::Shared
            | Builtin::RwShared
            | Builtin::Atomic
            | Builtin::AtomicInt
            | Builtin::Executor => Lowering::Opcode,
        }
    }

    /// A value of its own (`p := print`, `defer panic("x")`). A constructor is not one: a type is
    /// not a value, as for a user struct.
    pub const fn first_class(self) -> bool {
        match self {
            Builtin::Print | Builtin::Ord | Builtin::Chr | Builtin::Panic => true,
            Builtin::Int
            | Builtin::Float
            | Builtin::Bool
            | Builtin::Str
            | Builtin::Bytes
            | Builtin::ByteArray
            | Builtin::List
            | Builtin::Map
            | Builtin::Set
            | Builtin::Channel
            | Builtin::Shared
            | Builtin::RwShared
            | Builtin::Atomic
            | Builtin::AtomicInt
            | Builtin::Executor => false,
        }
    }
}
