//! The checker's internal type lattice (`Ty`) — distinct from the AST's `Type` annotation node.
//!
//! Pragmatic, no unification: `list`/`Result`/`Option` carry exactly one inner type, and
//! [`Ty::Unknown`] is a top/bottom element that is compatible with everything so a single error
//! doesn't cascade into a storm of follow-on errors.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    /// `bytes` — an immutable heap byte sequence (Python `bytes` model). Indexes/iterates to `int`
    /// (0–255), slices to `bytes`. Not a scalar; there is no `byte`/`u8` scalar type.
    Bytes,
    /// `bytearray` — the MUTABLE sibling of `bytes` (Python `bytearray` / Go mutable `[]byte` model).
    /// Constructor-only (`bytearray(...)`, no literal). Indexes/iterates to `int` (0–255), supports
    /// `ba[i] = x` (`IndexSet`), slices to `bytearray`. NOT `Hashable` (mutable ⇒ not a map/set key,
    /// like `list`). Sendable across the `--parallel` airlock by deep copy (like `list`).
    ByteArray,
    Nil,
    List(Box<Ty>),
    /// `map[K, V]` — insertion-ordered hash map. `K` is any `Hashable` type (int/str/bool or a
    /// struct implementing `hash(self) -> int`).
    Map(Box<Ty>, Box<Ty>),
    /// `set[T]` — insertion-ordered hash set. `T` is any `Hashable` type (int/str/bool or a struct
    /// implementing `hash(self) -> int`).
    Set(Box<Ty>),
    Func { params: Vec<Ty>, ret: Box<Ty> },
    /// `(T1, T2, …)` — a fixed-arity tuple (always ≥2 elements).
    Tuple(Vec<Ty>),
    /// A struct type, with its generic type arguments (empty for a non-generic struct). E.g.
    /// `Pair[int, str]` is `Struct("Pair", [Int, Str])`; a plain `Point` is `Struct("Point", [])`.
    Struct(String, Vec<Ty>),
    /// An enum type, with its generic type arguments (empty for a non-generic enum). E.g.
    /// `Tree[int]` is `Enum("Tree", [Int])`; a plain `Shape` is `Enum("Shape", [])`.
    Enum(String, Vec<Ty>),
    /// A bound generic type variable (e.g. `T` inside `fn max[T: Comparable]`). Opaque while
    /// checking a generic body; replaced by a concrete `Ty` at each call site via substitution.
    Param(String),
    /// `Result[T, E]` — `T` is the success type, `E` the error type. `T!` / `Result[T]` default
    /// `E` to the `Error` protocol existential (`Protocol("Error")`); `T!E` sets it explicitly.
    Result(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    /// `Channel[T]` — a shared mailbox for cross-task messages (C2). Element type `T` must be
    /// sendable. The handle itself is sendable, so reply channels work.
    Channel(Box<Ty>),
    /// `Shared[T]` — the cross-task mutable box (C3): one owner holds the value, writes are
    /// serialised. The handle is sendable (it's what `spawn` copies in — every task reaches the
    /// same box); the value isn't copied. Constructed value-first as `Shared(v)` (`T` = `typeof v`).
    Shared(Box<Ty>),
    /// `Atomic[T]` — the cross-task atomic box. Like `Shared[T]` (one box, many tasks; the handle is
    /// sendable, the value is copied in/out under a lock), but it presents atomic-operation methods
    /// (`load`/`store`/`exchange`/`cas`, plus `add`/`sub` on numeric `T`) instead of `Shared`'s
    /// `get`/`set`/`update`. Constructed value-first as `Atomic(v)` (`T` = `typeof v`).
    Atomic(Box<Ty>),
    /// `Executor` — the C5 escape hatch: an explicitly-owned work queue for detached tasks that
    /// outlive a `parallel:` scope. Non-generic; the handle is sendable (like `Channel`/`Shared`).
    Executor,
    /// `Socket` — a connected non-blocking TCP stream (D6), produced by `std.net.connect` /
    /// `Listener.accept`. Non-generic; the handle is sendable (a `spawn`ed fiber can service it).
    Socket,
    /// `Listener` — a non-blocking accepting TCP socket (D6), produced by `std.net.listen`. Non-generic
    /// and sendable, like `Socket`.
    Listener,
    /// `ptr` — an opaque C-ABI pointer handle (a raw `void*`). A builtin marshalling primitive (peer
    /// of `int`/`float`/`bool`/`str`) usable in `extern "lib":` signatures. Fully opaque: no methods,
    /// no fields; only `==`/`!=` against another `ptr` (incl. `std.ffi.null()`) and pass/return.
    /// Untyped (one `ptr` for every handle), never auto-freed. Sendable (a plain address).
    Ptr,
    /// A protocol used *as a value type* (existential), e.g. the default error type `Error`. A
    /// concrete type is assignable to it iff it satisfies the protocol; only the protocol's own
    /// methods are callable on it. Type-erased at runtime (methods dispatch by name).
    Protocol(String),
    /// An imported module, identified by the name it's bound under in the current module. Member
    /// access (`io.read()`) resolves against the module's exported signatures.
    Module(String),
    /// Un-inferable, or "an error was already reported here". Compatible with everything.
    Unknown,
}

