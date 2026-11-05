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
        // A builtin binds like every declared fn: its prelude decl's slots take named arguments and
        // fill omitted defaults (`print` packs the surplus into its variadic slot and fills
        // `sep`/`end`). A builtin with no decl (`builtin_container_sig`) has no slots, so the binder
        // refuses a named argument. Each arm below checks the bound list.
        let bound = match self.builtin_sig(b.name()) {
            Some(sig) => self.bind_sig(&sig, b.name(), args, 0, span),
            None => self.bind_call(None, b.name(), args, 0, span),
        };
        let Some(bound) = bound else {
            return Some(Ty::Unknown);
        };
        let args: &[Expr] = &bound;
        match b {
            Builtin::Print => {
                let sig = self.builtin_sig("print")?;
                self.check_args_range("print", &sig.params, sig.min_params, args, span);
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
            // A call on a scalar type's name sets no field, so it gives the zero value; a
            // conversion is a method on the source value (`ctor_takes_no_argument` names it).
            Builtin::Int => {
                self.ctor_takes_no_argument(b, &Ty::Int, targs, args, span);
                Some(Ty::Int)
            }
            Builtin::Float => {
                self.ctor_takes_no_argument(b, &Ty::Float, targs, args, span);
                Some(Ty::Float)
            }
            Builtin::Bool => {
                self.ctor_takes_no_argument(b, &Ty::Bool, targs, args, span);
                Some(Ty::Bool)
            }
            Builtin::Str => {
                self.ctor_takes_no_argument(b, &Ty::Str, targs, args, span);
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
                let made = Ty::list(elem);
                self.ctor_takes_no_argument(b, &made, targs, args, span);
                Some(made)
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
                self.ctor_takes_no_argument(b, &Ty::set(elem.clone()), targs, args, span);
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
                self.ctor_takes_no_argument(b, &Ty::map(k.clone(), v.clone()), targs, args, span);
                if !k.is_unknown()
                    && let Some(why) = self.key_ty_reject(&k)
                {
                    self.error(span, format!("Map key type {why}"));
                }
                Some(Ty::map(k, v))
            }
            // `bytearray()` — the MUTABLE byte buffer, empty.
            Builtin::ByteArray => {
                self.ctor_takes_no_argument(b, &Ty::ByteArray, targs, args, span);
                Some(Ty::ByteArray)
            }
            // `bytes()` — the IMMUTABLE form, empty; a value is a `b"..."` literal,
            // `bytes.from_iter(xs)` or `bytes.zeroed(n)`.
            Builtin::Bytes => {
                self.ctor_takes_no_argument(b, &Ty::Bytes, targs, args, span);
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

    /// `T(x)`, a call on a builtin type's name with an argument: the call sets no field, so it makes
    /// only the empty or zero value `made`, and the help names the form that builds one from `x`.
    fn ctor_takes_no_argument(
        &mut self,
        b: Builtin,
        made: &Ty,
        targs: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        let Some(first) = args.first() else {
            return;
        };
        let aty = self.infer_value(first);
        self.infer_all(&args[1..]);
        let name = b.name();
        let help = self.no_argument_help(b, made, aty.scalar(), targs);
        self.error_help(span, format!("{name}() takes no argument"), help);
    }

    /// The help of `ctor_takes_no_argument`, from the argument's type `aty`:
    /// - a type with a `from_iter` static builds from an iterable (`T.zeroed(n)` for an int size);
    /// - an argument of the type itself needs no call;
    /// - a source type whose decl has a `to_<T>` method converts with it;
    /// - else the display, truth and bool forms, or no conversion.
    fn no_argument_help(&self, b: Builtin, made: &Ty, aty: &Ty, targs: &[Ty]) -> Option<String> {
        let name = b.name();
        if NativeStatic::from_key(name, "from_iter").is_some() {
            if matches!(aty, Ty::Int) && NativeStatic::from_key(name, "zeroed").is_some() {
                return Some(format!("make n zero bytes with `{name}.zeroed(n)`"));
            }
            let head = if targs.is_empty() {
                name.to_string()
            } else {
                format!("{name}[{}]", render_targs(targs))
            };
            return Some(format!(
                "build a {name} from an iterable with `{head}.from_iter(xs)`"
            ));
        }
        if aty.is_unknown() {
            return None;
        }
        if aty == made {
            let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                "an"
            } else {
                "a"
            };
            return Some(format!("the argument is already {article} {name}"));
        }
        let conv = format!("to_{name}");
        if let Some(sig) = super::fn_writes::native_receiver(aty)
            .and_then(|key| self.structs.get(key))
            .and_then(|info| info.methods.get(&conv))
        {
            return Some(format!(
                "convert with `x.{conv}()`, which gives {}",
                sig.ret
            ));
        }
        // The target's static that takes the argument's type (`int.from(x: bool)`), read from its
        // prelude `native struct` decl.
        let numeric = || {
            let info = self.structs.get(name).filter(|_| matches!(aty, Ty::Bool))?;
            let (m, _) = info
                .methods
                .iter()
                .find(|(_, s)| s.is_static && s.params.as_slice() == std::slice::from_ref(aty))?;
            Some(format!("a bool is not a number; write `{name}.{m}(x)`"))
        };
        let help = match b {
            Builtin::Str => Some("render the value with `\"{x}\"`".to_string()),
            Builtin::Bool => self
                .truth_test(aty)
                .map(|t| format!("test the value: write `{t}`")),
            Builtin::Int | Builtin::Float => numeric(),
            Builtin::Print
            | Builtin::Ord
            | Builtin::Chr
            | Builtin::Panic
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
            | Builtin::Executor => None,
        };
        help.or_else(|| Some(format!("{aty} does not convert to {name}")))
    }

    /// The comparison that tests `x` for a `bool(x)` help, read from the argument's type: a number
    /// compares with 0, an optional with None, and a type with a `len` method compares its length.
    fn truth_test(&self, aty: &Ty) -> Option<&'static str> {
        if matches!(aty, Ty::Int | Ty::Float) {
            return Some("x != 0");
        }
        if matches!(aty, Ty::Option(_)) {
            return Some("x != None");
        }
        super::fn_writes::native_receiver(aty)
            .and_then(|key| self.structs.get(key))
            .filter(|info| info.methods.contains_key("len"))
            .map(|_| "x.len() > 0")
    }
}
