// vm::stmt — split out of vm/mod.rs. `super::*` == the `vm` module.
// Statement exec: try/defer unwind, struct/enum ctor, index/field, print, builtins, display/stringify.

use super::block::Cut;
use super::*;

/// TICKET-246 — one of the three boxes that render what they hold, borrowed from a heap object.
/// [`Vm::display_box`] is its only renderer.
#[derive(Clone, Copy)]
pub(super) enum BoxCore<'a> {
    Shared(&'a Arc<SharedCore>),
    RwShared(&'a Arc<RwSharedCore>),
    Atomic(&'a Arc<AtomicCore>),
}

impl<'a> BoxCore<'a> {
    /// The core's identity: what the `seen` stack is keyed on.
    pub(super) fn addr(self) -> usize {
        match self {
            BoxCore::Shared(c) => Arc::as_ptr(c) as usize,
            BoxCore::RwShared(c) => Arc::as_ptr(c) as usize,
            BoxCore::Atomic(c) => Arc::as_ptr(c) as usize,
        }
    }

    /// What the box holds, read with no wait (M3.2 / M3.3: each box holds one word).
    pub(super) fn peek(self) -> Value {
        match self {
            BoxCore::Shared(c) => c.v.get(),
            BoxCore::RwShared(c) => c.v.get(),
            BoxCore::Atomic(c) => c.load(),
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            BoxCore::Shared(_) => "Shared",
            BoxCore::RwShared(_) => "RwShared",
            BoxCore::Atomic(_) => "Atomic",
        }
    }
}

impl Vm {
    /// THE one read of the halts that cut a `defer` (`RunHalt::cuts_cleanup`: an exit or the
    /// latched verdict), asked before EACH record of frame `fi` above `marker` runs: `Ok(false)`
    /// when no `defer` is left or this party is already exiting; `Err` when another party's exit
    /// is published with a `defer` still pending. The exit is delivered here, so no `recover:`
    /// catches the unwind and no caller runs on. Stops a drain between two `defer`s too (Go:
    /// `os.Exit` does not run deferred calls). The record stays in the frame while it runs, so it
    /// stays GC-rooted; the drain pops it after.
    ///
    /// The length test comes first, so a return with no `defer` takes no mutex. The read is the
    /// exit cell (`quiesce.pending()`), never `run_halt_hint()`: the hint is stored after the cell,
    /// and a party that reads it in that gap would still run its `defer` (TICKET-213, measured at two
    /// workers).
    pub(super) fn has_next_deferred(
        &mut self,
        fi: usize,
        marker: usize,
    ) -> Result<bool, RuntimeError> {
        if self.frames[fi].deferred.len() <= marker || self.pending_exit.is_some() {
            return Ok(false);
        }
        if self.quiesce.run_halt().cuts_cleanup()
            && let Some(e) = self.run_exit_err(Span::RUNTIME)
        {
            return Err(e);
        }
        Ok(true)
    }

    /// TICKET-223 — THE post-drain read of "a hard halt cut this cleanup": this party is exiting
    /// (`has_next_deferred` delivered the exit), the deadlock verdict is latched, or `rte` itself is a
    /// hard halt (`--timeout`, `--max-heap`: [`executor_hard_halt`]) or a deadlock fault
    /// (`is_deadlock`, DEC-135), which a cleanup met while a fault unwound or a recover block
    /// drained (TICKET-289). `Err` carries `rte`, stamped fatal
    /// for a verdict (DEC-135), so no `recover:` catches it; `has_next_deferred` already stops the
    /// remaining `defer`s (DEC-152). `rte` keeps its message, so a delivered cause still outranks
    /// its stuck cleanup in the report (DEC-147). Every route that binds a `recover:`'s `Err`
    /// reads it: `finish_catch` and the recover-scoped `?` catch in `stmt.rs`.
    pub(super) fn cleanup_halt(&self, rte: RuntimeError) -> Result<RuntimeError, RuntimeError> {
        if self.pending_exit.is_some() || executor_hard_halt(&rte) || rte.is_deadlock {
            return Err(rte);
        }
        if self.quiesce.run_halt() == RunHalt::Deadlock {
            return Err(rte.deadlock());
        }
        Ok(rte)
    }

    /// Run frame `fi`'s pending `defer`s down to `marker`, LIFO, on the Rust stack, each asked for
    /// by [`Vm::has_next_deferred`]. Returns the latest fault; an exit ends the drain and is the
    /// result. The dispatch loop's drains run as a trampoline instead ([`Vm::drain_then`]).
    pub(super) fn drain_deferred(&mut self, fi: usize, marker: usize) -> Option<RuntimeError> {
        let mut err = None;
        loop {
            match self.has_next_deferred(fi, marker) {
                Ok(true) => {
                    if let Err(e) = self.run_one_deferred(fi) {
                        err = Some(e);
                    }
                    self.frames[fi].deferred.pop();
                }
                Ok(false) => return err,
                Err(exit) => return Some(exit),
            }
        }
    }

    /// Leave a lexical defer scope (`LeaveDeferScope`): pop the top marker and run the current
    /// frame's defers registered since it, LIFO ([`Vm::drain_then`]); a fault of one is the op's.
    pub(super) fn leave_defer_scope(&mut self) -> Result<(), RuntimeError> {
        let fi = self.frames.len() - 1;
        debug_assert!(
            !self.frames[fi].defer_markers.is_empty(),
            "LeaveDeferScope without a matching EnterDeferScope (compiler scope-count desync)"
        );
        let marker = self.frames[fi].defer_markers.pop().unwrap_or(0);
        self.drain_then(marker, DrainCont::Op)
    }

    /// Unwind frames from the current depth down to `target_frame_len`, running each discarded
    /// frame's deferred calls (innermost first) before dropping it. Used on a fault: deferred
    /// cleanup runs as the stack unwinds, before a `recover:` boundary regains control (or before
    /// the program exits on an uncaught fault). A fault in a deferred call supersedes the original.
    ///
    /// `report_escaped` — a genuine fault or a B3.4 cancel (TICKET-135, W14-39), but not `std.os.exit`, cancels
    /// each discarded frame's escaped nurseries (its implicit nursery + any inner `parallel:` the
    /// fault unwound past) BEFORE that frame's `defer`s run — matching the interp oracle, which
    /// reports in `exec_parallel` / `leave_implicit_nursery` as the body unwinds and only then runs
    /// `finish_frame`'s defers. The MODULE top-level nursery is preserved (it joins only on a clean
    /// run to program end; an uncaught top-level fault leaves it silent, as in the interp).
    pub(super) fn unwind_deferred(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
    ) -> Option<RuntimeError> {
        self.unwind_deferred_escaped(target_frame_len, report_escaped, true)
            .0
    }

    /// TICKET-152 (W14-37) — the FATAL-deadlock unwind: drop every frame down to
    /// `target_frame_len` WITHOUT running its `defer`s. A deadlock verdict is the runtime declaring
    /// that no task can make progress, so a `defer` that itself blocks would run inside a runtime
    /// that has already said so; Go's `fatal error: all goroutines are asleep - deadlock!` runs
    /// none, and parked siblings already run none (DEC-092). `report_escaped` stays `true`: DEC-135
    /// (W14-39) measured that skipping the escaped-nursery abort orphans parked children and hangs
    /// the join at `CHEZZI_THREADS=1`. What the aborted nurseries' children ended with is not
    /// reported: the verdict's fault stands, and its cut tasks are in the run's listing.
    pub(super) fn unwind_no_defer(&mut self, target_frame_len: usize) {
        self.unwind_deferred_escaped(target_frame_len, true, false);
    }

    /// TICKET-147 (W14-12) — the bypass funnels' unwind: run [`Vm::unwind_deferred_escaped`] and rank
    /// what it raised (a `defer` fault, or a fault of a nested nursery it aborted) against `rte`
    /// through [`Vm::unwind_result`].
    pub(super) fn unwind_cancelled(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
        rte: RuntimeError,
    ) -> RuntimeError {
        let (defer, escaped) = self.unwind_deferred_escaped(target_frame_len, report_escaped, true);
        self.unwind_result(rte, defer.or(escaped), report_escaped)
    }

    /// TICKET-195 — THE defer ranking of an unwind that has a cause: `cause` is the fault that
    /// started the unwind, `replaced` the fault its cleanup raised, if any. In order:
    /// 1. a hard-halt cause (`--timeout`, `--max-heap`) keeps the cleanup's fault, whatever the cut
    ///    (the caller re-stamps the marker, so the halt stays a halt);
    /// 2. a [`Cut::Delivered`] party reports the delivered fault — its own cleanup's fault or stuck
    ///    verdict ranks below the root cause — unless that cleanup itself hit a hard halt;
    /// 3. a [`Cut::Cancelled`] party lets a real cleanup fault replace the `cancelled` sentinel and,
    ///    on a cancel funnel (`report_escaped`), latches `cancel_unwind_faulted` so
    ///    `classify_mn_outcome` reports `CancelledFault`; a deadlock-marked one stays swallowed;
    /// 4. an own fault keeps Go's rule: the `defer`'s fault supersedes it.
    ///
    /// A drain with no cause to rank (`Op::DrainHandlerDefers`, `return_levels`,
    /// `leave_defer_scope`) does not call it.
    pub(super) fn unwind_result(
        &mut self,
        cause: RuntimeError,
        replaced: Option<RuntimeError>,
        report_escaped: bool,
    ) -> RuntimeError {
        if cause.is_over_memory || cause.is_timed_out {
            return replaced.unwrap_or(cause);
        }
        match self.cut {
            Some(Cut::Delivered { .. } | Cut::RunFault) => match replaced {
                Some(r) if r.is_over_memory || r.is_timed_out => r,
                _ => cause,
            },
            Some(Cut::Cancelled) => {
                if report_escaped && replaced.as_ref().is_some_and(|e| !e.is_deadlock) {
                    self.cancel_unwind_faulted = true;
                }
                replaced.unwrap_or(cause)
            }
            None => replaced.unwrap_or(cause),
        }
    }

    /// [`Vm::unwind_deferred`] that also reports the first fault an aborted (escaped) nursery's
    /// children ended with, as `(defer_fault, first_escaped_child_fault)`. TICKET-147 (W14-12): a
    /// genuine fault keeps the defer fault as its root cause (`unwind_deferred` drops the second
    /// element); the cancel funnels replace their `cancelled` sentinel with either.
    ///
    /// `run_defers` — `false` ONLY on the fatal-deadlock unwind, TICKET-152: the frames are dropped
    /// where they stand.
    ///
    /// The synchronous unwind: a hard halt's, a fatal deadlock's, and a bypass inside a nested loop.
    /// A fault's or a cancel's unwind runs its `defer`s on the dispatch loop ([`Vm::unwind_on`]);
    /// both report each frame's escaped nurseries above [`Vm::escaped_floor`] before its `defer`s
    /// and drop it with [`Vm::pop_unwound_frame`]. A drain
    /// trampoline met on the way ends its drain ([`Vm::drop_drain_frame`]): the fault its records
    /// raised ranks as a `defer` fault of this unwind.
    pub(super) fn unwind_deferred_escaped(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
        run_defers: bool,
    ) -> (Option<RuntimeError>, Option<RuntimeError>) {
        let mut err = None;
        let mut escaped_err: Option<RuntimeError> = None;
        while self.frames.len() > target_frame_len {
            let fi = self.frames.len() - 1;
            if report_escaped {
                let child = self.drain_escaped_nursery(self.escaped_floor(fi));
                escaped_err = escaped_err.or(child);
            }
            if self.frames[fi].proto == DRAIN_PROTO {
                if let Some(e) = self.drop_drain_frame(fi) {
                    err = Some(e);
                }
            } else if run_defers && let Some(e) = self.drain_deferred(fi, 0) {
                err = Some(e);
            }
            self.pop_unwound_frame(!run_defers);
        }
        (err, escaped_err)
    }

    /// The nurseries an unwind of frame `fi` reports as escaped, before the frame's `defer`s: those
    /// above this count (innermost-first, so inner `parallel:` levels report before the frame's
    /// implicit one — the interp's order). The module nursery is kept.
    pub(super) fn escaped_floor(&self, fi: usize) -> usize {
        let f = &self.frames[fi];
        let floor = if f.is_toplevel && f.has_implicit_nursery {
            f.nursery_len + 1 // preserve the module nursery
        } else {
            f.nursery_len
        };
        floor.min(self.nurseries.len())
    }

    /// Drop the top frame of an unwind, after its `defer`s, with the `recover:` handlers it
    /// installed. `cut`: a deadlock verdict cut this task ([`Vm::drop_ret`]).
    pub(super) fn pop_unwound_frame(&mut self, cut: bool) {
        let frame = self.frames.pop().unwrap();
        self.drop_ret(frame.ret, frame.base, cut);
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        while self
            .handlers
            .last()
            .is_some_and(|h| h.frame_len > self.frames.len())
        {
            self.handlers.pop();
        }
    }

    pub(super) fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant_id, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v.view() {
            ValueView::Obj(h) => self
                .heap
                .get(h)
                .as_enum()
                .map(|(variant_id, payload)| (variant_id, payload.len(), payload.try_get(0))),
            ValueView::Int(_) | ValueView::Bool(_) | ValueView::Nil => None,
        };
        // M19 lever #2 — gate on the fixed native variant ids (`VID_OK`/`VID_SOME` unwrap, `VID_ERR`/
        // `VID_NONE_VARIANT` propagate), NOT a name compare. A user enum shadowing `Ok`/`Err`/`Some`/
        // `None` gets distinct ids, so it is correctly NOT treated as a Result/Option by `?`.
        use crate::base::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
        if let Some((variant_id, n, first)) = info {
            if (variant_id == VID_OK || variant_id == VID_SOME) && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if variant_id == VID_ERR || variant_id == VID_NONE_VARIANT {
                // The propagated value rides the operand stack from here to its delivery: each
                // drain below can collect (a defer runs code; a nursery wait detaches this thread
                // and another thread collects the run's one heap), and the stack is a root, a Rust
                // local is not. `do_return` keeps its return value there for the same reason.
                self.push(v);
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                // The escaped `parallel:` body's defers (ORDERING in `try_escape`), drained before
                // the handler pops so the TICKET-287 park can still rewind this `?`. A re-run finds
                // them consumed and takes back the fault the first run parked with.
                if let Some(h) = self.same_frame_handler()
                    && self.nurseries.len() > h.nursery_len
                {
                    let floor = self.nursery_defer_floors[h.nursery_len];
                    let parked = self.take_escape_park();
                    return self.drain_then(floor, DrainCont::TryBody { span, parked });
                }
                return self.try_escape(span, None);
            }
        }
        Err(self.err(
            format!(
                "'?' expects a `T?` or `T!E` value, found {}",
                self.type_name(v)
            ),
            span,
        ))
    }

    /// The `recover:` handler installed in the top frame, if any.
    fn same_frame_handler(&self) -> Option<Handler> {
        let frame_len = self.frames.len();
        self.handlers
            .last()
            .copied()
            .filter(|h| h.frame_len == frame_len)
    }

    /// The `?` of `do_try` after the escaped `parallel:` body's defers ran (`body_defers`: their
    /// fault). The propagated value is on top of the stack.
    pub(super) fn try_escape(
        &mut self,
        span: Span,
        body_defers: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let v = *self.stack.last().expect("the propagated value");
        let frame_len = self.frames.len();
        let same_frame = self.same_frame_handler();
        // TICKET-287 — a fiber-owned nursery that is not done parks this fiber, as
        // `park_escaped_abort` does for `return`. A body defer fault parks with it.
        if let Some(h) = same_frame
            && let Some(origin) = self.park_abort_origin(h.nursery_len)
        {
            if let Some(e) = body_defers {
                self.store_escape_park(e);
            }
            self.rewind_op();
            self.join_suspend = Some(origin);
            return Ok(());
        }
        if let Some(h) = self.handlers.pop_if(|h| h.frame_len == frame_len) {
            self.stack.truncate(h.stack_len);
            self.push(v); // the recover's value slot, rooted across the drains below
            self.call_depth = h.call_depth;
            // Drop scope markers of defer scopes opened inside the recover block — the `?`
            // jumps past their `LeaveDeferScope`s, so they would otherwise leak.
            self.frames
                .last_mut()
                .unwrap()
                .defer_markers
                .truncate(h.markers_len);
            // TASK B — a recover-scoped `?` jumps past the `JoinNursery` of any `parallel:`
            // opened inside the recover block: cancel its tasks HERE
            // (before the handler binds its result and execution continues), so a recover-caught
            // `?` reports IDENTICALLY-AND-AS-EARLY as an uncaught one — matching the interp,
            // whose `exec_parallel` reports during the `?` unwind, before the recover's value is
            // produced. Without this the nursery lingered until the whole frame returned (the
            // report then trailed `print("recovered")`, an interp/VM divergence).
            //
            // ORDERING (matches the interp oracle): the escaped `parallel:` BODY's own defers
            // must run BEFORE the nursery reclaim, and the recover block's defers AFTER it —
            // because in the interp the body is its own `exec_scoped_block` whose defers drain
            // as the `?` unwinds out of the body, and only then does `exec_parallel` report;
            // the recover block's defers run later, at the recover boundary. So: drain the
            // body defers down to the outermost escaped nursery's floor, report, then drain the
            // remaining (recover-block) defers down to the handler's install-time floor. A body
            // defer fault is held and superseded by any later recover-block defer fault.
            let mut body_defer_err = body_defers;
            if let Some(e) = body_defer_err.take() {
                body_defer_err = Some(self.cleanup_halt(e)?);
            }
            // TICKET-147 — the propagated `Err` is the recover's value; a body defer fault
            // supersedes it, and an aborted nursery's child fault fills that gap.
            let child = self.drain_escaped_nursery(h.nursery_len);
            body_defer_err = body_defer_err.or(child);
            // Drain the recover block's own defers before binding the result (`try_recover`).
            return self.drain_then(
                h.defer_len,
                DrainCont::TryRecover {
                    ip: h.ip,
                    body: body_defer_err,
                },
            );
        }
        // A `?` at the top level (no enclosing function) is an unhandled error → exit. Use
        // the `?` op's own `span` so the reported location matches the interp (which threads
        // the `?`'s `expr.span` through its propagation marker).
        if self.frames.last().unwrap().is_toplevel {
            return Err(self.top_level_error(v, span).unwrap_or_else(|| {
                self.err(format!("unhandled error: {}", self.display(v)), span)
            }));
        }
        // Otherwise early-return this value from the enclosing function (running its
        // deferred calls first; a fault in one propagates as a fault). `v` is on top.
        self.do_return(true)
    }

    /// The recover-scoped `?` after its recover block's defers ran (`drained`: their latest
    /// fault): bind the recover's value and resume at its exit `ip`. A fault in one of those
    /// defers supersedes the propagated value (becomes the recover's `Err`); a recover-block
    /// defer fault in turn supersedes the body defer fault `body` (it unwinds later). A body
    /// defer fault becomes the recover's `Err` when no recover-block defer faulted (Go
    /// semantics — a defer fault supersedes).
    pub(super) fn try_recover(
        &mut self,
        ip: usize,
        body: Option<RuntimeError>,
        drained: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let fault = match drained {
            Some(e) => Some(self.cleanup_halt(e)?),
            None => body,
        };
        // With no fault, the propagated Result/Option value, already in place, IS the result.
        if let Some(e) = fault {
            self.pop(); // superseded
            let sp = e.span;
            let msg = self.alloc_str(e.message);
            self.stamp_err_span(msg, sp);
            let err = self.alloc_enum("Result", "Err", &[msg]);
            self.push(err);
        }
        // A fault either drain raised is handled here; a trace one of them captured in its own
        // loop must not decorate a later uncaught fault.
        self.fault_trace = None;
        self.fault_trace_depth = 0;
        self.jump(ip);
        Ok(())
    }

    // ----- construction / access -----

    /// TICKET-262 — owner rule 2: a value stored into a struct field declared with a `std.ffi` width
    /// must fit it. It reads `CType::check_fits`, the C boundary's own check, so the fault text is the
    /// boundary's: `value 300 does not fit int8 (-128..127)`. Every path that writes a struct field
    /// from a non-struct source calls it: `new_struct`, `set_field` and `coerce_json`.
    pub(super) fn field_fits(&self, tid: u32, idx: usize, val: Value) -> Result<(), String> {
        let Some(ct) = self
            .program
            .struct_widths
            .get(tid as usize)
            .and_then(|w| w.get(idx))
            .and_then(Option::as_ref)
        else {
            return Ok(());
        };
        let v = if val.is_float() {
            crate::native::NativeRet::Float(self.float_of(val))
        } else if let Some(n) = self.int_val(val) {
            crate::native::NativeRet::Int(n)
        } else {
            return Ok(());
        };
        ct.check_fits(&v).map_err(|e| e.message)
    }

    pub(super) fn new_struct(
        &mut self,
        name: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let def = self
            .program
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        if argc != def.fields.len() {
            return Err(self.err(
                format!(
                    "struct '{}' expects {} field(s), got {argc}",
                    def.display_name,
                    def.fields.len()
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        // Positional layout: the args already arrive in declaration order (desugar reorders any
        // named-field constructor before codegen), so split them straight in — no per-field name
        // strings, no zip with `def.fields`. `argc == def.fields.len()` is checked above.
        let fields: Vec<Value> = self.stack.split_off(at);
        // TICKET-253: `Range(a, b, 0)` faults where it is built, however it is built.
        if name == self.program.range_key && fields.get(2).and_then(|s| self.int_val(*s)) == Some(0)
        {
            return Err(self.err("Range() step cannot be zero".to_string(), span));
        }
        for (i, v) in fields.iter().enumerate() {
            self.field_fits(def.tid, i, *v)
                .map_err(|m| self.err(m, span))?;
        }
        let h = self.heap.alloc(Obj::Struct {
            tid: def.tid,
            fields: Fields::from_vec(fields),
        });
        self.push(Value::obj(h));
        Ok(())
    }

    /// The dense layout id for a struct type `name`, or [`TID_NONE`] if it isn't a registered type
    /// (native/ad-hoc structs) — such a struct never IC-caches, so it stays sound on the probe path.
    pub(super) fn struct_tid(&self, name: &str) -> u32 {
        self.program.structs.get(name).map_or(TID_NONE, |d| d.tid)
    }

    /// M19 memory-layout lever — resolve a struct `tid` back to its IDENTITY KEY (the `structs` map
    /// key, e.g. `<main>::Point`) on the cold path (method dispatch / Display / arith / hash), where `Obj::Struct` no longer carries a per-instance `name`. O(1) via `struct_names`.
    /// Returns `"?"` for [`crate::base::op::TID_NONE`] / an out-of-range id (defensive — every
    /// source-constructed struct has a registered `def.tid`, so this is unreachable for Display).
    pub(super) fn struct_name_of_tid(&self, tid: u32) -> &str {
        self.program
            .struct_names
            .get(tid as usize)
            .map_or("?", |s| s)
    }

    /// M19 lever #2 — resolve a `variant_id` back to its `(enum-type, variant)` names on the COLD path
    /// (Display / stringify / error), where the instance no longer carries the strings.
    /// O(1) via `Program::variants_by_id`. Returns `("?", "?")` for [`crate::base::op::VID_NONE`] / an
    /// out-of-range id (defensive — a registered enum always resolves).
    pub(super) fn enum_names(&self, variant_id: u32) -> (&str, &str) {
        self.program
            .variants_by_id
            .get(variant_id as usize)
            .map_or(("?", "?"), |d| (d.enum_name.as_str(), d.name.as_str()))
    }

    /// How a fault names a carrier value: one of the two words above, never `enum`.
    pub(super) fn carrier_word(variant_id: u32) -> Option<&'static str> {
        match variant_id {
            crate::base::op::VID_SOME | crate::base::op::VID_NONE_VARIANT => {
                Some(OPTION_VALUE_WORD)
            }
            crate::base::op::VID_OK | crate::base::op::VID_ERR => Some(RESULT_VALUE_WORD),
            _ => None,
        }
    }

    /// The subject of a missing-method fault on an enum value: a carrier by [`Self::carrier_word`],
    /// a user enum by `plain` over its display name.
    pub(super) fn enum_subject(variant_id: u32, key: &str, plain: fn(&str) -> String) -> String {
        Self::carrier_word(variant_id).map_or_else(
            || plain(&crate::base::words::bare_display(key)),
            str::to_string,
        )
    }

    /// The display rule of a carrier (TICKET-228): what a `T?` / `T!E` value prints in front of
    /// its payload, the way the user writes it. An error is `!e`; a success or a present value is
    /// its bare payload; `?` prints only in front of an absent payload (`?None`), so it stays
    /// distinct from `None`. Rust `None` for a non-carrier and for the absent value itself, which
    /// print by their variant name. `?` prints when the payload is absent or is itself a
    /// `?`-prefixed present value.
    pub(super) fn carrier_prefix(
        &self,
        variant_id: u32,
        payload: &RVec<Value>,
    ) -> Option<&'static str> {
        let absent_payload = || {
            payload
                .try_get(0)
                .and_then(|v| v.as_obj())
                .is_some_and(|h| match self.heap.get(h).as_enum() {
                    Some((inner, inner_payload)) => {
                        inner == crate::base::op::VID_NONE_VARIANT
                            || (inner == crate::base::op::VID_SOME
                                && self.carrier_prefix(inner, inner_payload) == Some("?"))
                    }
                    None => false,
                })
        };
        match variant_id {
            crate::base::op::VID_ERR => Some("!"),
            crate::base::op::VID_OK => Some(""),
            crate::base::op::VID_SOME => Some(if absent_payload() { "?" } else { "" }),
            _ => None,
        }
    }

    /// The pattern a carrier variant is matched by, for the `no match arm` fault text; Rust
    /// `None` for a user variant.
    pub(crate) fn carrier_pattern_text(variant_id: u32) -> Option<&'static str> {
        use crate::ast::CarrierTag;
        match variant_id {
            crate::base::op::VID_SOME | crate::base::op::VID_OK => {
                Some(CarrierTag::Present.pattern_text())
            }
            crate::base::op::VID_ERR => Some(CarrierTag::Error.pattern_text()),
            crate::base::op::VID_NONE_VARIANT => Some(Self::NONE_TEXT),
            _ => None,
        }
    }

    /// The index of the module that declared the enum keyed by `enum_key` (its method bodies resolve
    /// top-level names against that module's globals). Defaults to module 0 if unrecorded.
    pub(super) fn enum_home_module(&self, enum_key: &str) -> usize {
        self.program.enum_home.get(enum_key).copied().unwrap_or(0)
    }

    /// Construct an enum from `Op::NewEnum`. M19 lever #2 — the dense `variant_id` is baked into the op
    /// at compile time (no runtime hash lookup); it is stamped onto the instance instead of the two
    /// per-instance type/variant `Box<str>`s. `variant` is used only for the arity-mismatch message.
    pub(super) fn new_enum(
        &mut self,
        variant: &str,
        variant_id: u32,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if let Some(def) = self.program.variants_by_id.get(variant_id as usize)
            && argc != def.arity
        {
            return Err(self.err(
                format!(
                    "variant '{variant}' expects {} value(s), got {argc}",
                    def.arity
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        let payload = RVec::from_slice(&self.stack[at..]);
        self.stack.truncate(at);
        let h = self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        });
        self.push(Value::obj(h));
        Ok(())
    }
}
