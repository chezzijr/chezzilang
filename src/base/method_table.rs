//! base: the method table — what `recv.m(args)` runs, by (receiver row, method id). The compiler
//! interns each method name a call site spells into a [`MethodId`] and builds the table once per
//! program from the std decls (the generated `<Type>Method` enums, the bodied std methods), the
//! intrinsic protocol rows (`base::intrinsic`) and the program's own structs and enums. The VM reads
//! one dense entry per call; it decides nothing by name.

use crate::base::builtin_method::{
    AtomicIntMethod, AtomicMethod, BytearrayMethod, BytesMethod, ChannelMethod, DECLARED_METHODS,
    ErrorMethod, ExecutorMethod, FloatMethod, IntMethod, ListMethod, ListenerMethod, MapMethod,
    ReaderMethod, RwSharedMethod, RwSharedPrim, SetMethod, SharedMethod, SocketMethod, StrMethod,
    WriterMethod,
};
use crate::base::hooks::{HOOKS, Hook, Unhooked, protocol_method};
use crate::base::intrinsic::{INTRINSIC_PROTO_METHODS, RecvKind};
use crate::base::op::Program;
use std::collections::HashMap;

/// A method name, interned by the compiler: the operand of `Op::CallMethod`, `Op::CallHook` and
/// `Op::DeferMethod`, and the column of the [`MethodTable`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct MethodId(pub u32);

/// The method names of one program, one id each: every name std declares on a builtin type or
/// protocol (so each has a column whether or not the program calls it), then each name a call site
/// spells.
#[derive(Debug, Clone, Default)]
pub struct MethodNames {
    names: Vec<String>,
    ids: HashMap<String, MethodId>,
}

impl MethodNames {
    /// The std names: the methods of each builtin type's decl, the `native fn` defaults of the std
    /// protocols, and the one method of each protocol with a hook or an intrinsic row.
    pub fn with_std() -> Self {
        let mut n = Self::default();
        for (_, bodyless, bodied) in DECLARED_METHODS {
            for m in bodyless.iter().chain(bodied.iter()) {
                n.intern(m);
            }
        }
        for m in ErrorMethod::ALL {
            n.intern(m.name());
        }
        for row in HOOKS {
            n.intern(row.hook.method());
        }
        for u in Unhooked::ALL {
            n.intern(u.method());
        }
        n
    }

    pub fn intern(&mut self, name: &str) -> MethodId {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = MethodId(self.names.len() as u32);
        self.names.push(name.to_string());
        self.ids.insert(name.to_string(), id);
        id
    }

    pub fn get(&self, name: &str) -> Option<MethodId> {
        self.ids.get(name).copied()
    }

