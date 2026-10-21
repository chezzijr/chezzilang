//! Checker: builtin method name lists, `mutates_receiver`, builtin container signatures and type
//! docs.
//! Split out of `src/checker/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::base::builtin_method::DECLARED_METHODS;

/// Does `method` mutate its receiver in place (as opposed to returning a new value)? Maps the
/// receiver's type to its kind and reads the one table, `crate::native::is_mutating_native_kind`.
///
/// The handle types (`Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`Writer`/
/// `Reader`) are absent BY DESIGN and must stay absent: they cross the airlock by handle, so a
/// task-side `s.update(...)` / `ch.send(v)` IS visible to the parent (measured). This predicate feeds
/// the DEC-089 read-temporary warning plus D4 rules 1 and 2, so handles must stay absent.
///
/// User-struct methods use the separate `StructInfo.self_writers` fixed-point summary.
pub(super) fn mutates_receiver(recv: &Ty, method: &str) -> bool {
    let kind = match recv {
        Ty::List(_) => "List",
        Ty::Map(..) => "Map",
        Ty::Set(_) => "Set",
        Ty::ByteArray => "bytearray",
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Bytes
        | Ty::Nil
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => return false,
    };
    crate::native::is_mutating_native_kind(kind, method)
}

// The bespoke `str_method_sig` / `bytes_method_sig` / `bytearray_method_sig` arms are RETIRED (phase
// 5a-containers): every one of their FLAT sigs is now declared as a body-less `native fn` method inside a
// `native struct str` / `bytes` / `bytearray` in `std/prelude.chz`, harvested into that reserved scalar's
// method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with NO type
// args (the scalars are non-generic — the identity path returns the stored sig verbatim). The
// `str(x)`/`bytes(x)`/`bytearray(x)` CTORS stay the `native ctor` decls, and runtime dispatch stays
// Rust-inline (`vm/mod.rs core_method` / `bytes_method`). The `bytearray.extend` special-case (its arg may
// be any of bytes|bytearray|List[int], not a flat `FnSig`) stays an explicit branch in `infer_method_call`
// BEFORE the table lookup.

// The bespoke `list_method_sig` / `map_method_sig` / `set_method_sig` arms are RETIRED (phase
// 5a-containers): every one of their FLAT sigs is now declared as a body-less `native fn` method inside a
// `native struct List[T]` / `Map[K, V]` / `Set[T]` in `std/prelude.chz`, harvested into that reserved
// type's method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with
// the value's element/key/value type substituted for the sig's `Ty::Param`s. The literal syntax + turbofish
// ctor stay compiler-wired (`resolve_type`/`builtin_container_sig`). The generic-recovery `List` methods
// (`map`/`filter`/`fold`/`sort_by`/`sort_by_key`) are now ALSO file-backed as `native fn map[U](...)` etc:
// the generic solver's closure-return LOOP-BACK recovers a return-position `[U]`/`[K]` from an
// (even unannotated) closure body, so the `Ty::List` arm routes their harvested sigs through
// `infer_generic_method`. The `sum` numeric-element gate is the sole surviving residual (a plain
// `sum(self) -> T` would wrongly accept a non-numeric list); `sort` is file-backed via `where T: Comparable`.

// The bespoke `channel_method_sig` arm is RETIRED (phase 5a-containers): every one of its FLAT sigs
// (`send`/`try_send`/`recv`/`try_recv`/`close`/`trip`/`len`/`cap`) is now declared as a body-less
// `native fn` method inside a `native struct Channel[T]` in `std/prelude.chz`, harvested into the
// reserved type's method table (re-seeded by `seed_stdlib_structs`) and looked up via
// `native_handle_method` with the channel's element type substituted for the sig's `Ty::Param("T")`.
// The `Channel[T](cap)` ctor stays compiler-wired and runtime dispatch stays Rust-inline
// (`vm/netio.rs channel_method`); only the checker sig moved to the file-backed mirror.

// The bespoke `shared_method_sig` / `rwshared_method_sig` / `atomic_method_sig` / `executor_method_sig`
// arms are RETIRED (phase 4c-concurrency): every one of their sigs is now declared as a body-less
// `native fn` method inside a `native struct` in `std/concurrency.chz`, harvested into that type's
// method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with the
// box's element type substituted for `Ty::Param("T")`. The two constraints a plain sig cannot express
// stay as thin residuals: `RwShared.read`'s closure return R is recovered in the `Ty::RwShared` arm,
// and `Atomic.add`/`sub`'s numeric-`T` gate is a `!elem.is_numeric()` check in the `Ty::Atomic` arm.

