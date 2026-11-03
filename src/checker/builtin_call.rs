//! Checker: builtin (prelude) calls.
//! Split out of `src/checker/expr.rs`. `super::*` is the parent module.

use super::expr::single_targ;
use super::*;
use crate::base::builtin_method::NativeStatic;

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
            // A call on a builtin type's name makes only an empty value (`List[int]()`, `List()`
            // typed by the expected type on its own statement); a value built from an iterable is
            // `List.from_iter(xs)`, the decl's static method.
            Builtin::List => {
                let elem = match targs {
                    [] => Ty::Unknown,
                    [t] => t.clone(),
                    _ => {
                        self.error(span, "List[T]() takes exactly one type argument");
                        Ty::Unknown
                    }
                };
                self.ctor_takes_no_argument(b, targs, args, span);
                Some(Ty::list(elem))
            }
            Builtin::Set => {
                let elem = match targs {
                    [] => Ty::Unknown,
                    [t] => t.clone(),
                    _ => {
                        self.error(span, "Set[T]() takes exactly one type argument");
                        Ty::Unknown
                    }
                };
                self.ctor_takes_no_argument(b, targs, args, span);
                if !elem.is_unknown()
                    && let Some(why) = self.key_ty_reject(&elem)
                {
                    self.error(span, format!("Set element type {why}"));
                }
                Some(Ty::set(elem))
            }
            Builtin::Map => {
                let (k, v) = match targs {
                    [] => (Ty::Unknown, Ty::Unknown),
                    [k, v] => (k.clone(), v.clone()),
                    _ => {
                        self.error(span, "Map[K, V]() takes exactly two type arguments");
                        (Ty::Unknown, Ty::Unknown)
                    }
                };
                self.ctor_takes_no_argument(b, targs, args, span);
                if !k.is_unknown()
                    && let Some(why) = self.key_ty_reject(&k)
                {
                    self.error(span, format!("Map key type {why}"));
                }
                Some(Ty::map(k, v))
            }
            // `bytearray()` — the MUTABLE byte buffer, empty.
            Builtin::ByteArray => {
                self.ctor_takes_no_argument(b, targs, args, span);
                Some(Ty::ByteArray)
            }
            // `bytes` — the IMMUTABLE form: the `b"..."` literal, `bytes.from_iter(xs)` or
            // `bytes.zeroed(n)`.
            Builtin::Bytes => {
                if args.is_empty() {
                    self.error(
                        span,
                        "bytes() needs a source: build one with `bytes.from_iter(xs)` or `bytes.zeroed(n)`",
                    );
                }
                self.ctor_takes_no_argument(b, targs, args, span);
                Some(Ty::Bytes)
            }
            Builtin::Channel => {
                // `Channel[T]()` — an unbounded mailbox; `Channel[T](cap)` — a bounded FIFO whose
                // `send` blocks when `cap` messages are queued. The element type comes from the explicit
                // type argument (it can't be inferred); any type (M3.2: a send moves the handle). The optional capacity is a
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

    /// `T(x)`, a call on a builtin type's name with an argument: the call makes only an empty
    /// value, so the help names the static method that builds one (`T.from_iter(xs)`, or
    /// `T.zeroed(n)` for an int size of `bytes`/`bytearray`).
    fn ctor_takes_no_argument(&mut self, b: Builtin, targs: &[Ty], args: &[Expr], span: Span) {
        let Some(first) = args.first() else {
            return;
        };
        let size = matches!(self.infer_value(first).scalar(), Ty::Int);
        self.infer_all(&args[1..]);
        let name = b.name();
        let head = if targs.is_empty() {
            name.to_string()
        } else {
            format!("{name}[{}]", render_targs(targs))
        };
        let zeroed = format!("{name}.zeroed");
        let help = if size && NativeStatic::from_key(name, "zeroed").is_some() {
            format!("make n zero bytes with `{zeroed}(n)`")
        } else {
            format!("build a {name} from an iterable with `{head}.from_iter(xs)`")
        };
        self.error_help(span, format!("{name}() takes no argument"), Some(help));
    }
}
