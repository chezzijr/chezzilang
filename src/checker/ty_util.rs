//! Checker: type utilities: protocols prebuilt, pinning, `merge_unknown`, `subst`, instantiation,
//! `unify`, binary-operator protocols.
//! Split out of `src/checker/mod.rs`. `super::*` is the parent module.

use super::*;

/// Render a resolved type-arg list for a user-facing redirect hint (`int, str`), used by the
/// removed-gliding-form error to suggest the type-side form `Enum[int, str].Variant(...)`.
pub(super) fn render_targs(targs: &[Ty]) -> String {
    targs
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first identifier bound more than once WITHIN a single pattern (Rust's rule), or `None`. Walks
/// a pattern's binders but COUNTS instead of dedup'ing: `_` (wildcard) binds nothing and is
/// skipped; literals/ranges bind nothing. Or-alternatives are NOT descended — each `A(x) | B(x)`
/// alternative is its own binding context (consistency across alts is governed elsewhere); a
/// duplicate inside one alternative is caught when `bind_match_arm` recurses on that alternative.
/// `is_binder(name)` returns `true` iff a bare `Pattern::Ident(name)` actually BINDS a fresh variable
/// here, rather than naming a (refutable) nullary variant. A bare `None`/`Ok`/`Err`/`Some` or a user
/// variant name binds nothing (it's a variant test — see `bind_subpattern`), so two of them in one
/// pattern (e.g. `(None, None, None)`) is NOT a duplicate binding. `is_binder` reads
/// `Checker::bare_pattern_name`.
pub(super) fn first_duplicate_binder(
    p: &Pattern,
    is_binder: &impl Fn(&str) -> bool,
) -> Option<String> {
    fn go(
        p: &Pattern,
        seen: &mut std::collections::HashSet<String>,
        is_binder: &impl Fn(&str) -> bool,
    ) -> Option<String> {
        match p {
            Pattern::Ident(n, _, _) => {
                if !is_binder(n) {
                    return None;
                }
                if !seen.insert(n.clone()) {
                    return Some(n.clone());
                }
                None
            }
            Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) => {
                for b in bindings {
                    if let Some(dup) = go(b, seen, is_binder) {
                        return Some(dup);
                    }
                }
                None
            }
            // An or-pattern's alternatives are separate binding contexts, so a name reused ACROSS
            // alternatives (`A(x) | B(x)`) is NOT a duplicate. But (a) each alternative must itself
            // be duplicate-free, and (b) the names an or-pattern binds STILL collide with binders
            // OUTSIDE the or (`(x, A(x) | B(x))` binds `x` twice on a matching path). So check each
            // alternative on its own, then merge the or's binder set ONCE into the outer `seen`.
            Pattern::Or(alts) => {
                let mut or_binds = std::collections::BTreeSet::new();
                for alt in alts {
                    let mut alt_seen = std::collections::HashSet::new();
                    if let Some(dup) = go(alt, &mut alt_seen, is_binder) {
                        return Some(dup);
                    }
                    or_binds.extend(alt_seen);
                }
                for n in or_binds {
                    if !seen.insert(n.clone()) {
                        return Some(n);
                    }
                }
                None
            }
            Pattern::Carrier { inner, .. } => go(inner, seen, is_binder),
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => None,
        }
    }
    let mut seen = std::collections::HashSet::new();
    go(p, &mut seen, is_binder)
}

/// The prebuilt protocols every program starts with. `Comparable` requires
/// `compare(self, other: Self) -> int`; primitives (int/float/str) satisfy it intrinsically.
/// M22 — structural equality of two protocol method signatures (params + return + arity), used to
/// dedup identical embed-pulled methods (the legal diamond) vs. flag a conflicting-signature embed.
pub(super) fn fn_sig_eq(a: &FnSig, b: &FnSig) -> bool {
    a.params == b.params && a.ret == b.ret && a.min_params == b.min_params
}

pub(super) fn prebuilt_protocols() -> HashMap<String, ProtocolInfo> {
    let mut m = HashMap::new();
    m.insert(
        // `Any` — the TOP type: an EMPTY structural protocol (zero embeds, zero methods) so EVERY
        // type satisfies it (scalars included — see the empty-protocol short-circuit in
        // `satisfies_args_d`). This stays the RUNTIME source of truth; since empty protocols are now
        // expressible (`protocol Any:\n    pass`), it is ALSO mirrored + drift-guarded in
        // `std/prelude.chz` like the other reserved protocols. A user empty protocol behaves
        // identically (the accept-all behaviour is not special-cased on the name "Any"). Used as the
        // honest element type of a variadic display slot (`print(...args: Any)`); NOT dynamic typing —
        // a value typed `Any` carries no methods, so it can only be passed around / displayed.
        "Any".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: Vec::new(),
        },
    );
    m.insert(
        // `Comparable` embeds `Eq` (M23) — mirrors Rust's `Ord: Eq`: a type ordered must also be
        // equatable, and `eq` must agree with `compare` (the implementor's contract, unchecked). Built
        // with the SAME `embeds` field `Arithmetic` uses — no special-casing. int/float/str still need
        // no explicit `eq` method: `satisfies_args_d`'s embed-flattening loop DOES run for them now
        // (embeds is no longer empty), but it recurses into `Eq`'s OWN intrinsic-grant early-out (every
        // scalar satisfies `Eq`), so the embed passes trivially before `Comparable`'s own intrinsic
        // grant is reached — no `Comparable`-specific short-circuit needed.
        "Comparable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: vec![TyBound {
                name: "Eq".to_string(),
                args: Vec::new(),
            }],
            // receiver `self` (Unknown), `other: Self` (Param "Self"), returning int.
            methods: vec![(
                "compare".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Int),
            )],
        },
    );
    m.insert(
        // `Eq` — user-defined equality: `eq(self, other: Self) -> bool`. Every scalar satisfies it
        // intrinsically (all FOUR — `==` is defined on `bool` too, unlike `Comparable`'s ordering);
        // a struct/enum satisfies it structurally through its own `eq`. Embedded by `Comparable` (M23):
        // a type ordered must also be equatable.
        "Eq".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown), `other: Self` (Param "Self"), returning bool.
            methods: vec![(
                "eq".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Bool),
            )],
        },
    );
    m.insert(
        "Stringable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown) only, returning str. A struct with `str(self) -> str`
            // satisfies it; `print`/`str()`/interpolation dispatch to that method at runtime.
            methods: vec![("str".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Str))],
        },
    );
    m.insert(
        // The default error type (Go-style `error`): one method `message(self) -> str`. `str`
        // conforms intrinsically; any struct with `message(self) -> str` conforms structurally.
        "Error".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "message".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Str),
            )],
        },
    );
    m.insert(
        // W7-8 — `PathLike`: the INPUT position of every path-taking std fn (Python's `os.PathLike`,
        // Rust's `AsRef<Path>`). Its sole method `as_path(self) -> bytes` hands back the RAW OS bytes,
        // so a non-UTF-8 filename never has to round-trip through the validated-UTF-8 `str`.
        // `str`/`bytes`/`bytearray` conform INTRINSICALLY (they have no `as_path` of their own — the
        // grant rows in `INTRINSIC_PROTO_METHODS` + the `satisfies_args_d` early-out are the only
        // seam); `path.Path` conforms STRUCTURALLY through its own `as_path`.
        // Deliberately DISTINCT from the future byte-DATA protocol's `as_bytes`, so a type satisfying
        // both has no method ambiguity.
        "PathLike".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "as_path".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Bytes),
            )],
        },
    );
    m.insert(
        "Hashable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown) only, returning int. A struct with `hash(self) -> int`
            // satisfies it. WIRED TO MAP/SET KEYS: `map`/`set` are real hash tables (insertion-order
            // entries + a hash→position index), so any `Hashable` type can be a key/element — the
            // scalars int/str/bool intrinsically, or a struct via its `hash()` (dispatched at
            // runtime, hash confirmed by structural `==`). CONTRACT: two structurally-equal structs
            // (the `==` used to confirm a probe) MUST return the same `hash()` — the implementor owns
            // this (like Rust's `Hash`/`Eq`); the checker can't enforce purity.
            methods: vec![("hash".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Int))],
        },
    );
    // Per-operator numeric protocols (M10-G3, M22): a struct satisfying `Add`/`Sub`/`Mul`/`Div`/`Mod`
    // (method `add`/`sub`/`mul`/`div`/`mod`(self, other: Self) -> Self) overloads `+`/`-`/`*`/`/`/`%`.
    // `Self` for `other` and the return makes them binary same-type operators (mirrors `Comparable`'s
    // `compare`).
    for (proto, method) in [
        ("Add", "add"),
        ("Sub", "sub"),
        ("Mul", "mul"),
        ("Div", "div"),
        ("Mod", "mod"),
        ("BitAnd", "bitand"),
        ("BitOr", "bitor"),
        ("BitXor", "bitxor"),
        ("Shl", "shl"),
        ("Shr", "shr"),
    ] {
        m.insert(
            proto.to_string(),
            ProtocolInfo {
                type_params: Vec::new(),
                embeds: Vec::new(),
                methods: vec![(
                    method.to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("Self".into())],
                        Ty::Param("Self".into()),
                    ),
                )],
            },
        );
    }
    // `Neg` (M22) — the UNARY `-` protocol: a struct/type-param satisfying `Neg` (method
    // `neg(self) -> Self`, one param `self` only, no `other`) overloads unary `-`. int/float satisfy
    // it intrinsically (their native negation). Mirrors `Stringable`'s single-`self`-param shape.
    m.insert(
        "Neg".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "neg".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Param("Self".into())),
            )],
        },
    );
    // `Arithmetic` (M22) — a builtin protocol BUNDLE: embeds `Add + Sub + Mul + Div` and adds no own
    // methods. `[T: Arithmetic]` flattens to "has add/sub/mul/div", so int/float and any 4-op struct
    // satisfy it. Built with the SAME `embeds` field user bundles use — no special-casing of builtins.
    m.insert(
        "Arithmetic".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: vec![
                TyBound {
                    name: "Add".to_string(),
                    args: Vec::new(),
                },
                TyBound {
                    name: "Sub".to_string(),
                    args: Vec::new(),
                },
                TyBound {
                    name: "Mul".to_string(),
                    args: Vec::new(),
                },
                TyBound {
                    name: "Div".to_string(),
                    args: Vec::new(),
                },
            ],
            methods: Vec::new(),
        },
    );
    // `Num` (TICKET-214) — the numeric bundle: every operator whose result is `T` for both int and
    // float (`+ - * / %`, unary `-`), plus ordering and `==`. SEALED to int/float in
    // `satisfies_args_d`; the embeds are what a `T: Num` body's operators read.
    m.insert(
        "Num".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: ["Arithmetic", "Mod", "Neg", "Comparable"]
                .iter()
                .map(|n| TyBound {
                    name: (*n).to_string(),
                    args: Vec::new(),
                })
                .collect(),
            methods: Vec::new(),
        },
    );
    // `Iterator[T]` — the language's one parameterized protocol. The method shape mirrors the
    // structural detection (`next(self) -> Option[T]`); conformance is decided in `satisfies` — a cursor
    // (`.iter()` / a generator result) or a user struct with a structural `next`, NOT a raw collection
    // (which holds no position and satisfies only `Iterable`, W6-3b) and NOT the generic structural
    // loop. The bound's `[T]` arg recovers the element type at call sites.
    m.insert(
        "Iterator".to_string(),
        ProtocolInfo {
            // One type param: the element `next` yields.
            type_params: vec!["Elem".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "next".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Option(Box::new(Ty::Param("Elem".into()))),
                ),
            )],
        },
    );
    // `Iterable[T]` — "can produce a fresh cursor". The method shape mirrors the structural detection
    // (`iter(self) -> Iterator[T]`); conformance is decided in `satisfies_args` (built-in collections
    // + any `Iterator[T]` intrinsically — every Iterator IS Iterable via `iter() == self` — plus a
    // user struct with a structural `iter`), NOT the generic structural loop, and the bound's `[T]`
    // recovers the element type at call sites (`infer_method_call`'s `Iterable.iter` special-case).
    // Distinct from `Iterator[T]`: an `Iterable` only promises a cursor; an `Iterator` also has `next`.
    // So `[S: Iterable[T], T]` is the bound that accepts ANY iterable (Rust's `IntoIterator`), and
    // `[S: Iterator[T], T]` is the one that needs a real cursor (Rust's `Iterator`).
    m.insert(
        "Iterable".to_string(),
        ProtocolInfo {
            type_params: vec!["Elem".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "iter".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Struct("Iterator".to_string(), vec![Ty::Param("Elem".into())]),
                ),
            )],
        },
    );
    // The indexing pair (Rust `Index`/`IndexMut`-style) + `Slice`. Built-in `list`/`map`/`str`
    // satisfy these intrinsically (see `satisfies_args`); a user struct satisfies them structurally
    // via `index`/`set_index`/`slice`. `K`/`V`/`R` are recovered at call sites by `recover_index_args`
    // (mirrors `Iterator[T]`'s element recovery).
    m.insert(
        "Index".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "index".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown, Ty::Param("K".into())],
                    Ty::Param("V".into()),
                ),
            )],
        },
    );
    m.insert(
        "IndexSet".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            embeds: Vec::new(),
            methods: vec![
                (
                    "index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into())],
                        Ty::Param("V".into()),
                    ),
                ),
                (
                    "set_index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into()), Ty::Param("V".into())],
                        Ty::Nil,
                    ),
                ),
            ],
        },
    );
    m.insert(
        "Slice".to_string(),
        ProtocolInfo {
            type_params: vec!["R".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "slice".to_string(),
                // TICKET-253: the one argument is the `Range` value (`std.prelude`'s struct, bare key).
                FnSig::plain(
                    vec![Ty::Unknown, Ty::Struct("Range".into(), Vec::new())],
                    Ty::Param("R".into()),
                ),
            )],
        },
    );
    // `Convert[S]` — the extensible type-conversion protocol (Rust `From`, target-keyed). Its sole
    // method `convert(x: S) -> Self` is STATIC (first param is `x: S`, NOT `self`), so it harvests to
    // params `[Ty::Param("S")]` (no leading `Ty::Unknown` self slot) with `is_static: true`. `S` is the
    // one arity param, so the bound reads `[T: Convert[str]]`. Slice 1: declared + reserved +
    // bound-parseable only; structural witnessing (slice 2) + `T.convert` through a bound (slice 3) are
    // NOT wired yet. `is_static` is load-bearing for those slices but NOT compared by `fn_sig_eq`/the
    // drift guard (params + ret + min_params only); set it to match the parsed prelude shape.
    m.insert(
        "Convert".to_string(),
        ProtocolInfo {
            type_params: vec!["S".to_string()],
            embeds: Vec::new(),
            methods: vec![("convert".to_string(), {
                let mut sig = FnSig::plain(vec![Ty::Param("S".into())], Ty::Param("Self".into()));
                sig.is_static = true;
                sig
            })],
        },
    );
    // `Contains[Item]` — the membership protocol (Python `__contains__`): `x in obj` dispatches to
    // `contains(self, item: Item) -> bool`. Built-in list/set/map/str test membership intrinsically
    // (see `op_contains`); a user struct/enum satisfies it structurally. `Item` (the element type) is
    // recovered at the `in` site by `contains_item_ty` (mirrors `Index[K,V]`).
    m.insert(
        "Contains".to_string(),
        ProtocolInfo {
            type_params: vec!["Item".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "contains".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Item".into())], Ty::Bool),
            )],
        },
    );
    m
}

