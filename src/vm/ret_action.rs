//! VM: `RetAction`, what a frame's return does with its value before the caller sees it, and the
//! hook calls an operator handler makes through it.
//! `super::*` is the parent module.

use super::*;
use crate::base::builtin_method::NativeStatic;
use crate::base::hooks::Hook;

/// The work an operator handler did after its hook returned, now done by [`Vm::do_return`] when
/// the hook's frame returns (plan 2026-11-07, frame-based re-entry). A handler on the dispatch loop
/// pushes the hook's frame and returns, so a wait inside the hook parks the fiber like any call.
/// `do_return` and the fault unwind ([`Vm::drop_ret`]) are its only readers. Every variant is
/// `Copy`.
#[derive(Clone, Copy, Debug)]
pub(super) enum RetAction {
    /// The value is the result (an ordinary call, `a + b`, `x[k]`, `-x`, `iter`).
    Push,
    /// Drop the value (`x[k] = v` through `set_index` leaves nothing).
    Discard,
    /// A `contains` result as the `bool` of `x in obj`.
    ToBool,
    /// An `eq` result as the `bool` of `==`, flipped for `!=`.
    EqBool { negate: bool },
    /// A `compare` result (an int) as the `bool` of an ordering operator.
    CmpOp(Cmp),
    /// A `str` result as `Op::ToStr`'s str.
    Stringify,
    /// A `str` result as `Op::ToStrFmt`'s formatted str. The spec is the caller's `Op::ToStrFmt`,
    /// the op before the caller frame's `ip`; its nested width/precision fields sit below the
    /// value.
    StringifyFmt,
    /// A list HOF's callback returned; its loop runs on from the slots below the frame
    /// ([`Vm::list_hof`]).
    Hof(HofKind),
    /// A `next` hook returned to a `T.from_iter(it)` drain; the source and the drained list sit
    /// below the frame ([`Vm::do_native_static`]).
    Drain(NativeStatic),
    /// A `print` arg's `str` hook returned; the line so far sits below the frame
    /// ([`Vm::do_print_sep`]). `custom`: the call's `sep` and `end` sit above the args.
    Print { custom: bool },
    /// A sort's comparator returned (a `sort_by` fn, or a struct / enum `compare`); its merge
    /// runs on from the slots below the frame ([`Vm::sort_resume`]).
    Sort(SortKind),
    /// A `compare` hook returned to a `min` / `max` (or `min_by` / `max_by`) scan; it runs on
    /// from the slots below the frame ([`Vm::extreme_resume`]).
    MinMax { max: bool },
    /// A box method's closure returned (`Shared.update` / `read`, `RwShared.read` / `write`); the
    /// box and the closure sit below the frame and the task holds the box's guard
    /// ([`Vm::box_frame`]).
    Box(BoxCall),
    /// A generator body frame (pushed by [`Vm::gen_resume`]); the generator's handle sits below
    /// it. Its `yield` ([`Vm::gen_yield`]) or return hands `Some(v)` / `None` to the `GenThen`.
    Gen(GenThen),
}

/// What a generator resume's `Option` result feeds.
#[derive(Clone, Copy, Debug)]
pub(super) enum GenThen {
    /// The caller's stack (`g.next()`, a `for` step, a synchronous [`Vm::generator_next`]).
    Push,
    /// A `T.from_iter(g)` drain ([`Vm::drain_resume`]).
    Drain(NativeStatic),
}

/// An ordering operator.
#[derive(Clone, Copy, Debug)]
pub(super) enum Cmp {
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl Cmp {
    /// Does `ord` satisfy this operator?
    pub(super) fn holds(self, ord: std::cmp::Ordering) -> bool {
        match self {
            Cmp::Lt => ord.is_lt(),
            Cmp::LtEq => ord.is_le(),
            Cmp::Gt => ord.is_gt(),
            Cmp::GtEq => ord.is_ge(),
        }
    }
}

impl Vm {
    /// Call `hook` on `recv` and leave its result, after `ret`, on the operand stack. `flat` (the
    /// handler runs on the dispatch loop) pushes the hook's frame and `do_return` applies `ret`; a
    /// synchronous native caller passes `false` and re-enters. `Ok(false)` when `recv` has no hook.
    pub(super) fn hook_op(
        &mut self,
        flat: bool,
        hook: Hook,
        recv: Value,
        args: &[Value],
        ret: RetAction,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if flat {
            return self.push_hook_frame(hook, recv, args, ret, span);
        }
        let Some(v) = self.reentered(|vm| vm.invoke_hook(hook, recv, args, span))? else {
            return Ok(false);
        };
        self.push(v);
        self.apply_ret(ret, span)?;
        Ok(true)
    }