    pub fn name(&self, id: MethodId) -> &str {
        self.names.get(id.0 as usize).map_or("?", String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// The receiver rows of every kind but a user struct and a user enum, which get one row per type
/// after these ([`MethodTable::of_struct`], [`MethodTable::of_enum`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MethodRow {
    /// An inline int and a boxed one (`Obj::BigInt`).
    Int,
    Float,
    Bool,
    /// The absent value an expression with no value gives.
    Nil,
    Str,
    Bytes,
    ByteArray,
    List,
    Map,
    Set,
    Tuple,
    /// A fn value: a user fn or closure, a native or builtin fn, an `extern` fn.
    Func,
    Ptr,
    /// A capture cell (never a receiver: a read loads its value first).
    Cell,
    Channel,
    /// The `.iter()` cursor over a snapshot.
    Cursor,
    Generator,
    Module,
    Shared,
    RwShared,
    Atomic,
    AtomicInt,
    Executor,
    Socket,
    Listener,
    Writer,
    Reader,
}

impl MethodRow {
    pub const ALL: [MethodRow; 27] = [
        MethodRow::Int,
        MethodRow::Float,
        MethodRow::Bool,
        MethodRow::Nil,
        MethodRow::Str,
        MethodRow::Bytes,
        MethodRow::ByteArray,
        MethodRow::List,
        MethodRow::Map,
        MethodRow::Set,
        MethodRow::Tuple,
        MethodRow::Func,
        MethodRow::Ptr,
        MethodRow::Cell,
        MethodRow::Channel,
        MethodRow::Cursor,
        MethodRow::Generator,
        MethodRow::Module,
        MethodRow::Shared,
        MethodRow::RwShared,
        MethodRow::Atomic,
        MethodRow::AtomicInt,
        MethodRow::Executor,
        MethodRow::Socket,
        MethodRow::Listener,
        MethodRow::Writer,
        MethodRow::Reader,
    ];

    /// The builtin type this row is, as its std decl spells it (the `DECLARED_METHODS` and
    /// `Program::native_methods` key); `None` for a kind with no decl.
    pub fn type_name(self) -> Option<&'static str> {
        match self {
            MethodRow::Int => Some("int"),
            MethodRow::Float => Some("float"),
            MethodRow::Bool => Some("bool"),
            MethodRow::Str => Some("str"),
            MethodRow::Bytes => Some("bytes"),
            MethodRow::ByteArray => Some("bytearray"),
            MethodRow::List => Some("List"),
            MethodRow::Map => Some("Map"),
            MethodRow::Set => Some("Set"),
            MethodRow::Channel => Some("Channel"),
            MethodRow::Shared => Some("Shared"),
            MethodRow::RwShared => Some("RwShared"),
            MethodRow::Atomic => Some("Atomic"),
            MethodRow::AtomicInt => Some("AtomicInt"),
            MethodRow::Executor => Some("Executor"),
            MethodRow::Socket => Some("Socket"),
            MethodRow::Listener => Some("Listener"),
            MethodRow::Writer => Some("Writer"),
            MethodRow::Reader => Some("Reader"),
            MethodRow::Ptr => Some("ptr"),
            MethodRow::Nil
            | MethodRow::Tuple
            | MethodRow::Func
            | MethodRow::Cell
            | MethodRow::Cursor
            | MethodRow::Generator
            | MethodRow::Module => None,
        }
    }

    /// The row of the builtin type `name`.
    pub fn of_type(name: &str) -> Option<MethodRow> {
        Self::ALL.into_iter().find(|r| r.type_name() == Some(name))
    }

    /// The bodyless std methods of this row's decl.
    fn natives(self) -> Vec<NativeArm> {
        fn all<T: Copy>(xs: &[T], f: fn(T) -> NativeArm) -> Vec<NativeArm> {
            xs.iter().map(|&x| f(x)).collect()
        }
        match self {
            MethodRow::Int => all(IntMethod::ALL, NativeArm::Int),
            MethodRow::Float => all(FloatMethod::ALL, NativeArm::Float),
            MethodRow::Str => all(StrMethod::ALL, NativeArm::Str),
            MethodRow::Bytes => all(BytesMethod::ALL, NativeArm::Bytes),
            MethodRow::ByteArray => all(BytearrayMethod::ALL, NativeArm::ByteArray),
            MethodRow::List => all(ListMethod::ALL, NativeArm::List),
            MethodRow::Map => all(MapMethod::ALL, NativeArm::Map),
            MethodRow::Set => all(SetMethod::ALL, NativeArm::Set),
            MethodRow::Channel => all(ChannelMethod::ALL, NativeArm::Channel),
            MethodRow::Shared => all(SharedMethod::ALL, NativeArm::Shared),
            MethodRow::RwShared => {
                let mut v = all(RwSharedMethod::ALL, NativeArm::RwShared);
                v.extend(all(RwSharedPrim::ALL, NativeArm::RwSharedPrim));
                v
            }
            MethodRow::Atomic => all(AtomicMethod::ALL, NativeArm::Atomic),
            MethodRow::AtomicInt => all(AtomicIntMethod::ALL, NativeArm::AtomicInt),
            MethodRow::Executor => all(ExecutorMethod::ALL, NativeArm::Executor),
            MethodRow::Socket => all(SocketMethod::ALL, NativeArm::Socket),
            MethodRow::Listener => all(ListenerMethod::ALL, NativeArm::Listener),
            MethodRow::Writer => all(WriterMethod::ALL, NativeArm::Writer),
            MethodRow::Reader => all(ReaderMethod::ALL, NativeArm::Reader),
            MethodRow::Bool
            | MethodRow::Nil
            | MethodRow::Tuple
            | MethodRow::Func
            | MethodRow::Ptr
            | MethodRow::Cell
            | MethodRow::Cursor
            | MethodRow::Generator
            | MethodRow::Module => Vec::new(),
        }
    }

    /// The row's own intrinsic methods, which win over every other entry of the name: a
    /// collection's `iter` (a fresh cursor), a cursor's and a generator's `iter` (itself) and
    /// `next`.
    fn specials(self) -> &'static [(Hook, Intrinsic)] {
        match self {
            MethodRow::Str
            | MethodRow::Bytes
            | MethodRow::ByteArray
            | MethodRow::List
            | MethodRow::Map
            | MethodRow::Set
            | MethodRow::Channel => &[(Hook::Iter, Intrinsic::CollectionIter)],
            MethodRow::Cursor => &[
                (Hook::Iter, Intrinsic::SelfIter),
                (Hook::Next, Intrinsic::CursorNext),
            ],
            MethodRow::Generator => &[
                (Hook::Iter, Intrinsic::SelfIter),
                (Hook::Next, Intrinsic::GenNext),
            ],
            MethodRow::Int
            | MethodRow::Float
            | MethodRow::Bool
            | MethodRow::Nil
            | MethodRow::Tuple
            | MethodRow::Func
            | MethodRow::Ptr
            | MethodRow::Cell
            | MethodRow::Module
            | MethodRow::Shared
            | MethodRow::RwShared
            | MethodRow::Atomic
            | MethodRow::AtomicInt
            | MethodRow::Executor
            | MethodRow::Socket
            | MethodRow::Listener
            | MethodRow::Writer
            | MethodRow::Reader => &[],
        }
    }

