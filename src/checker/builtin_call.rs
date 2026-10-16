//! Checker: builtin (prelude) calls.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::expr::single_targ;
use super::*;

impl Checker {
    /// A call of builtin `b` (the rule table answered `Resolution::Builtin`). The
    /// `List`/`Set`/`Map` arms are the ctor-RETURN-TYPE / generic-inference source for the
    /// container ctors (arity/overload check, element-type inference), which no flat `FnSig`
    /// states; `builtin_container_sig` supplies only the flat display/placeholder sig.
    pub(super) fn infer_builtin_call(
        &mut self,
        b: Builtin,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Option<Ty> {
        match b {
            // `print` binds like every declared fn: the prelude decl's slots pack the surplus into
            // its variadic slot and fill `sep`/`end`, and the bound args check against its params.
            Builtin::Print => {
                let sig = self.builtin_sig("print")?;
                let Some(bound) = self.bind_call(sig.slots.as_deref(), "print", args, 0, span)
                else {
                    return Some(Ty::Nil);
                };
                self.check_args_range("print", &sig.params, sig.min_params, &bound, span);
                Some(Ty::Nil)
            }
            // `panic(msg)` raises the same recoverable `RuntimeError` the runtime uses for overflow/
            // OOB/decode faults (caught by the nearest `recover:` as `Err`, else aborts the program).
            // It never returns, so it is bottom-typed (`Ty::Unknown`): in value position it absorbs
            // into the other branch's concrete type via `unify_branch`, and in tail position
            // `flow::stmt` (via `call_diverges` on this Resolution) treats it as a divergence.
            Builtin::Panic => {
                self.check_arity("panic", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other @ Ty::Int
                        | other @ Ty::Float
                        | other @ Ty::Bool
                        | other @ Ty::Bytes
                        | other @ Ty::ByteArray
                        | other @ Ty::Nil
                        | other @ Ty::List(_)
                        | other @ Ty::Map(..)
                        | other @ Ty::Set(_)
                        | other @ Ty::Func { .. }
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
                        | other @ Ty::Var(_) => {
                            self.error(a.span, format!("panic() expects a str, got {other}"))
                        }
                    }
                }
                Some(Ty::Unknown)
            }
            Builtin::Int => {
                self.check_arity("int", 1, args, span);
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("int", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Int)
            }
            Builtin::Float => {
                self.check_arity("float", 1, args, span);
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("float", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Float)
            }
            Builtin::Bool => {
                self.check_arity("bool", 1, args, span);
                // `bool(x)` is a total truthiness cast over the scalars (int/float/bool/str) —
                // like `str`, it accepts any SCALAR. But an AGGREGATE arg
                // is outside the domain and faults at runtime — reject it at check.
                if let Some(a) = args.first() {
                    let aty = self.infer_value(a);
                    self.reject_non_scalar_cast("bool", &aty, a.span);
                }
                self.infer_all(args.get(1..).unwrap_or(&[]));
                Some(Ty::Bool)
            }
            Builtin::Str => {
                self.check_arity("str", 1, args, span);
                // `str` is the Stringable display cast (accepts anything).
                self.infer_all(args);
                Some(Ty::Str)
            }
            Builtin::Ord => {
                self.check_arity("ord", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Str | Ty::Unknown => {}
                        other @ Ty::Int
                        | other @ Ty::Float
                        | other @ Ty::Bool
                        | other @ Ty::Bytes
                        | other @ Ty::ByteArray
                        | other @ Ty::Nil
                        | other @ Ty::List(_)
                        | other @ Ty::Map(..)
                        | other @ Ty::Set(_)
                        | other @ Ty::Func { .. }
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
                        | other @ Ty::Var(_) => {
                            self.error(a.span, format!("ord() expects a str, got {other}"))
                        }
                    }
                }
                Some(Ty::Int)
            }
            Builtin::Chr => {
                self.check_arity("chr", 1, args, span);
                if let Some(a) = args.first() {
                    match self.infer_value(a) {
                        Ty::Int | Ty::Unknown => {}
                        other @ Ty::Float
                        | other @ Ty::Bool
                        | other @ Ty::Str
                        | other @ Ty::Bytes
                        | other @ Ty::ByteArray
                        | other @ Ty::Nil
                        | other @ Ty::List(_)
                        | other @ Ty::Map(..)
                        | other @ Ty::Set(_)
                        | other @ Ty::Func { .. }
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
                        | other @ Ty::Var(_) => {
                            self.error(a.span, format!("chr() expects an int, got {other}"))
                        }
                    }
                }
                Some(Ty::Str)
            }
            // `list(it)` → a list from ANY iterable VALUE (list/set/str/bytes/bytearray/map-keys/
            // Iterator, a `Range` value). The element type flows through `iter_elem`; `List(0..3)` is
            // `[0, 1, 2]` and `List(5..)` runs until memory ends, as `List(g())` does for an endless
            // generator (TICKET-253). The argument is REQUIRED: an empty list is the `[]` literal
            // (zero args can't infer T).
            Builtin::List => {
                // `List[T]()` — explicit element type (turbofish), bare `List()` — empty list whose
                // element type comes from the expected type on its own statement (mirrors `Set()`), and
                // `List(it)` builds from any for-iterable. With a turbofish AND an iterable, the
                // iterable's elements are checked against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "List[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                match args.len() {
                    0 => Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown))),
                    1 => {
                        let it = self.infer_conv_arg(&args[0], targ_elem.clone());
                        let elem = match self.iterable_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!(
                                        "List() expects an iterable, got {it}{}",
                                        self.iter_hook_note(&it)
                                    ),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    let note = self.protocol_note(&t, &elem);
                                    let [t_s, elem_s] = Ty::render_distinct([&t, &elem]);
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "List[{t_s}]() expected elements of type {t_s}, found {elem_s}{note}"
                                        ),
                                    );
                                }
                                Some(Ty::list(t))
                            }
                            None => Some(Ty::list(elem)),
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "List() takes at most one iterable argument — use [] for an empty list",
                        );
                        Some(Ty::list(targ_elem.unwrap_or(Ty::Unknown)))
                    }
                }
            }
            Builtin::Set => {
                // `Set()`/`Set[T]()` → empty set (element from the turbofish, else from the expected
                // type on its own statement, like `{}` for maps); `Set(it)` → a set from ANY iterable VALUE
                // (broadened from list-only), deduped. The element type flows through `iter_elem`;
                // it must be Hashable. A bare RANGE is not a value, so `Set(0..3)` is rejected —
                // use `Set(range(0, 3))`. With a turbofish AND an iterable, elements check against `T`.
                let targ_elem = match targs {
                    [] => None,
                    [t] => Some(t.clone()),
                    _ => {
                        self.error(span, "Set[T]() takes exactly one type argument");
                        Some(Ty::Unknown)
                    }
                };
                let elem = match args.len() {
                    0 => targ_elem.unwrap_or(Ty::Unknown),
                    1 => {
                        let it = self.infer_conv_arg(&args[0], targ_elem.clone());
                        let elem = match self.iterable_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Set() expects an iterable, got {it}{}",
                                        self.iter_hook_note(&it)
                                    ),
                                );
                                Ty::Unknown
                            }
                        };
                        match targ_elem {
                            Some(t) => {
                                if !t.is_unknown()
                                    && !elem.is_unknown()
                                    && !self.assignable(&t, &elem)
                                {
                                    let note = self.protocol_note(&t, &elem);
                                    let [t_s, elem_s] = Ty::render_distinct([&t, &elem]);
                                    self.error(
                                        args[0].span,
                                        format!(
                                            "Set[{t_s}]() expected elements of type {t_s}, found {elem_s}{note}"
                                        ),
                                    );
                                }
                                t
                            }
                            None => elem,
                        }
                    }
                    _ => {
                        self.error(span, "Set() expects Set() or Set(iterable)");
                        targ_elem.unwrap_or(Ty::Unknown)
                    }
                };
                if !elem.is_unknown()
                    && let Some(why) = self.key_ty_reject(&elem)
                {
                    self.error(span, format!("Set element type {why}"));
                }
                Some(Ty::set(elem))
            }
            // `map(it)` → a map from an iterable of EXACTLY 2-tuples `(K, V)`. A non-2-tuple element is
            // a STATIC error here (not a runtime surprise). K must be Hashable. Last-wins on duplicate
            // keys (like the `{k: v}` literal). The argument is REQUIRED: an empty map is the `{}`
            // literal. (Free-call `map(it)` is a distinct namespace from the `xs.map(f)` list HOF.)
            Builtin::Map => {
                // `Map[K, V]()` → typed empty map (turbofish); bare `Map()` → empty map typed by the
                // expected type on its own statement (mirrors the `{}` literal and `Set()`); `Map(it)` →
                // a map from an iterable of EXACTLY 2-tuples. With a turbofish AND an iterable, the
                // tuple parts are checked against `[K, V]`.
                let targ_kv = match targs {
                    [] => None,
                    [k, v] => Some((k.clone(), v.clone())),
                    _ => {
                        self.error(span, "Map[K, V]() takes exactly two type arguments");
                        Some((Ty::Unknown, Ty::Unknown))
                    }
                };
                let (k, v) = match args.len() {
                    0 => targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown)),
                    1 => {
                        let it = self.infer_conv_arg(
                            &args[0],
                            targ_kv.clone().map(|(k, v)| Ty::Tuple(vec![k, v])),
                        );
                        let elem = match self.iterable_elem(&it) {
                            Some(e) => e,
                            None if it.is_unknown() => Ty::Unknown,
                            None => {
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map() expects an iterable, got {it}{}",
                                        self.iter_hook_note(&it)
                                    ),
                                );
                                Ty::Unknown
                            }
                        };
                        let (k, v) = match elem {
                            Ty::Tuple(ref parts) if parts.len() == 2 => {
                                (parts[0].clone(), parts[1].clone())
                            }
                            Ty::Unknown => (Ty::Unknown, Ty::Unknown),
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
                            | other @ Ty::Func { .. }
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
                            | other @ Ty::Var(_) => {
                                self.error(
                                    args[0].span,
                                    format!("Map() expects an iterable of (key, value) 2-tuples, found element {other}"),
                                );
                                (Ty::Unknown, Ty::Unknown)
                            }
                        };
                        if let Some((tk, tv)) = &targ_kv {
                            if !tk.is_unknown() && !k.is_unknown() && !self.assignable(tk, &k) {
                                let note = self.protocol_note(tk, &k);
                                let [tk_s, tv_s, k_s] = Ty::render_distinct([tk, tv, &k]);
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk_s}, {tv_s}]() expected keys of type {tk_s}, found {k_s}{note}"
                                    ),
                                );
                            }
                            if !tv.is_unknown() && !v.is_unknown() && !self.assignable(tv, &v) {
                                let note = self.protocol_note(tv, &v);
                                let [tk_s, tv_s, v_s] = Ty::render_distinct([tk, tv, &v]);
                                self.error(
                                    args[0].span,
                                    format!(
                                        "Map[{tk_s}, {tv_s}]() expected values of type {tv_s}, found {v_s}{note}"
                                    ),
                                );
                            }
                            (tk.clone(), tv.clone())
                        } else {
                            (k, v)
                        }
                    }
                    _ => {
                        self.error(
                            span,
                            "Map() takes at most one iterable argument — use {} for an empty map",
                        );
                        targ_kv.clone().unwrap_or((Ty::Unknown, Ty::Unknown))
                    }
                };
                if !k.is_unknown()
                    && let Some(why) = self.key_ty_reject(&k)
                {
                    self.error(span, format!("Map key type {why}"));
                }
                Some(Ty::map(k, v))
            }
            // `bytearray(...)` — the MUTABLE byte buffer (constructor-only, no literal). Four forms:
            // `bytearray()` (empty), `bytearray(N)` (N zero bytes), `bytearray(b)` (from a `bytes`,
            // mutable copy), `bytearray([ints])` (from a `list[int]`, each 0–255 validated at runtime),
            // and `bytearray(ba)` (copy). Always infers `bytearray`.
            Builtin::ByteArray => {
                match args.len() {
                    0 => {}
                    1 => match self.infer_value(&args[0]) {
                        Ty::Int | Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(elem.scalar(), Ty::Int | Ty::Unknown) => {}
                        other @ Ty::Float | other @ Ty::Bool | other @ Ty::Str | other @ Ty::Nil | other @ Ty::List(_) | other @ Ty::Map(..) | other @ Ty::Set(_) | other @ Ty::Func { .. } | other @ Ty::Tuple(_) | other @ Ty::Struct(..) | other @ Ty::Enum(..) | other @ Ty::Param(_) | other @ Ty::Result(..) | other @ Ty::Option(_) | other @ Ty::Channel(_) | other @ Ty::Shared(_) | other @ Ty::Atomic(_) | other @ Ty::AtomicInt | other @ Ty::RwShared(_) | other @ Ty::Executor | other @ Ty::Socket | other @ Ty::Listener | other @ Ty::Writer | other @ Ty::Reader | other @ Ty::Ptr | other @ Ty::Width(_) | other @ Ty::Protocol(..) | other @ Ty::Module(_) | other @ Ty::Var(_) => self.error(
                            args[0].span,
                            format!("bytearray() expects an int size, a bytes, a bytearray, or a List[int], got {other}"),
                        ),
                    },
                    _ => self.error(span, "bytearray() expects bytearray(), bytearray(int), bytearray(bytes|bytearray), or bytearray(List[int])"),
                }
                Some(Ty::ByteArray)
            }
            // `bytes(...)` — the conversion bridge to the IMMUTABLE form (also constructor-only; the
            // `b"..."` literal is the other way to make a `bytes`). `bytes(ba)` snapshots a `bytearray`,
            // `bytes(b)` copies a `bytes`, `bytes([ints])` builds from a `list[int]`. Infers `bytes`.
            Builtin::Bytes => {
                match args.len() {
                    1 => match self.infer_value(&args[0]) {
                        Ty::Bytes | Ty::ByteArray | Ty::Unknown => {}
                        Ty::List(elem) if matches!(elem.scalar(), Ty::Int | Ty::Unknown) => {}
                        other @ Ty::Int
                        | other @ Ty::Float
                        | other @ Ty::Bool
                        | other @ Ty::Str
                        | other @ Ty::Nil
                        | other @ Ty::List(_)
                        | other @ Ty::Map(..)
                        | other @ Ty::Set(_)
                        | other @ Ty::Func { .. }
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
                        | other @ Ty::Var(_) => self.error(
                            args[0].span,
                            format!(
                                "bytes() expects a bytes, a bytearray, or a List[int], got {other}"
                            ),
                        ),
                    },
                    _ => self.error(
                        span,
                        "bytes() expects bytes(bytes|bytearray) or bytes(List[int])",
                    ),
                }
                Some(Ty::Bytes)
            }
            Builtin::Channel => {
                // `Channel[T]()` — an unbounded mailbox; `Channel[T](cap)` — a bounded FIFO whose
                // `send` blocks when `cap` messages are queued. The element type comes from the explicit
                // type argument (it can't be inferred), and must be sendable. The optional capacity is a
                // runtime int expr (validated `> 0` at runtime, so it can't be arity-checked away here).
                if args.len() > 1 {
                    self.error(span, "Channel[T]() takes an optional capacity argument");
                } else if args.len() == 1 {
                    self.expect_int(&args[0], "Channel capacity");
                }
                let elem = match targs {
                    [t] => t.clone(),
                    // TICKET-142 (W14-33): an expected `Channel[T]` (a let annotation, a call
                    // argument's declared param) supplies the element, as `l: List[float] = []` does.
                    [] if let Some(Ty::Channel(e)) = hint => (**e).clone(),
                    [] => {
                        self.error(span, "Channel() needs an element type — write Channel[T]()");
                        Ty::Unknown
                    }
                    _ => {
                        self.error(span, "Channel[T]() takes exactly one type argument");
                        Ty::Unknown
                    }
                };
                if !elem.is_unknown() && !self.sendable(&elem) {
                    let hint = self.sendable_error_hint(&elem);
                    self.error(
                        span,
                        format!("Channel element type must be sendable (able to cross a task boundary), found {elem}{hint}"),
                    );
                }
                Some(Ty::channel(elem))
            }
            Builtin::Shared => {
                // `Shared(v)` — a fresh cross-task box initialised with `v`. The element type is
                // inferred from the value (value-first, unlike `Channel[T]()`); an OPTIONAL `[T]`
                // turbofish pins it and is checked against the value's type (`Shared[str](0)` rejects).
                // A program reaches it through an import of std.concurrency, like every ctor here
                // but `Channel`.
                let inferred = self.one_arg_hinted("Shared", args, span, single_targ(targs));
                let elem = self.concurrency_turbofish_elem("Shared", targs, inferred, span);
                Some(Ty::shared(elem))
            }
            Builtin::RwShared => {
                // `RwShared(v)` — a fresh cross-task read-write box initialised with `v`. The element
                // type is inferred from the value (value-first, like `Shared`); an OPTIONAL `[T]`
                // turbofish pins it and is checked against the value's type.
                let inferred = self.one_arg_hinted("RwShared", args, span, single_targ(targs));
                let elem = self.concurrency_turbofish_elem("RwShared", targs, inferred, span);
                Some(Ty::rwshared(elem))
            }
            Builtin::Atomic => {
                // `Atomic(v)` — a fresh cross-task atomic box initialised with `v`. Value-first like
                // `Shared`; an OPTIONAL `[T]` turbofish pins the element type and is checked against
                // the value's type.
                let inferred = self.one_arg_hinted("Atomic", args, span, single_targ(targs));
                let elem = self.concurrency_turbofish_elem("Atomic", targs, inferred, span);
                self.conformance_obligation(Obligation::AtomicPayload {
                    elem: elem.clone(),
                    span,
                });
                Some(Ty::atomic(elem))
            }
            Builtin::Executor => {
                // `Executor()` — a fresh, empty, explicitly-owned work queue (C5 escape hatch).
                // Non-generic; zero arguments, or one `int` cap on the jobs running at once; a `[T]`
                // type arg is rejected upstream.
                self.check_args_range("Executor", &[Ty::Int], 0, args, span);
                Some(Ty::Executor)
            }
            Builtin::AtomicInt => {
                // `AtomicInt(v)` — a fresh lock-free int atomic. Monomorphic (no `[T]`); the single arg
                // must be an int.
                self.check_args("AtomicInt", &[Ty::Int], args, span);
                Some(Ty::AtomicInt)
            }
        }
    }
}