impl Ty {
    pub fn list(inner: Ty) -> Ty {
        Ty::List(Box::new(inner))
    }
    pub fn map(key: Ty, value: Ty) -> Ty {
        Ty::Map(Box::new(key), Box::new(value))
    }
    pub fn set(elem: Ty) -> Ty {
        Ty::Set(Box::new(elem))
    }
    /// `Result[T]` / `T!` — error type defaults to the `Error` protocol.
    pub fn result(inner: Ty) -> Ty {
        Ty::Result(Box::new(inner), Box::new(Ty::error_proto()))
    }
    /// `Result[T, E]` / `T!E` — explicit error type.
    pub fn result_e(inner: Ty, err: Ty) -> Ty {
        Ty::Result(Box::new(inner), Box::new(err))
    }
    /// The default error type: the `Error` protocol as an existential.
    pub fn error_proto() -> Ty {
        Ty::Protocol("Error".to_string())
    }
    pub fn option(inner: Ty) -> Ty {
        Ty::Option(Box::new(inner))
    }
    pub fn channel(inner: Ty) -> Ty {
        Ty::Channel(Box::new(inner))
    }
    pub fn shared(inner: Ty) -> Ty {
        Ty::Shared(Box::new(inner))
    }
    pub fn atomic(inner: Ty) -> Ty {
        Ty::Atomic(Box::new(inner))
    }
    /// A non-generic struct type (no type arguments) — the common case.
    pub fn strukt(name: impl Into<String>) -> Ty {
        Ty::Struct(name.into(), Vec::new())
    }

    /// Is this a number (`int` or `float`)?
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
    }
}

/// Structural compatibility for assignment / argument passing. [`Ty::Unknown`] on either side
/// (at any depth) matches anything, which is what keeps one error from cascading.
pub fn compatible(expected: &Ty, actual: &Ty) -> bool {
    use Ty::*;
    match (expected, actual) {
        (Unknown, _) | (_, Unknown) => true,
        (Int, Int) | (Float, Float) | (Bool, Bool) | (Str, Str) | (Bytes, Bytes) | (ByteArray, ByteArray) | (Nil, Nil) => true,
        (List(a), List(b)) | (Option(a), Option(b)) | (Channel(a), Channel(b)) | (Shared(a), Shared(b))
        | (Atomic(a), Atomic(b)) => {
            compatible(a, b)
        }
        (Result(at, ae), Result(bt, be)) => compatible(at, bt) && compatible(ae, be),
        // A protocol existential: identity matches; `str` conforms to `Error` intrinsically.
        // Struct conformance needs the registry — handled by `Checker::assignable`, not here.
        (Protocol(a), Protocol(b)) => a == b,
        (Protocol(p), Str) if p == "Error" => true,
        (Map(ka, va), Map(kb, vb)) => compatible(ka, kb) && compatible(va, vb),
        (Set(a), Set(b)) => compatible(a, b),
        (Struct(a, aa), Struct(b, ba)) | (Enum(a, aa), Enum(b, ba)) => {
            a == b && aa.len() == ba.len() && aa.iter().zip(ba).all(|(x, y)| compatible(x, y))
        }
        (Executor, Executor) | (Socket, Socket) | (Listener, Listener) | (Ptr, Ptr) => true,
        (Module(a), Module(b)) | (Param(a), Param(b)) => a == b,
        (Func { params: p1, ret: r1 }, Func { params: p2, ret: r2 }) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2).all(|(a, b)| compatible(a, b))
                && compatible(r1, r2)
        }
        (Tuple(a), Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| compatible(x, y)),
        _ => false,
    }
}