    /// Apply `ret` to the value on the stack top: a hook's result, `span` its call site.
    pub(super) fn apply_ret(&mut self, ret: RetAction, span: Span) -> Result<(), RuntimeError> {
        match ret {
            RetAction::Push => Ok(()),
            RetAction::Discard => {
                self.pop();
                Ok(())
            }
            RetAction::ToBool => {
                let b = self.hook_bool(self.peek_top(), Hook::Contains, span)?;
                self.set_top(Value::bool(b));
                Ok(())
            }
            RetAction::EqBool { negate } => {
                let b = self.hook_bool(self.peek_top(), Hook::Eq, span)?;
                self.set_top(Value::bool(b ^ negate));
                Ok(())
            }
            RetAction::CmpOp(cmp) => {
                let ord = self.hook_ord(self.peek_top(), span)?;
                self.set_top(Value::bool(cmp.holds(ord)));
                Ok(())
            }
            RetAction::Stringify => {
                // The result stays rooted on the stack while it renders.
                let s = self.stringify(self.peek_top(), span, 0)?;
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.set_top(Value::obj(h));
                Ok(())
            }
            RetAction::StringifyFmt => {
                let program = Arc::clone(&self.program);
                let caller = self.frames.last().expect("a hook returns to its caller");
                let Op::ToStrFmt(spec) = &program.protos[caller.proto].code[caller.ip - 1] else {
                    unreachable!("a StringifyFmt frame is pushed only by Op::ToStrFmt")
                };
                self.render_str_fmt(spec, true, span)
            }
            RetAction::Hof(kind) => {
                let out = self.pop();
                self.hof_resume(kind, Some(out), true, span)
            }
            RetAction::Drain(which) => {
                let out = self.pop();
                self.drain_resume(which, Some(out), span)
            }
            RetAction::Print { custom } => self.print_resume(custom, span),
            RetAction::Sort(kind) => {
                let out = self.pop();
                self.sort_resume(kind, Some(out), true, span)
            }
            RetAction::MinMax { max } => {
                let out = self.pop();
                self.extreme_resume(max, Some(out), true, span)
            }
            RetAction::Box(call) => {
                let out = self.pop();
                let v = self.box_done(call, Ok(out))?;
                self.push(v);
                Ok(())
            }
            RetAction::Gen(then) => self.gen_return(then, span),
        }
    }

    /// A frame based at `base` left without its return (a fault, a cancel or a halt unwinds it):
    /// give back what its `ret` holds. Its slots below the frame go with the caller's stack.
    pub(super) fn drop_ret(&mut self, ret: RetAction, base: usize) {
        match ret {
            RetAction::Box(call) => self.box_release(call),
            RetAction::Gen(_) => self.gen_drop(base),
            RetAction::Push
            | RetAction::Discard
            | RetAction::ToBool
            | RetAction::EqBool { .. }
            | RetAction::CmpOp(_)
            | RetAction::Stringify
            | RetAction::StringifyFmt
            | RetAction::Hof(_)
            | RetAction::Drain(_)
            | RetAction::Print { .. }
            | RetAction::Sort(_)
            | RetAction::MinMax { .. } => {}
        }
    }

    /// A `bool` hook's result (`eq`, `contains`), or the fault for a non-bool.
    pub(super) fn hook_bool(
        &self,
        res: Value,
        hook: Hook,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        res.as_bool().ok_or_else(|| {
            self.err(
                format!(
                    "{}() must return bool, got {}",
                    hook.method(),
                    self.type_name(res)
                ),
                span,
            )
        })
    }

    /// A `compare` hook's result as an `Ordering`, or the fault for a non-int.
    pub(super) fn hook_ord(
        &self,
        res: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        match self.int_val(res) {
            Some(n) => Ok(n.cmp(&0)),
            None => Err(self.err(
                format!("compare() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
    }

    fn peek_top(&self) -> Value {
        *self.stack.last().expect("a hook result is on the stack")
    }

    fn set_top(&mut self, v: Value) {
        *self
            .stack
            .last_mut()
            .expect("a hook result is on the stack") = v;
    }
}