    /// Does a value of this row answer the std protocols' `native fn` defaults (`Error.line`)?
    /// Every value does but a channel, a cursor, a generator, a handle and a module, whose
    /// dispatch is their own methods only.
    fn answers_defaults(self) -> bool {
        match self {
            MethodRow::Int
            | MethodRow::Float
            | MethodRow::Bool
            | MethodRow::Nil
            | MethodRow::Str
            | MethodRow::Bytes
            | MethodRow::ByteArray
            | MethodRow::List
            | MethodRow::Map
            | MethodRow::Set
            | MethodRow::Tuple
            | MethodRow::Func
            | MethodRow::Ptr
            | MethodRow::Cell => true,
            MethodRow::Channel
            | MethodRow::Cursor
            | MethodRow::Generator
            | MethodRow::Module
            | MethodRow::Shared
            | MethodRow::RwShared
            | MethodRow::Atomic
            | MethodRow::AtomicInt
            | MethodRow::Executor
            | MethodRow::Socket
            | MethodRow::Listener
            | MethodRow::Writer
            | MethodRow::Reader => false,
        }
    }
}

/// The table rows an intrinsic row of a [`RecvKind`] lands in.
pub enum KindRows {
    Fixed(&'static [MethodRow]),
    /// Every user struct's row.
    Structs,
    /// Every enum's row (`Option` and `Result` are enums at run time).
    Enums,
    /// A protocol value is its concrete witness at run time: the witness's own row answers.
    Witness,
}

impl RecvKind {
    pub fn method_rows(self) -> KindRows {
        match self {
            RecvKind::Int => KindRows::Fixed(&[MethodRow::Int]),
            RecvKind::Float => KindRows::Fixed(&[MethodRow::Float]),
            RecvKind::Bool => KindRows::Fixed(&[MethodRow::Bool]),
            RecvKind::Str => KindRows::Fixed(&[MethodRow::Str]),
            RecvKind::Bytes => KindRows::Fixed(&[MethodRow::Bytes]),
            RecvKind::ByteArray => KindRows::Fixed(&[MethodRow::ByteArray]),
            RecvKind::List => KindRows::Fixed(&[MethodRow::List]),
            RecvKind::Map => KindRows::Fixed(&[MethodRow::Map]),
            RecvKind::Set => KindRows::Fixed(&[MethodRow::Set]),
            RecvKind::Tuple => KindRows::Fixed(&[MethodRow::Tuple]),
            RecvKind::Func => KindRows::Fixed(&[MethodRow::Func]),
            RecvKind::Channel => KindRows::Fixed(&[MethodRow::Channel]),
            RecvKind::Cursor => KindRows::Fixed(&[MethodRow::Cursor, MethodRow::Generator]),
            RecvKind::Shared => KindRows::Fixed(&[MethodRow::Shared]),
            RecvKind::RwShared => KindRows::Fixed(&[MethodRow::RwShared]),
            RecvKind::Atomic => KindRows::Fixed(&[MethodRow::Atomic, MethodRow::AtomicInt]),
            RecvKind::Handle => KindRows::Fixed(&[
                MethodRow::Executor,
                MethodRow::Socket,
                MethodRow::Listener,
                MethodRow::Writer,
                MethodRow::Reader,
                MethodRow::Ptr,
            ]),
            RecvKind::Struct => KindRows::Structs,
            RecvKind::Enum | RecvKind::Option | RecvKind::Result => KindRows::Enums,
            RecvKind::Protocol => KindRows::Witness,
        }
    }
}

/// A bodyless std method: the variant of its type's generated enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NativeArm {
    Int(IntMethod),
    Float(FloatMethod),
    Str(StrMethod),
    Bytes(BytesMethod),
    ByteArray(BytearrayMethod),
    List(ListMethod),
    Map(MapMethod),
    Set(SetMethod),
    Channel(ChannelMethod),
    Shared(SharedMethod),
    RwShared(RwSharedMethod),
    RwSharedPrim(RwSharedPrim),
    Atomic(AtomicMethod),
    AtomicInt(AtomicIntMethod),
    Executor(ExecutorMethod),
    Socket(SocketMethod),
    Listener(ListenerMethod),
    Writer(WriterMethod),
    Reader(ReaderMethod),
}