/// The substitution from a struct's type parameters to a concrete instantiation's type arguments
/// (`Stack[int]` ⇒ `{T: int}`). Empty for a non-generic struct.
pub(super) fn struct_param_map(info: &StructInfo, targs: &[Ty]) -> HashMap<String, Ty> {
    info.type_params
        .iter()
        .map(|tp| tp.name.clone())
        .zip(targs.iter().cloned())
        .collect()
}

/// Is `ty` fully concrete — free of any type parameter or `Unknown`? Used to decide when a forwarded
/// parameterized bound's args can be compared strictly (only a concrete-vs-concrete mismatch is an
/// error; anything still generic forwards loosely).
pub(super) fn ty_fully_concrete(ty: &Ty) -> bool {
    ty_all_holes(ty, &|_| false)
}

/// True iff every `Ty::Param` and `Ty::Unknown` leaf of `ty` satisfies `ok`. The one structural
/// walk under [`ty_fully_concrete`], [`ty_concrete_but`] and [`pin_generic_fn_value`]; they differ only at
/// the leaves. A type with no such leaf (a scalar, a module) answers `true`.
pub(super) fn ty_all_holes(ty: &Ty, ok: &dyn Fn(&Ty) -> bool) -> bool {
    let all = |ts: &[Ty]| ts.iter().all(|t| ty_all_holes(t, ok));
    match ty {
        Ty::Unknown | Ty::Param(_) | Ty::Var(_) => ok(ty),
        Ty::List(x) | Ty::Option(x) | Ty::Set(x) => ty_all_holes(x, ok),
        Ty::Map(k, v) | Ty::Result(k, v) => ty_all_holes(k, ok) && ty_all_holes(v, ok),
        // A parameterized protocol existential carrying a free type-param (`Container[T]`) is NOT
        // concrete — recurse into the carried args (no catch-all Protocol laundering).
        Ty::Struct(_, a) | Ty::Enum(_, a) | Ty::Protocol(_, a) => all(a),
        Ty::Tuple(ts) => all(ts),
        Ty::Func { params, ret, .. } => all(params) && ty_all_holes(ret, ok),
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::BuiltinFn { .. }
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
        | Ty::Module(_) => true,
    }
}