/// A DISPLAY/PLACEHOLDER signature for the STILL-SYNTHETIC free / constructor builtins (editor hover,
/// v1): the GENERIC / reserved-type container & runtime ctors
/// (`range`/`List`/`Map`/`Set`/`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). (`print` is
/// no longer here — it is now the file-backed variadic `native fn print(...)` decl, so its sig flows
/// through `builtin_sig` BEFORE this fallback.) This is the FLAT sig the container ctors keep here BECAUSE their real generic
/// type-identity (`List[int]` → `Ty::List(Int)`, …) is NOT a flat `FnSig` and lives in
/// `resolve_type`/`infer_named_call`; their `Builtin` ids carry only their dispatch.
/// The eight MIGRATED universe builtins
/// (`ord`/`chr`/`panic`/`int`/`float`/`str`/`bytes`/`bytearray`) are NOT here — their sigs now come
/// from `std/prelude.chz` via [`Checker::builtin_sig`]. Unlike the `infer_named_call` arms — whose
/// result is arg-dependent (overloads, variadics, named params, type-arg-driven element types) — this
/// returns ONE canonical positional shape per name, mirrored by hand from `docs/reference/stdlib/ §1`.
/// Polymorphic-input slots use `Ty::Unknown` (renders `?`); the precise concrete RETURN is the hover
/// payload.
pub(super) fn builtin_container_sig(name: &str) -> Option<FnSig> {
    // `print`'s signature is now the file-backed variadic `native fn print(...)` decl in
    // `std/prelude.chz` (harvested into `native_prelude_sigs`, read by `builtin_sig` BEFORE this
    // fallback) — the last synthetic Rust sig, retired. Only the generic container/runtime ctors, whose
    // type-arg-driven identity is not a flat `FnSig`, remain here.
    let (params, ret) = match name {
        // Container constructors --------------------------------------------------------------
        // Element/key/value types are inferred from the argument → `?` slots.
        "List" => (vec![Ty::Unknown], Ty::list(Ty::Unknown)),
        "Set" => (vec![Ty::Unknown], Ty::set(Ty::Unknown)),
        "Map" => (vec![Ty::Unknown], Ty::map(Ty::Unknown, Ty::Unknown)),
        // Runtime constructors ----------------------------------------------------------------
        // `Channel[T]()` is type-arg-driven, no value arg → no params.
        "Channel" => (vec![], Ty::channel(Ty::Unknown)),
        "Shared" => (vec![Ty::Unknown], Ty::shared(Ty::Unknown)),
        "RwShared" => (vec![Ty::Unknown], Ty::rwshared(Ty::Unknown)),
        "Atomic" => (vec![Ty::Unknown], Ty::atomic(Ty::Unknown)),
        "AtomicInt" => (vec![Ty::Int], Ty::AtomicInt),
        // `Executor()` — zero-arg work queue.
        "Executor" => (vec![], Ty::Executor),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Editor hover (Tier C): a concise one-line "how to use" blurb for a BUILTIN/STDLIB type or ctor
/// name, paraphrased from `docs/reference/stdlib/ §1–3`. For a type that owns a method table, a
/// `\nmethods: a, b, c` line is appended from the `DECLARED_METHODS` row of that type (generated
/// from the `native fn` decls in `std/`, so every advertised method provably exists). Returns `None`
/// for any non-builtin name (a user type's own docstring already flows via `name_docs`). Built only
/// under the hover probe (the call sites are `hover_probe.is_some()`-gated), never on the hot check
/// path. The method-bearing types are listed first; the no-method types (range/tuple/Result/…) only
/// get a usage line.
pub(super) fn builtin_type_doc(name: &str) -> Option<String> {
    let usage: &str = match name {
        // Containers (own a method table) --------------------------------------------------------
        "List" => {
            "growable ordered sequence — List[T](); index with xs[i], iterate with `for x in xs:`"
        }
        "Map" => "key→value hash map — Map[K, V](); index with m[k], iterate with `for k, v in m:`",
        "Set" => "unordered collection of unique elements — Set[T](); operators | & - ^",
        "str" => "immutable UTF-8 text; index/slice by codepoint, concatenate with +",
        "bytes" => "immutable byte sequence — bytes(x); index with b[i] (byte as int)",
        "bytearray" => "mutable byte buffer — bytearray(x); index/assign b[i], push/pop bytes",
        // Concurrency / runtime types (own a method table) ---------------------------------------
        "Channel" => {
            "FIFO mailbox between tasks — Channel[T](); iterate received values with `for v in ch:`"
        }
        "Shared" => "cross-task shared cell — Shared(v) (import std.concurrency)",
        "RwShared" => {
            "cross-task read-write cell, many readers OR one writer — RwShared(v) (import std.concurrency)"
        }
        "Atomic" => {
            "cross-task atomic cell — Atomic(v) (import std.concurrency; add/sub need numeric T)"
        }
        "AtomicInt" => "monomorphic lock-free int atomic — AtomicInt(v) (import std.concurrency)",
        "Executor" => "task pool — Executor() (import std.concurrency)",
        "Socket" => "TCP connection handle from std.net connect()/accept()",
        "Listener" => "TCP listening socket from std.net listen()",
        "Writer" => {
            "write-only file/stream handle from std.io create()/append()/stdout()/stderr()/buffered()"
        }
        "Reader" => {
            "read-only file handle from std.io open() — line/chunk streaming of a large file"
        }
        // Types without a built-in method table (usage line only) --------------------------------
        "Range" => {
            "a range of ints as a value — a..b, a..=b, ..b, a.., or Range(start, end, step); iterable, `in`, slices"
        }
        "tuple" => "fixed-size heterogeneous group — (a, b); destructure with `x, y := t`",
        "Result" => {
            "success-or-error — T!E / T!; build with a plain value or !e, handle with ?, ??, else or match"
        }
        "Option" => {
            "a value or nothing — T?; a plain value or None, handle with ?, ??, else or match"
        }
        "Iterator" => {
            "lazy element cursor — Iterator[T] from a generator or `.iter()`; drive with `for x in it:`"
        }
        _ => return None,
    };
    let mut doc = usage.to_string();
    if let Some((_, bodyless, bodied)) = DECLARED_METHODS.iter().find(|(ty, _, _)| *ty == name) {
        let shown: Vec<&str> = bodyless
            .iter()
            .chain(bodied.iter())
            .copied()
            .filter(|m| !m.starts_with('_'))
            .collect();
        doc.push_str("\nmethods: ");
        doc.push_str(&shown.join(", "));
    }
    Some(doc)
}