impl NativeArm {
    pub fn name(self) -> &'static str {
        match self {
            NativeArm::Int(m) => m.name(),
            NativeArm::Float(m) => m.name(),
            NativeArm::Str(m) => m.name(),
            NativeArm::Bytes(m) => m.name(),
            NativeArm::ByteArray(m) => m.name(),
            NativeArm::List(m) => m.name(),
            NativeArm::Map(m) => m.name(),
            NativeArm::Set(m) => m.name(),
            NativeArm::Channel(m) => m.name(),
            NativeArm::Shared(m) => m.name(),
            NativeArm::RwShared(m) => m.name(),
            NativeArm::RwSharedPrim(m) => m.name(),
            NativeArm::Atomic(m) => m.name(),
            NativeArm::AtomicInt(m) => m.name(),
            NativeArm::Executor(m) => m.name(),
            NativeArm::Socket(m) => m.name(),
            NativeArm::Listener(m) => m.name(),
            NativeArm::Writer(m) => m.name(),
            NativeArm::Reader(m) => m.name(),
        }
    }
}

/// A method a kind has with no declaration behind it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Intrinsic {
    /// A hook protocol's method answered by its operator's own worker (`x.add(y)` is `x + y`).
    Hook(Hook),
    /// `PathLike.as_path` on a `str` / `bytes` / `bytearray`.
    AsPath,
    /// A std protocol's `native fn` default.
    Error(ErrorMethod),
    /// A collection's or a channel's `iter`: a fresh cursor.
    CollectionIter,
    /// A cursor's or a generator's `iter`: itself.
    SelfIter,
    /// A cursor's `next`.
    CursorNext,
    /// A generator's `next`: resume it.
    GenNext,
    /// `module.f(args)`: the module's member, called as a value.
    ModuleMember,
}