/// Is `ty` concrete up to the type params `rigid` accepts — an in-scope param standing as a type
/// here ([`Checker::rigid_param`])? An `Unknown` leaf, or any other param, answers `false`. The gate
/// of every hint that only SEEDS `expected_hint`; a gate that feeds `assignable` keeps
/// [`ty_fully_concrete`] (DEC-054, DEC-025).
pub(super) fn ty_concrete_but(ty: &Ty, rigid: &dyn Fn(&str) -> bool) -> bool {
    ty_all_holes(ty, &|h| matches!(h, Ty::Param(n) if rigid(n)))
}

/// The verdict of [`pin_generic_fn_value`] — the ONE answer to *"does this position determine every
/// type parameter of the generic function read here?"*.
pub(super) enum FnValuePin {
    /// Every type param bound to a concrete type. Carries the bindings (so the caller can
    /// `enforce_bounds`) and the fully-substituted concrete `fn(..) -> ..` the value takes on.
    Pinned(HashMap<String, Ty>, Ty),
    /// A type param is unbound, or bound to a still-free `Ty::Param`. Nothing about this position
    /// can determine it, so the read cannot become a function value — the rule reports.
    Undetermined,
    /// Not this rule's business, and never an error here: the slot is not a matching-arity `fn(..)`
    /// (an arity / shape diagnostic owns that and says it accurately), or the pin came out
    /// `Unknown`-cored (`[].map(ident)`'s `fn(?) -> ?` — the empty-collection sentinel, which runs
    /// fine and prints `[]`).
    Skip,
}