/// Render a `Ty` for a user-facing diagnostic about a **ref binding/param**, mapping the lowered
/// `Ref[T]` box back to the `ref T` surface the user actually wrote (spec item 8 — `ref` is
/// transparent; the user never typed `Ref`). Recurses so a nested box (`list[Ref[int]]`) also reads
/// `ref` (`list[ref int]`). Plain types are unchanged. Use this only where the type originates from a
/// `ref` binding — ordinary diagnostics keep the literal `Ty` `Display`.
pub fn ref_display(ty: &Ty) -> String {
    match ty {
        Ty::Struct(n, args) if n == "Ref" && args.len() == 1 => format!("ref {}", ref_display(&args[0])),
        Ty::List(t) => format!("list[{}]", ref_display(t)),
        Ty::Set(t) => format!("set[{}]", ref_display(t)),
        Ty::Option(t) => format!("Option[{}]", ref_display(t)),
        Ty::Map(k, v) => format!("map[{}, {}]", ref_display(k), ref_display(v)),
        Ty::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(ref_display).collect();
            format!("({})", parts.join(", "))
        }
        // Anything without a `Ref[T]` to unwrap renders exactly as its normal `Display`.
        other => other.to_string(),
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::Bytes => write!(f, "bytes"),
            Ty::ByteArray => write!(f, "bytearray"),
            Ty::Nil => write!(f, "nil"),
            Ty::List(t) => write!(f, "list[{t}]"),
            Ty::Map(k, v) => write!(f, "map[{k}, {v}]"),
            Ty::Set(t) => write!(f, "set[{t}]"),
            // `Result[T]` when the error is the default `Error` or still unconstrained (`?`);
            // `Result[T, E]` for an explicit error type.
            Ty::Result(t, e) => match e.as_ref() {
                Ty::Protocol(p) if p == "Error" => write!(f, "Result[{t}]"),
                Ty::Unknown => write!(f, "Result[{t}]"),
                _ => write!(f, "Result[{t}, {e}]"),
            },
            Ty::Option(t) => write!(f, "Option[{t}]"),
            Ty::Channel(t) => write!(f, "Channel[{t}]"),
            Ty::Shared(t) => write!(f, "Shared[{t}]"),
            Ty::Atomic(t) => write!(f, "Atomic[{t}]"),
            Ty::Executor => write!(f, "Executor"),
            Ty::Socket => write!(f, "Socket"),
            Ty::Listener => write!(f, "Listener"),
            Ty::Ptr => write!(f, "ptr"),
            Ty::Protocol(n) => write!(f, "{n}"),
            Ty::Struct(n, args) | Ty::Enum(n, args) => {
                write!(f, "{n}")?;
                if !args.is_empty() {
                    write!(f, "[")?;
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{a}")?;
                    }
                    write!(f, "]")?;
                }
                Ok(())
            }
            Ty::Param(n) => write!(f, "{n}"),
            Ty::Module(n) => write!(f, "module {n}"),
            Ty::Func { params, ret } => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}")
            }
            Ty::Tuple(elems) => {
                write!(f, "(")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{t}")?;
                }
                write!(f, ")")
            }
            Ty::Unknown => write!(f, "?"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_compatible_with_anything() {
        assert!(compatible(&Ty::Unknown, &Ty::Int));
        assert!(compatible(&Ty::Int, &Ty::Unknown));
        // and at depth: Result[?] accepts Result[int]
        assert!(compatible(&Ty::result(Ty::Unknown), &Ty::result(Ty::Int)));
    }

    #[test]
    fn primitives_must_match_exactly() {
        assert!(compatible(&Ty::Int, &Ty::Int));
        assert!(!compatible(&Ty::Int, &Ty::Float)); // no implicit int->float
        assert!(!compatible(&Ty::Str, &Ty::Int));
    }

    #[test]
    fn nominal_types_compare_by_name() {
        assert!(compatible(&Ty::strukt("Point"), &Ty::strukt("Point")));
        assert!(!compatible(&Ty::strukt("Point"), &Ty::strukt("Vec")));
        assert!(!compatible(&Ty::strukt("Point"), &Ty::Enum("Point".into(), vec![])));
        // Generic structs compare by name AND type arguments.
        assert!(compatible(
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]),
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str])
        ));
        assert!(!compatible(
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]),
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Int])
        ));
    }

    #[test]
    fn display_renders_source_forms() {
        assert_eq!(Ty::list(Ty::Int).to_string(), "list[int]");
        assert_eq!(Ty::result(Ty::Int).to_string(), "Result[int]");
        assert_eq!(Ty::strukt("Point").to_string(), "Point");
        assert_eq!(Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]).to_string(), "Pair[int, str]");
        assert_eq!(
            Ty::Func { params: vec![Ty::Int, Ty::Str], ret: Box::new(Ty::Bool) }.to_string(),
            "fn(int, str) -> bool"
        );
    }
}