impl Intrinsic {
    /// The method `protocol`'s intrinsic row promises: its hook's, or an unhooked protocol's.
    /// `None` for `Error`, whose method on a `str` is the native `str.message`.
    pub fn of_protocol(protocol: &str) -> Option<Intrinsic> {
        if let Some(row) = HOOKS.iter().find(|r| r.protocol == protocol) {
            return Some(Intrinsic::Hook(row.hook));
        }
        let u = Unhooked::ALL
            .into_iter()
            .find(|u| u.protocol() == protocol)?;
        match u {
            Unhooked::Error => None,
            Unhooked::PathLike => Some(Intrinsic::AsPath),
        }
    }
}

/// What one (row, method) runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MethodEntry {
    /// The kind has no such method: the call faults with the kind's miss text.
    Miss,
    /// A compiled body — a user struct's or enum's method, or a bodied std method of a builtin
    /// type — and the index of its home module.
    Proto {
        proto: u32,
        home: u32,
    },
    /// A fn-typed field of a struct, at this index, called as a value.
    Field(u32),
    Native(NativeArm),
    Intrinsic(Intrinsic),
}

/// The dense (row, method id) table of one program. Rows are [`MethodRow::ALL`], then one per
/// struct `tid`, then one per enum; each row has one entry per [`MethodId`].
#[derive(Debug, Clone, Default)]
pub struct MethodTable {
    names: MethodNames,
    width: usize,
    entries: Vec<MethodEntry>,
    /// The row of each enum variant's enum, by `variant_id`.
    enum_rows: Vec<u32>,
}