/// The REPORT half's carve-out, asked by the two callers that can EMIT the diagnostic — never by the
/// pin. Each asks it of a different type, because they are different questions:
///
/// * an expected-type HINT (`infer_ident`'s Scope A) is already as substituted as it will get, so a
///   non-concrete parameter there means the position genuinely cannot answer — including a RIGID
///   outer type param (`fn f[T](): g: fn(T) -> T = ident` is legal and must stay accepted).
/// * a generic METHOD's declared slot still carries the method's own free `[U]`, which is normal.
///   The sentinel to carve out there is a `Ty::Unknown` that was in the slot BEFORE the call's
///   bindings were substituted in — i.e. one the receiver's element type contributed
///   (`[].map(ident)`'s `fn(?) -> U`), not one `recover_return_only_params` degraded a live `[U]`
///   into (`Bx(0).two(ident, ident)`'s `fn(U) -> U`, which nothing determines and Go refuses).
pub(super) fn fn_slot_params_concrete(t: &Ty, rigid: &dyn Fn(&str) -> bool) -> bool {
    matches!(t, Ty::Func { params, .. } if params.iter().all(|p| ty_concrete_but(p, rigid)))
}

/// See [`fn_slot_params_concrete`] — the declared-slot half: does a PARAMETER position of `t` hold
/// the empty-collection `Unknown` sentinel? Asked of the UNSUBSTITUTED slot, so a live `[U]` that a
/// recovery degraded to `?` (`Bx(0).two(ident, ident)`) does not read as the sentinel.
pub(super) fn fn_slot_params_have_unknown(t: &Ty) -> bool {
    matches!(t, Ty::Func { params, .. }
        if params.iter().any(|p| p.is_unknown() || p.has_hole()))
}

/// THE derivation behind the uninstantiated-generic-function-value rule, asked at every position a
/// generic fn is read as a VALUE: a binding / annotation / return (via `infer_ident`'s expected-type
/// hint), a generic method's argument slot (the interleaved pin in `try_pin_generic_fn_value_arg`),
/// and again when the frame closes (`Checker::close_tyvar_frame`, TICKET-225), which reports the
/// [`FnValuePin::Undetermined`] ones. `declared` is the fn's own signature (type params still free);
/// `want` is the expected type / declared slot with everything known SO FAR substituted in.
///
/// ORDERING: this is a pure question about the bindings it is handed, so *when* it is asked is the
/// caller's responsibility. The argument-position caller must ask it only once the whole call has
/// been inferred — `[1,2,3].fold(0, pick)` pins `pick`'s `T` from the FIRST argument while `pick` is
/// the SECOND, so asking per-argument would refuse a program Go accepts.
///
/// NOTHING about the SLOT gates the pin — only the RESULT does (a non-rigid param left: `Undetermined`; an `Unknown`
/// left, the empty-collection sentinel: `Skip`). A non-concrete slot position is not evidence the read is un-instantiable: `[]
/// .fold(0, add)`'s slot reads `fn(int, ?) -> int` because the receiver is empty, yet argument ZERO
/// pins `T = int` completely and Go's `Fold([]int{}, 0, add)` returns `0`. The carve-out belongs to
/// the REPORT (see [`fn_slot_params_concrete`]), which each reporting caller applies itself.
///
/// The callee's own params are renamed fresh first ([`freshen_params`], the rule
/// `instantiate_method` uses), so a caller's param spelled like one of them stays the caller's. A
/// param left in the result counts as concrete only when `rigid` accepts it
/// ([`Checker::rigid_param`]): a caller's in-scope param pins like a concrete type.
pub(super) fn pin_generic_fn_value(
    type_params: &[TyParam],
    declared: &Ty,
    want: &Ty,
    rigid: &dyn Fn(&str) -> bool,
) -> FnValuePin {
    let (Ty::Func { params: dp, .. }, Ty::Func { params: wp, .. }) = (declared, want) else {
        return FnValuePin::Skip;
    };
    if dp.len() != wp.len() {
        return FnValuePin::Skip;
    }
    let mut clash = Vec::new();
    ty_collect_params(want, None, &mut clash);
    let ren = freshen_params(type_params, &clash);
    let new_name = |n: &str| ren.get(n).cloned().unwrap_or_else(|| n.to_string());
    let ren_map: HashMap<String, Ty> = ren
        .iter()
        .map(|(old, new)| (old.clone(), Ty::Param(new.clone())))
        .collect();
    let renamed = subst(declared, &ren_map);
    let mut map: HashMap<String, Ty> = HashMap::new();
    unify(&renamed, want, &mut map);
    if !type_params
        .iter()
        .all(|tp| map.contains_key(&new_name(&tp.name)))
    {
        return FnValuePin::Undetermined;
    }
    let refined = subst(&renamed, &map);
    if !ty_all_holes(&refined, &|h| match h {
        Ty::Param(n) => rigid(n),
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Func { .. }
        | Ty::BuiltinFn { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
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
        | Ty::Var(_) => true,
    }) {
        return FnValuePin::Undetermined;
    }
    if !ty_concrete_but(&refined, rigid) {
        return FnValuePin::Skip;
    }
    let pinned = type_params
        .iter()
        .filter_map(|tp| {
            map.get(&new_name(&tp.name))
                .map(|t| (tp.name.clone(), t.clone()))
        })
        .collect();
    FnValuePin::Pinned(pinned, refined)
}

/// Structural merge for same-statement solving: fill `Ty::Unknown` slots in `a` with the corresponding
/// concrete slot from `shape`, recursing to arbitrary depth (so `list[Option[Box[int]]]` fills in a
/// single merge). A bare `Unknown` in `a` becomes `shape` (when `shape` is concrete). For matching
/// compounds (List/Set/Option/Channel/Shared/Atomic ×1, Map/Result ×2, Tuple ×n, Struct/Enum by
/// NAME + arity) it recurses pairwise. On a shape-NAME or arity mismatch (e.g. pushing a different
/// generic enum) it leaves `a` unchanged — no fill — so the normal `check_args` mismatch fires.
/// One lift (TICKET-234): an open optional beside a present value is that value's optional, so
/// `Option(Unknown)` beside `int` is `int?`. This is the rule every sibling join shares.
pub(crate) fn merge_unknown(a: &Ty, shape: &Ty) -> Ty {
    use Ty::*;
    if shape.is_unknown() {
        return a.clone();
    }
    if a.is_unknown() {
        return shape.clone();
    }
    match (a, shape) {
        (List(ae), List(se)) => List(Box::new(merge_unknown(ae, se))),
        (Set(ae), Set(se)) => Set(Box::new(merge_unknown(ae, se))),
        (Option(ae), Option(se)) => Option(Box::new(merge_unknown(ae, se))),
        (Channel(ae), Channel(se)) => Channel(Box::new(merge_unknown(ae, se))),
        (Shared(ae), Shared(se)) => Shared(Box::new(merge_unknown(ae, se))),
        (RwShared(ae), RwShared(se)) => RwShared(Box::new(merge_unknown(ae, se))),
        (Atomic(ae), Atomic(se)) => Atomic(Box::new(merge_unknown(ae, se))),
        (Map(ak, av), Map(sk, sv)) => Map(
            Box::new(merge_unknown(ak, sk)),
            Box::new(merge_unknown(av, sv)),
        ),
        (Result(at, ae), Result(st, se)) => Result(
            Box::new(merge_unknown(at, st)),
            Box::new(merge_unknown(ae, se)),
        ),
        (Tuple(ats), Tuple(sts)) if ats.len() == sts.len() => Tuple(
            ats.iter()
                .zip(sts)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Struct(an, aa), Struct(sn, sa)) if an == sn && aa.len() == sa.len() => Struct(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Enum(an, aa), Enum(sn, sa)) if an == sn && aa.len() == sa.len() => Enum(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        // TICKET-234 -- an open optional beside a present value is that value's optional. A
        // carrier value takes the arm above and is never re-wrapped; `Option`-only, because only a
        // `T?` has a wrap that needs no second type.
        (Option(ae), s)
            if ae.is_unknown()
                && !matches!(s, Option(_) | Result(..) | Nil)
                && ty_fully_concrete(s) =>
        {
            Option(Box::new(s.clone()))
        }
        (
            Func {
                params: ap,
                ret: ar,
                labels,
            },
            Func {
                params: sp,
                ret: sr,
                ..
            },
        ) if ap.len() == sp.len() => Func {
            params: ap
                .iter()
                .zip(sp)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
            ret: Box::new(merge_unknown(ar, sr)),
            labels: labels.clone(),
        },
        // Shape/name/arity mismatch: leave `a` unchanged (no fill — the caller's mismatch fires).
        _ => a.clone(),
    }
}

/// Substitute generic type parameters in `ty` using `map` (e.g. `Self ↦ Point`, `T ↦ int`).
/// Unmapped params are left as-is.
pub(super) fn subst(ty: &Ty, map: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(n) => map.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Ty::List(t) => Ty::list(subst(t, map)),
        Ty::Option(t) => Ty::option(subst(t, map)),
        Ty::Result(t, e) => Ty::result_e(subst(t, map), subst(e, map)),
        Ty::Map(k, v) => Ty::map(subst(k, map), subst(v, map)),
        Ty::Set(t) => Ty::set(subst(t, map)),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst(t, map)).collect()),
        Ty::Func {
            params,
            ret,
            labels,
        } => Ty::Func {
            params: params.iter().map(|t| subst(t, map)).collect(),
            ret: Box::new(subst(ret, map)),
            // Preserve surface labels across generic substitution (they name params, not types).
            labels: labels.clone(),
        },
        // Native reserved generic handles must substitute their element param too (mirror `subst`'s
        // List/Map/… arms) so a generic wrapper struct field `ch: Channel[T]` becomes `Channel[int]`
        // after construction — without these the field stayed `Channel[T]` and every use rejected.
        Ty::Channel(t) => Ty::channel(subst(t, map)),
        Ty::Shared(t) => Ty::shared(subst(t, map)),
        Ty::Atomic(t) => Ty::atomic(subst(t, map)),
        Ty::RwShared(t) => Ty::rwshared(subst(t, map)),
        Ty::Struct(n, args) => Ty::Struct(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        Ty::Enum(n, args) => Ty::Enum(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        // A parameterized protocol existential (`Container[int]`) must recurse into its carried args
        // so DECISION-2 method-return recovery is not inert (`c.get(0)` substituting the protocol's
        // param → the carried arg flows through here). A bare `Error` has no args (a no-op).
        Ty::Protocol(n, args) => {
            Ty::Protocol(n.clone(), args.iter().map(|t| subst(t, map)).collect())
        }
        other @ Ty::Int
        | other @ Ty::Float
        | other @ Ty::Bool
        | other @ Ty::Str
        | other @ Ty::Bytes
        | other @ Ty::ByteArray
        | other @ Ty::Nil
        | other @ Ty::BuiltinFn { .. }
        | other @ Ty::AtomicInt
        | other @ Ty::Executor
        | other @ Ty::Socket
        | other @ Ty::Listener
        | other @ Ty::Writer
        | other @ Ty::Reader
        | other @ Ty::Ptr
        | other @ Ty::Width(_)
        | other @ Ty::Module(_)
        | other @ Ty::Unknown
        | other @ Ty::Var(_) => other.clone(),
    }
}

/// [`subst`] lifted to a whole signature — params, return, and the bound args of `type_params` and
/// `where_bounds` (TICKET-202). Every other field (labels, arity, `is_static`, doc) is
/// vocabulary-independent and rides along unchanged.
pub(super) fn subst_sig(sig: &FnSig, map: &HashMap<String, Ty>) -> FnSig {
    FnSig {
        params: sig.params.iter().map(|t| subst(t, map)).collect(),
        ret: subst(&sig.ret, map),
        type_params: subst_ty_params(&sig.type_params, map),
        where_bounds: subst_ty_params(&sig.where_bounds, map),
        ..sig.clone()
    }
}

/// The first of `{base}1`, `{base}2`, … that is not in `taken`.
pub(super) fn fresh_param_name(base: &str, taken: &std::collections::BTreeSet<String>) -> String {
    (1..)
        .map(|n| format!("{base}{n}"))
        .find(|c| !taken.contains(c))
        .expect("an unbounded counter finds a free name")
}

/// The one rename rule for a generic item's own params `tps` meeting names `clash` that are not
/// its own (DEC-197): each param named in `clash` maps to a fresh name (`{name}{n}`), never one in
/// `clash` or `tps`. Params not in `clash` keep their names and are absent from the map. Read by
/// [`instantiate_method`] and [`pin_generic_fn_value`].
pub(super) fn freshen_params(tps: &[TyParam], clash: &[String]) -> HashMap<String, String> {
    let mut taken: std::collections::BTreeSet<String> = clash.iter().cloned().collect();
    taken.extend(tps.iter().map(|tp| tp.name.clone()));
    let mut ren: HashMap<String, String> = HashMap::new();
    for tp in tps {
        if clash.contains(&tp.name) {
            let new = fresh_param_name(&tp.name, &taken);
            taken.insert(new.clone());
            ren.insert(tp.name.clone(), new);
        }
    }
    ren
}

/// Instantiate a method signature on a receiver whose type args are `recv_map`, with ONE
/// substitution keyed on the declaration's own params. A method type param whose name occurs free
/// in a receiver argument (a generic caller's `U` on `Box[U].pair[U]`) gets a fresh name first, so
/// the caller's param and the method's never collapse into one. With no collision the result equals
/// `subst_sig(sig, recv_map)`. Every generic-method dispatch arm calls this; never substitute the
/// receiver map and then the method map in sequence.
pub(super) fn instantiate_method(sig: &FnSig, recv_map: &HashMap<String, Ty>) -> FnSig {
    let mut free = Vec::new();
    for v in recv_map.values() {
        ty_collect_params(v, None, &mut free);
    }
    let ren = freshen_params(&sig.type_params, &free);
    if ren.is_empty() {
        return subst_sig(sig, recv_map);
    }
    let mut map = recv_map.clone();
    for (old, new) in &ren {
        map.insert(old.clone(), Ty::Param(new.clone()));
    }
    let rename = |n: &String| ren.get(n).cloned().unwrap_or_else(|| n.clone());
    FnSig {
        params: sig.params.iter().map(|t| subst(t, &map)).collect(),
        ret: subst(&sig.ret, &map),
        type_params: subst_ty_params(&sig.type_params, &map)
            .into_iter()
            .map(|tp| TyParam {
                name: rename(&tp.name),
                ..tp
            })
            .collect(),
        where_bounds: subst_ty_params(&sig.where_bounds, &map),
        witness_params: sig.witness_params.iter().map(rename).collect(),
        ..sig.clone()
    }
}

/// Does the type ANNOTATION `t` mention any of `names` anywhere, at any nesting depth? Used to spot
/// an owner type param buried inside an embed's type argument (`Contains[List[T]]`), which the
/// read-only resolver cannot re-spell — see `validate_protocol_embeds`.
pub(crate) fn type_mentions_any(t: &Type, names: &[String]) -> bool {
    match t {
        Type::Nil(_) => false,
        Type::Named { name, .. } => names.contains(name),
        Type::Qualified { args, .. } => args.iter().any(|a| type_mentions_any(a, names)),
        Type::Generic(head, args, _) => {
            names.contains(head) || args.iter().any(|a| type_mentions_any(a, names))
        }
        Type::Func { params, ret, .. } => {
            params.iter().any(|a| type_mentions_any(a, names)) || type_mentions_any(ret, names)
        }
        Type::Tuple(ts) => ts.iter().any(|a| type_mentions_any(a, names)),
    }
}

/// The first bare name in `t` that is neither one of `names` nor accepted by `known` — i.e. a type
/// argument that will silently resolve to `Ty::Unknown`, which is permissive against every operand.
pub(super) fn first_unresolvable_name(
    t: &Type,
    names: &[String],
    known: &dyn Fn(&str) -> bool,
) -> Option<String> {
    match t {
        Type::Nil(_) => None,
        Type::Named { name, .. } => (!names.contains(name) && !known(name)).then(|| name.clone()),
        Type::Qualified { args, .. } | Type::Generic(_, args, _) => args
            .iter()
            .find_map(|a| first_unresolvable_name(a, names, known)),
        Type::Func { params, ret, .. } => params
            .iter()
            .chain(std::iter::once(&**ret))
            .find_map(|a| first_unresolvable_name(a, names, known)),
        Type::Tuple(ts) => ts
            .iter()
            .find_map(|a| first_unresolvable_name(a, names, known)),
    }
}

/// Does `ty` mention `Self` anywhere, at any nesting depth (`Self`, `List[Self]`, `Option[Self]`)?
/// Implemented as a `subst` round-trip rather than a hand-rolled walk so it covers exactly the arms
/// `subst` covers — a hand-rolled twin would drift the first time a `Ty` variant is added. `Unknown`
/// is a safe probe: substituting it into a type that already contains one leaves the type equal.
pub(super) fn mentions_self(ty: &Ty) -> bool {
    let probe = HashMap::from([("Self".to_string(), Ty::Unknown)]);
    subst(ty, &probe) != *ty
}

/// **Object safety** — is this signature un-dispatchable through a protocol EXISTENTIAL?
///
/// True when `Self` appears in a non-receiver PARAMETER position. A protocol value erases which
/// concrete type it holds, so two values of one protocol need not be the same witness: with
/// `fn add(self, other: Self) -> Self`, `a + b` over two `Vecish` values would hand a `W` to `V::add`
/// and fault on the first field access. Rust states the same rule as object safety (a `Self`-typed
/// parameter makes a trait non-`dyn`-able); Go bans `Self` from interfaces outright.
///
/// `Self` in the RETURN is fine — it widens to the existential, which is a legal supertype of
/// whatever the witness returns. That is what keeps unary `-` (`neg(self) -> Self`) usable.
pub(super) fn self_in_param_position(sig: &FnSig) -> bool {
    sig.params.iter().skip(1).any(mentions_self)
}

/// TICKET-187 — the one name-conformance test: the first parameter (1-based, receiver not counted)
/// that both `proto` and `actual` name, under different names, as `(pos, expected, found)`. A call
/// through a protocol binds by the protocol's names, so an implementor that spells a parameter
/// differently would receive the wrong argument (Swift labels are part of the requirement the same
/// way). `None` when either side has no labels (a synthetic sig) or every name agrees.
pub(super) fn param_name_mismatch(
    proto: &FnSig,
    actual: &FnSig,
) -> Option<(usize, String, String)> {
    if proto.labels.is_empty() || actual.labels.is_empty() {
        return None;
    }
    let receiver = usize::from(!proto.is_static);
    proto
        .labels
        .iter()
        .zip(&actual.labels)
        .enumerate()
        .find_map(|(i, pair)| match pair {
            (Some(want), Some(got)) if want != got => {
                Some((i + 1 - receiver, want.clone(), got.clone()))
            }
            _ => None,
        })
}

/// TICKET-197 — the one rendering of a [`param_name_mismatch`] verdict, shared by `satisfies` and
/// every operator and hook error that a misnamed hook method explains.
pub(super) fn name_mismatch_text(
    protocol: &str,
    method: &str,
    (pos, expected, found): (usize, String, String),
) -> String {
    format!(
        "method '{method}' parameter {pos} is named '{found}', but {protocol} declares '{expected}'"
    )
}

impl Checker {
    /// Does a struct method `actual` match a protocol method `proto` (with `Self` bound to `self_ty`)?
    pub(super) fn method_matches(&self, proto: &FnSig, actual: &FnSig, self_ty: &Ty) -> bool {
        if proto.params.len() != actual.params.len() {
            return false;
        }
        // A STATIC-slot protocol requirement (`Convert`'s `convert(x: S) -> Self`, first param NOT `self`)
        // is witnessed ONLY by a matching STATIC method — a value cannot invoke a static ctor, so an
        // instance/`self`-slot method with the same arity (`convert(self) -> Self`) must NOT falsely satisfy
        // it (and vice-versa). Every non-static protocol requirement keeps `is_static == false`, so this is a
        // no-op for every existing instance-method protocol.
        if proto.is_static != actual.is_static {
            return false;
        }
        // M24 Task 5 — a method that takes hidden witness arguments can never WITNESS a protocol
        // requirement. A protocol method has no type parameters of its own (the parser refuses them), so
        // every dispatch through the requirement — an existential value call, `T.static()` through a
        // bound (`Op::CallStaticDyn`) — pushes the declared arity only, and the hidden argument would be
        // missing. Refusing satisfaction here keeps that a type error instead of a runtime arity fault.
        if !actual.witness_params.is_empty() {
            return false;
        }
        // A GENERIC method cannot witness a protocol requirement. Its signature is spelled in terms of
        // its OWN type params, which exist in no scope the requirement can see — so comparing them here
        // compares two independently-scoped binders by their NAME STRING (`ty.rs`'s
        // `(Param(a), Param(b)) => a == b`), and alpha-renaming changes meaning. Measured: `protocol
        // Sink[U]: fn put(self, v: U)` witnessed by `fn put[U](self, v: U)` from a caller
        // `fn use_sink[U, T: Sink[U]](x: T)` is `ok: no type errors`; renaming ONLY the method's own `U`
        // to `W` makes it *type GS does not satisfy Sink (method 'put' has the wrong signature)*.
        // Freshening the binders instead gives the identical verdict — a fresh name never equals the
        // requirement's param — so refusing IS the semantics, just stated honestly.
        //
        // Rust is the owning ancestor and refuses the same shape outright: `trait Show { fn show(&self)
        // -> String; }` with `impl Show for G { fn show<U>(&self) -> String }` is `E0049: method 'show'
        // has 1 type parameter but its trait declaration has 0`. `satisfies_native` has always applied
        // this to builtins ("native method '…' is generic and cannot witness a protocol requirement");
        // this is the same policy for user types, and `recover_bound_args_from` already skips such a
        // method on the recovery side for the same reason.
        if !actual.type_params.is_empty() {
            return false;
        }
        let map = HashMap::from([("Self".to_string(), self_ty.clone())]);
        proto
            .params
            .iter()
            .zip(&actual.params)
            .all(|(p, a)| self.join_ty(&subst(p, &map), a))
            && self.join_ty(&subst(&proto.ret, &map), &actual.ret)
    }
}

/// Bind type parameters in `decl` to the corresponding concrete `actual` types (first binding wins;
/// `Unknown` actuals are ignored so an un-inferable argument doesn't pin a param).
/// The `Ty::Param` vector for a list of type parameters — the type-arg slots of a generic type's
/// declared SHAPE (`Struct(key, [Param(T), …])`). Used to build the expected-type-hint unify target.
pub(super) fn param_shape(tps: &[TyParam]) -> Vec<Ty> {
    tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect()
}

/// Pre-seed a generic ctor/call's type-param substitution `sub` from an expected-type `hint` (a
/// `let`/return/parameter annotation). `shape` is the ctor/call's declared return type in terms of its
/// OWN params (e.g. `Struct(key, [Param(T)])`, or a generic fn's `sig.ret`). Unify is a no-op on a
/// key/shape mismatch and only binds a param still FREE in `sub`, so this runs AFTER arg-unification
/// to give precedence turbofish > args > annotation — it fills only the params the arguments left
/// unbound (the genuine deadlock, e.g. `Heap([], fn(x, y): x < y)` annotated `Heap[int]`).
pub(super) fn seed_from_hint(hint: Option<&Ty>, shape: &Ty, sub: &mut HashMap<String, Ty>) {
    if let Some(e) = hint {
        unify(shape, e, sub);
    }
}

/// Return-mask a closure's actual `Func` type for pass-1 unification: replace its return with
/// `Ty::Unknown` so only its PARAMETER positions can bind a method type param in pass 1. Used on the
/// generic-METHOD path (`infer_generic_method`) so an UNANNOTATED closure whose body is a nested free
/// generic call (`xs.map(fn(x): ident(x))`) — whose prepass return leaks the callee's own `Ty::Param`
/// — does NOT prematurely pin the method's return-position `[U]` to that leaked param. The loop-back's
/// checking-mode re-inference then recovers `U` as the CONCRETE return type (`int`). No `Unknown`
/// laundering: `U` is bound concretely by the loop-back, not degraded. A non-closure actual (or a
/// non-`Func`) is returned unchanged, so param/receiver unification is untouched.
pub(super) fn mask_closure_ret(actual: &Ty) -> Ty {
    match actual {
        Ty::Func { params, labels, .. } => Ty::Func {
            params: params.clone(),
            ret: Box::new(Ty::Unknown),
            labels: labels.clone(),
        },
        other @ Ty::Int
        | other @ Ty::Float
        | other @ Ty::Bool
        | other @ Ty::Str
        | other @ Ty::Bytes
        | other @ Ty::ByteArray
        | other @ Ty::Nil
        | other @ Ty::List(_)
        | other @ Ty::Map(..)
        | other @ Ty::Set(_)
        | other @ Ty::BuiltinFn { .. }
        | other @ Ty::Tuple(_)
        | other @ Ty::Struct(..)
        | other @ Ty::Enum(..)
        | other @ Ty::Param(_)
        | other @ Ty::Result(..)
        | other @ Ty::Option(_)
        | other @ Ty::Channel(_)
        | other @ Ty::Shared(_)
        | other @ Ty::Atomic(_)
        | other @ Ty::AtomicInt
        | other @ Ty::RwShared(_)
        | other @ Ty::Executor
        | other @ Ty::Socket
        | other @ Ty::Listener
        | other @ Ty::Writer
        | other @ Ty::Reader
        | other @ Ty::Ptr
        | other @ Ty::Width(_)
        | other @ Ty::Protocol(..)
        | other @ Ty::Module(_)
        | other @ Ty::Unknown
        | other @ Ty::Var(_) => other.clone(),
    }
}

/// The one text for a `?x` whose operand does not fit the payload of its carrier.
pub(super) fn wrap_payload_mismatch(payload: &Ty, t: &Ty) -> String {
    format!(
        "'?' value: expected {payload}, found {t}{}",
        slot_note(payload, t)
    )
}

pub(super) fn unify(decl: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
    match (decl, actual) {
        (Ty::Param(n), a) => {
            if !a.is_unknown() && !map.contains_key(n) {
                map.insert(n.clone(), a.clone());
            } else if let Some(old) = map.get(n)
                && old.has_hole()
            {
                // TICKET-234 -- a parameter first bound to a type with an open slot is filled by
                // a later argument of the same call instead of keeping the first binding
                // (`pair([], [1])`). Argument loops call `Checker::unify_arg`, which reads a `?x` first.
                let merged = merge_unknown(old, a);
                map.insert(n.clone(), merged);
            }
        }
        (Ty::List(d), Ty::List(a)) | (Ty::Set(d), Ty::Set(a)) | (Ty::Option(d), Ty::Option(a)) => {
            unify(d, a, map)
        }
        // Native reserved generic handles bind their element param exactly like `List[T]` above —
        // without these arms `unify(Shared[T], Shared[int])` fell to the `_` no-op and bound nothing,
        // so a generic fn over `Shared`/`Channel`/`Atomic`/`RwShared` (or a wrapper struct holding
        // one) rejected the call. (Sibling `ty_collect_params` already lists all four.)
        (Ty::Channel(d), Ty::Channel(a))
        | (Ty::Shared(d), Ty::Shared(a))
        | (Ty::Atomic(d), Ty::Atomic(a))
        | (Ty::RwShared(d), Ty::RwShared(a)) => unify(d, a, map),
        (Ty::Result(dt, de), Ty::Result(at, ae)) => {
            unify(dt, at, map);
            unify(de, ae, map);
        }
        (Ty::Map(dk, dv), Ty::Map(ak, av)) => {
            unify(dk, ak, map);
            unify(dv, av, map);
        }
        (Ty::Struct(dn, da), Ty::Struct(an, aa))
        | (Ty::Enum(dn, da), Ty::Enum(an, aa))
        | (Ty::Protocol(dn, da), Ty::Protocol(an, aa))
            if dn == an && da.len() == aa.len() =>
        {
            da.iter().zip(aa).for_each(|(d, a)| unify(d, a, map));
        }
        (Ty::Tuple(ds), Ty::Tuple(as_)) if ds.len() == as_.len() => {
            ds.iter().zip(as_).for_each(|(d, a)| unify(d, a, map));
        }
        // Labels are surface-only: unify on params + ret only, ignoring labels (`..`). The actual
        // side may be a builtin fn value (`xs.map(ord)`): `fn_parts` reads both function variants.
        (
            Ty::Func {
                params: dp,
                ret: dr,
                ..
            },
            a,
        ) => {
            if let Some((ap, ar)) = a.fn_parts()
                && dp.len() == ap.len()
            {
                dp.iter().zip(ap).for_each(|(d, a)| unify(d, a, map));
                unify(dr, ar, map);
            }
        }
        _ => {}
    }
}

/// Collect (into `out`, dedup, in first-seen order) the names from `wanted` that appear as a
/// `Ty::Param` anywhere inside `ty`. Used by the un-inferable-closure-param deadlock diagnostic to
/// find which still-unbound type parameters a closure-typed slot mentions. `wanted: None` = collect
/// EVERY param name, whatever its scope — what [`Checker::may_be_equal`]'s erasure escape needs (it
/// has no candidate list; it must find any free param at all).
pub(super) fn ty_collect_params(
    ty: &Ty,
    wanted: Option<&std::collections::HashSet<String>>,
    out: &mut Vec<String>,
) {
    match ty {
        Ty::Param(n) => {
            if wanted.is_none_or(|w| w.contains(n)) && !out.contains(n) {
                out.push(n.clone());
            }
        }
        Ty::List(e)
        | Ty::Set(e)
        | Ty::Option(e)
        | Ty::Channel(e)
        | Ty::Shared(e)
        | Ty::Atomic(e)
        | Ty::RwShared(e) => ty_collect_params(e, wanted, out),
        Ty::Map(a, b) | Ty::Result(a, b) => {
            ty_collect_params(a, wanted, out);
            ty_collect_params(b, wanted, out);
        }
        Ty::Tuple(parts) | Ty::Struct(_, parts) | Ty::Enum(_, parts) | Ty::Protocol(_, parts) => {
            for p in parts {
                ty_collect_params(p, wanted, out);
            }
        }
        Ty::Func { params, ret, .. } => {
            for p in params {
                ty_collect_params(p, wanted, out);
            }
            ty_collect_params(ret, wanted, out);
        }
        Ty::Int
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::BuiltinFn { .. }
        | Ty::AtomicInt
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => {}
    }
}

/// The type a literal match-pattern matches against.
pub(super) fn lit_pattern_ty(lit: &LitPattern) -> Ty {
    match lit {
        LitPattern::Int(_) => Ty::Int,
        LitPattern::Str(_) => Ty::Str,
        LitPattern::Bool(_) => Ty::Bool,
    }
}

/// Result type of a numeric binary op: float if either side is float, else int.
pub(super) fn numeric_result(l: &Ty, r: &Ty) -> Ty {
    if *l == Ty::Float || *r == Ty::Float {
        Ty::Float
    } else {
        Ty::Int
    }
}

/// The reserved protocol an operator asks. The one operator-to-protocol map; `None` for the
/// short-circuit `and` / `or`. Exhaustive: a new `BinaryOp` fails the build here.
pub(crate) fn binary_protocol(op: BinaryOp) -> Option<&'static str> {
    use BinaryOp::*;
    match op {
        Add => Some("Add"),
        Sub => Some("Sub"),
        Mul => Some("Mul"),
        Div => Some("Div"),
        Mod => Some("Mod"),
        Lt | LtEq | Gt | GtEq => Some("Comparable"),
        Eq | NotEq => Some("Eq"),
        In => Some("Contains"),
        BitAnd => Some("BitAnd"),
        BitOr => Some("BitOr"),
        BitXor => Some("BitXor"),
        Shl => Some("Shl"),
        Shr => Some("Shr"),
        And | Or => None,
    }
}

pub(super) fn op_sym(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        In => "in",
        Lt | LtEq | Gt | GtEq | Eq | NotEq | And | Or => "?",
    }
}