impl MethodTable {
    /// Build the table of `program`, whose call sites interned `names`. Each row is filled from
    /// its weakest entry to its strongest, a later entry of a name replacing an earlier one: the
    /// std protocol defaults, the intrinsic rows, the native methods (or a struct's fn fields),
    /// the compiled bodies, the kind's own specials.
    pub fn build(program: &Program, names: MethodNames) -> MethodTable {
        let width = names.len();
        let fixed = MethodRow::ALL.len();
        let structs = program.struct_names.len();
        // One row per distinct enum, in first-variant order.
        let mut enum_keys: Vec<&str> = Vec::new();
        let mut enum_rows = Vec::with_capacity(program.variants_by_id.len());
        for v in &program.variants_by_id {
            let i = match enum_keys.iter().position(|k| *k == v.enum_name) {
                Some(i) => i,
                None => {
                    enum_keys.push(&v.enum_name);
                    enum_keys.len() - 1
                }
            };
            enum_rows.push((fixed + structs + i) as u32);
        }
        let rows = fixed + structs + enum_keys.len();
        let mut entries = vec![MethodEntry::Miss; rows * width];
        let mut set = |row: usize, name: &str, e: MethodEntry| {
            if let Some(id) = names.get(name) {
                entries[row * width + id.0 as usize] = e;
            }
        };
        let row_of = |r: MethodRow| r as usize;
        let defaults = |row: usize, set: &mut dyn FnMut(usize, &str, MethodEntry)| {
            for m in ErrorMethod::ALL {
                set(row, m.name(), MethodEntry::Intrinsic(Intrinsic::Error(*m)));
            }
        };
        // The std protocol defaults.
        for r in MethodRow::ALL {
            if r.answers_defaults() {
                defaults(row_of(r), &mut set);
            }
        }
        for row in fixed..rows {
            defaults(row, &mut set);
        }
        // The intrinsic rows.
        for (protocol, kind) in INTRINSIC_PROTO_METHODS {
            let (Some(method), Some(which)) =
                (protocol_method(protocol), Intrinsic::of_protocol(protocol))
            else {
                continue;
            };
            let e = MethodEntry::Intrinsic(which);
            match kind.method_rows() {
                KindRows::Fixed(rs) => {
                    for r in rs {
                        set(row_of(*r), method, e);
                    }
                }
                KindRows::Structs => {
                    for tid in 0..structs {
                        set(fixed + tid, method, e);
                    }
                }
                KindRows::Enums => {
                    for i in 0..enum_keys.len() {
                        set(fixed + structs + i, method, e);
                    }
                }
                KindRows::Witness => {}
            }
        }
        // The native methods, then the bodied std methods, then the row's specials.
        for r in MethodRow::ALL {
            for arm in r.natives() {
                set(row_of(r), arm.name(), MethodEntry::Native(arm));
            }
        }
        for (ty, ms) in &program.native_methods {
            let Some(r) = MethodRow::of_type(ty) else {
                continue;
            };
            let home = program.native_home.get(ty).copied().unwrap_or(0) as u32;
            for (m, &proto) in ms {
                let proto = proto as u32;
                set(row_of(r), m, MethodEntry::Proto { proto, home });
            }
        }
        for r in MethodRow::ALL {
            for (hook, which) in r.specials() {
                set(row_of(r), hook.method(), MethodEntry::Intrinsic(*which));
            }
        }
        // A module's every column is its member of the name.
        let module = row_of(MethodRow::Module);
        for id in 0..width {
            entries[module * width + id] = MethodEntry::Intrinsic(Intrinsic::ModuleMember);
        }
        // Each struct: its fn fields, then its methods.
        for (tid, key) in program.struct_names.iter().enumerate() {
            let Some(def) = program.structs.get(&**key) else {
                continue;
            };
            let row = fixed + tid;
            for (i, f) in def.fields.iter().enumerate() {
                if let Some(id) = names.get(f) {
                    entries[row * width + id.0 as usize] = MethodEntry::Field(i as u32);
                }
            }
            let home = def.module_idx as u32;
            for (m, &proto) in &def.methods {
                if let Some(id) = names.get(m) {
                    let proto = proto as u32;
                    entries[row * width + id.0 as usize] = MethodEntry::Proto { proto, home };
                }
            }
        }
        // Each enum: its methods.
        for (i, key) in enum_keys.iter().enumerate() {
            let Some(ms) = program.enum_methods.get(*key) else {
                continue;
            };
            let row = fixed + structs + i;
            let home = program.enum_home.get(*key).copied().unwrap_or(0) as u32;
            for (m, &proto) in ms {
                if let Some(id) = names.get(m) {
                    let proto = proto as u32;
                    entries[row * width + id.0 as usize] = MethodEntry::Proto { proto, home };
                }
            }
        }
        MethodTable {
            names,
            width,
            entries,
            enum_rows,
        }
    }

    pub fn names(&self) -> &MethodNames {
        &self.names
    }

    pub fn name(&self, id: MethodId) -> &str {
        self.names.name(id)
    }

    fn at(&self, row: usize, id: MethodId) -> MethodEntry {
        self.entries
            .get(row * self.width + id.0 as usize)
            .copied()
            .unwrap_or(MethodEntry::Miss)
    }

    pub fn fixed(&self, row: MethodRow, id: MethodId) -> MethodEntry {
        self.at(row as usize, id)
    }

    pub fn of_struct(&self, tid: u32, id: MethodId) -> MethodEntry {
        self.at(MethodRow::ALL.len() + tid as usize, id)
    }

    pub fn of_enum(&self, variant_id: u32, id: MethodId) -> MethodEntry {
        match self.enum_rows.get(variant_id as usize) {
            Some(&row) => self.at(row as usize, id),
            None => MethodEntry::Miss,
        }
    }
}
