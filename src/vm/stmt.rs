// vm::stmt — split out of vm/mod.rs. `super::*` == the `vm` module.
// Statement exec: try/defer unwind, struct/enum ctor, index/field, print, builtins, display/stringify.

use super::block::Cut;
use super::*;

/// TICKET-246 — appended to the D4 fault when the write happens inside a `Shared` / `RwShared`
/// closure.
pub(crate) const BOX_COPY_WRITE_NOTE: &str = " (inside a Shared/RwShared closure this value is a copy of a capture or of a module global; if the box holds a module global's copy, store its .copy() in the box instead)";

/// TICKET-246 — one of the three boxes that render what they hold, borrowed from a heap object
/// or from a wire handle. [`Vm::display_box`] is its only renderer.
#[derive(Clone, Copy)]
pub(super) enum BoxCore<'a> {
    Shared(&'a Arc<SharedCore>),
    RwShared(&'a Arc<RwSharedCore>),
    Atomic(&'a Arc<AtomicCore>),
}

impl<'a> BoxCore<'a> {
    /// The box behind a wire handle; `None` for every other wire value.
    pub(super) fn of_wire(w: &'a WireValue) -> Option<Self> {
        match w {
            WireValue::Shared(c) => Some(BoxCore::Shared(c)),
            WireValue::RwShared(c) => Some(BoxCore::RwShared(c)),
            WireValue::Atomic(c) => Some(BoxCore::Atomic(c)),
            WireValue::Int(_)
            | WireValue::Float(_)
            | WireValue::Bool(_)
            | WireValue::Nil
            | WireValue::List { .. }
            | WireValue::Tuple { .. }
            | WireValue::Map { .. }
            | WireValue::Set { .. }
            | WireValue::Str(_)
            | WireValue::Bytes(_)
            | WireValue::ByteArray(_)
            | WireValue::Iter { .. }
            | WireValue::Struct { .. }
            | WireValue::Enum { .. }
            | WireValue::Cell { .. }
            | WireValue::Backref(_)
            | WireValue::Handle(_)
            | WireValue::Channel(_)
            | WireValue::AtomicInt(_)
            | WireValue::Executor(_)
            | WireValue::Socket(_)
            | WireValue::Listener(_)
            | WireValue::Writer(_)
            | WireValue::Reader(_)
            | WireValue::Ptr(_)
            | WireValue::Builtin(_)
            | WireValue::Native { .. }
            | WireValue::Cffi(_)
            | WireValue::Closure { .. }
            | WireValue::Func { .. }
            | WireValue::Generator { .. } => None,
        }
    }

    pub(super) fn to_wire(self) -> WireValue {
        match self {
            BoxCore::Shared(c) => WireValue::Shared(Arc::clone(c)),
            BoxCore::RwShared(c) => WireValue::RwShared(Arc::clone(c)),
            BoxCore::Atomic(c) => WireValue::Atomic(Arc::clone(c)),
        }
    }

    /// The core's identity: what the `seen` stack and the snapshot map are keyed on.
    pub(super) fn addr(self) -> usize {
        match self {
            BoxCore::Shared(c) => Arc::as_ptr(c) as usize,
            BoxCore::RwShared(c) => Arc::as_ptr(c) as usize,
            BoxCore::Atomic(c) => Arc::as_ptr(c) as usize,
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

/// Box snapshots taken before a render, keyed on [`BoxCore::addr`].
pub(super) type BoxSnaps = fxhash::FxHashMap<usize, WireValue>;

impl Vm {
    /// Build the D4 layer-C fault for a write to an airlock copy. `name` is the binding name when
    /// the write site knows it (a global slot store); every other site passes `None` and gets the
    /// generic `this value` subject.
    pub(super) fn copied_write_err(&self, name: Option<&str>, span: Span) -> RuntimeError {
        let subject = match name {
            Some(n) => format!("'{n}'"),
            None => "this value".to_string(),
        };
        // TICKET-246 — inside a box closure a marked value is a capture's copy or a module
        // global's copy. The second can be held by the box state (`s.update(fn(_): DEFAULTS)`),
        // where "lost at the join" alone would mislead, so the fault names that case and its fix.
        let in_box = if self.box_chain.is_empty() {
            ""
        } else {
            BOX_COPY_WRITE_NOTE
        };
        self.err(format!("{subject} {COPY_WRITE_TAIL}{in_box}"), span)
    }

    /// Drain the current (top) frame's deferred calls, LIFO, popping one at a time from the frame's
    /// own list so the not-yet-run records stay GC-rooted in the frame. Stops once any party's
    /// `std.os.exit` is published, between two `defer`s too (Go: `os.Exit` does not run deferred
    /// calls). Returns the latest fault, if any.
    pub(super) fn drain_top_frame_deferred(&mut self) -> Option<RuntimeError> {
        self.drain_frame_to(0)
    }

    /// THE one pop and the one read of the halts that cut a `defer` (`RunHalt::cuts_cleanup`: an
    /// exit or the latched verdict), asked before EACH pop: `Ok(None)` when no
    /// `defer` is left or this party is already exiting; `Err` when another party's exit is
    /// published with a `defer` still pending. The exit is delivered here, so no `recover:` catches
    /// the unwind and no caller runs on.
    ///
    /// The length test comes first, so a return with no `defer` takes no mutex. The read is the
    /// exit cell (`quiesce.pending()`), never `run_halt_hint()`: the hint is stored after the scheds
    /// are halted, and a party woken by that halt would still run its `defer` (TICKET-213, measured
    /// at two workers).
    fn next_deferred(
        &mut self,
        fi: usize,
        marker: usize,
    ) -> Result<Option<Deferred>, RuntimeError> {
        if self.frames[fi].deferred.len() <= marker || self.pending_exit.is_some() {
            return Ok(None);
        }
        if self.quiesce.run_halt().cuts_cleanup()
            && let Some(e) = self.run_exit_err(Span::RUNTIME)
        {
            return Err(e);
        }
        Ok(self.frames[fi].deferred.pop())
    }

    /// TICKET-223 — THE post-drain read of "a hard halt cut this cleanup": this party is exiting
    /// (`next_deferred` delivered the exit), the deadlock verdict is latched, or `rte` itself is a
    /// hard halt (`--timeout`, `--max-heap`: [`executor_hard_halt`]), which a cleanup met while a
    /// fault unwound or a recover block drained (TICKET-289). `Err` carries `rte`, stamped fatal
    /// for a verdict (DEC-135), so no `recover:` catches it; `next_deferred` already stops the
    /// remaining `defer`s (DEC-152). `rte` keeps its message, so a delivered cause still outranks
    /// its stuck cleanup in the report (DEC-147). Every route that binds a `recover:`'s `Err`
    /// reads it: `finish_catch` and the recover-scoped `?` catch in `stmt.rs`.
    pub(super) fn cleanup_halt(&self, rte: RuntimeError) -> Result<RuntimeError, RuntimeError> {
        if self.pending_exit.is_some() || executor_hard_halt(&rte) {
            return Err(rte);
        }
        if self.quiesce.run_halt() == RunHalt::Deadlock {
            return Err(rte.deadlock());
        }
        Ok(rte)
    }

    /// Run frame `fi`'s pending `defer`s down to `marker`, LIFO, each popped by
    /// [`Vm::next_deferred`]. Returns the latest fault; an exit ends the drain and is the result.
    fn drain_deferred(&mut self, fi: usize, marker: usize) -> Option<RuntimeError> {
        let mut err = None;
        loop {
            match self.next_deferred(fi, marker) {
                Ok(Some(d)) => {
                    if let Err(e) = self.run_one_deferred(d) {
                        err = Some(e);
                    }
                }
                Ok(None) => return err,
                Err(exit) => return Some(exit),
            }
        }
    }

    /// Leave a lexical defer scope (`LeaveDeferScope`): pop the top marker and run the current
    /// frame's defers registered since it, LIFO. This is the block-scoped analogue of
    /// `drain_top_frame_deferred` — it drains down to a marker, not to the bottom of the frame.
    /// Stops once any party's `std.os.exit` is published, between two `defer`s too. Returns the
    /// latest fault from a deferred call, if any.
    pub(super) fn leave_defer_scope(&mut self) -> Option<RuntimeError> {
        let fi = self.frames.len() - 1;
        debug_assert!(
            !self.frames[fi].defer_markers.is_empty(),
            "LeaveDeferScope without a matching EnterDeferScope (compiler scope-count desync)"
        );
        let marker = self.frames[fi].defer_markers.pop().unwrap_or(0);
        self.drain_frame_to(marker)
    }

    /// Drain the current (top) frame's pending defers down to `marker` (the count to leave behind),
    /// LIFO. The block-scoped analogue of `drain_top_frame_deferred` for an explicit marker — used
    /// by `LeaveDeferScope` and by every `recover:` boundary path. Stops once any party's
    /// `std.os.exit` is published, between two `defer`s too. Returns the latest fault from a
    /// deferred call, if any.
    pub(super) fn drain_frame_to(&mut self, marker: usize) -> Option<RuntimeError> {
        let fi = self.frames.len() - 1;
        self.drain_deferred(fi, marker)
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
        self.unwind_deferred_escaped(target_frame_len, report_escaped, true, false)
            .0
    }

    /// TICKET-152 (W14-37) — the FATAL-deadlock unwind: drop every frame down to
    /// `target_frame_len` WITHOUT running its `defer`s. A deadlock verdict is the runtime declaring
    /// that no task can make progress, so a `defer` that itself blocks would run inside a runtime
    /// that has already said so; Go's `fatal error: all goroutines are asleep - deadlock!` runs
    /// none, and parked siblings already run none (DEC-092). `report_escaped` stays `true`: DEC-135
    /// (W14-39) measured that skipping the escaped-nursery abort orphans parked children and hangs
    /// the join at `CHEZZI_THREADS=1`.
    pub(super) fn unwind_no_defer(&mut self, target_frame_len: usize) {
        self.unwind_deferred_escaped(target_frame_len, true, false, false);
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
        let (defer, escaped) =
            self.unwind_deferred_escaped(target_frame_len, report_escaped, true, false);
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
    /// A drain with no cause to rank (`Op::DrainHandlerDefers`, `unwind_escaped_levels`,
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
    /// `park` — TICKET-287: the fault unwind of a fiber's outermost dispatch loop. At a frame whose
    /// escaped nursery is fiber-owned and not done, the unwind stops BEFORE that frame's abort and
    /// defers, with `join_suspend` set ([`Vm::park_abort_origin`]); the caller stores a
    /// [`FaultPark`](super::frames::FaultPark) and re-enters here at the same frame on resume.
    pub(super) fn unwind_deferred_escaped(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
        run_defers: bool,
        park: bool,
    ) -> (Option<RuntimeError>, Option<RuntimeError>) {
        let mut err = None;
        let mut escaped_err: Option<RuntimeError> = None;
        while self.frames.len() > target_frame_len {
            let fi = self.frames.len() - 1;
            // Report this frame's escaped nurseries BEFORE its defers (drain pops innermost-first, so
            // inner `parallel:` levels report before the frame's implicit one — the interp's order).
            if report_escaped {
                let f = &self.frames[fi];
                let floor = if f.is_toplevel && f.has_implicit_nursery {
                    f.nursery_len + 1 // preserve the module nursery
                } else {
                    f.nursery_len
                };
                let floor = floor.min(self.nurseries.len());
                if park && let Some(origin) = self.park_abort_origin(floor) {
                    self.join_suspend = Some(origin);
                    return (err, escaped_err);
                }
                let child = self.drain_escaped_nursery(floor);
                escaped_err = escaped_err.or(child);
            }
            if run_defers && let Some(e) = self.drain_deferred(fi, 0) {
                err = Some(e);
            }
            let frame = self.frames.pop().unwrap();
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
        (err, escaped_err)
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
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                let frame_len = self.frames.len();
                let same_frame = self
                    .handlers
                    .last()
                    .copied()
                    .filter(|h| h.frame_len == frame_len);
                // The escaped `parallel:` body's defers (ORDERING below), drained before the
                // handler pops so the TICKET-287 park can still rewind this `?`. A re-run finds
                // them consumed and takes back the fault the first run parked with.
                let body_defers = match same_frame {
                    Some(h) if self.nurseries.len() > h.nursery_len => {
                        let floor = self.nursery_defer_floors[h.nursery_len];
                        let parked = self.take_escape_park();
                        self.drain_frame_to(floor).or(parked)
                    }
                    _ => None,
                };
                // TICKET-287 — a fiber-owned nursery that is not done parks this fiber, as
                // `park_escaped_abort` does for `return`. A body defer fault parks with it.
                if let Some(h) = same_frame
                    && let Some(origin) = self.park_abort_origin(h.nursery_len)
                {
                    if let Some(e) = body_defers {
                        self.store_escape_park(e);
                    }
                    self.push(v);
                    self.rewind_op();
                    self.join_suspend = Some(origin);
                    return Ok(());
                }
                if let Some(h) = self.handlers.pop_if(|h| h.frame_len == frame_len) {
                    self.stack.truncate(h.stack_len);
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
                    // Drain the recover block's own defers before binding the result. A fault in one
                    // supersedes the propagated value (becomes the recover's `Err`); a recover-block
                    // defer fault in turn supersedes a body defer fault (it unwinds later).
                    match self.drain_frame_to(h.defer_len) {
                        Some(e) => {
                            let e = self.cleanup_halt(e)?;
                            let sp = e.span;
                            let msg = self.alloc_str(e.message);
                            self.stamp_err_span(msg, sp);
                            let err = self.alloc_enum("Result", "Err", &[msg]);
                            self.push(err);
                        }
                        None => match body_defer_err {
                            // No recover-block defer fault, but a parallel-body defer faulted: that
                            // becomes the recover's `Err` (Go semantics — a defer fault supersedes).
                            Some(e) => {
                                let sp = e.span;
                                let msg = self.alloc_str(e.message);
                                self.stamp_err_span(msg, sp);
                                let err = self.alloc_enum("Result", "Err", &[msg]);
                                self.push(err);
                            }
                            None => self.push(v), // the propagated Result/Option value IS the result
                        },
                    }
                    // W13-20 — a deferred `.next()` run by either drain above (the escaped `parallel:`
                    // body's, or the recover block's own) can fault and park its own frames in
                    // `gen_fault_prefix` without ever passing through the dispatch loop's error arm
                    // to be taken (this recover-scoped `?` catches inline, never via that arm). Clear
                    // it here too, or it survives to decorate a later, unrelated uncaught fault — the
                    // same leak class as the `recover:`-catch path in `exec.rs`, one level lower.
                    self.gen_fault_prefix.clear();
                    self.jump(h.ip);
                    return Ok(());
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
                // deferred calls first; a fault in one propagates as a fault).
                self.push(v);
                self.do_return(true)?;
                return Ok(());
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
    /// key, e.g. `<main>::Point`) on the cold path (method dispatch / Display / arith / hash / wire /
    /// snap), where `Obj::Struct` no longer carries a per-instance `name`. O(1) via `struct_names`.
    /// Returns `"?"` for [`crate::base::op::TID_NONE`] / an out-of-range id (defensive — every
    /// source-constructed struct has a registered `def.tid`, so this is unreachable for Display).
    pub(super) fn struct_name_of_tid(&self, tid: u32) -> &str {
        self.walk_src().struct_name_of_tid(tid)
    }

    /// M19 lever #2 — resolve a `variant_id` back to its `(enum-type, variant)` names on the COLD path
    /// (Display / stringify / error / wire / snap), where the instance no longer carries the strings.
    /// O(1) via `Program::variants_by_id`. Returns `("?", "?")` for [`crate::base::op::VID_NONE`] / an
    /// out-of-range id (defensive — a registered enum always resolves).
    pub(super) fn enum_names(&self, variant_id: u32) -> (&str, &str) {
        self.program
            .variants_by_id
            .get(variant_id as usize)
            .map_or(("?", "?"), |d| (d.enum_name.as_str(), d.name.as_str()))
    }

    /// The display rule of a carrier (TICKET-228): what a `T?` / `T!E` value prints in front of
    /// its payload, the way the user writes it. An error is `!e`; a success or a present value is
    /// its bare payload; `?` prints only in front of an absent payload (`?None`), so it stays
    /// distinct from `None`. Rust `None` for a non-carrier and for the absent value itself, which
    /// print by their variant name. `absent_payload` says whether the payload is absent or is
    /// itself a `?`-prefixed present value.
    fn carrier_prefix_of(
        variant_id: u32,
        absent_payload: impl FnOnce() -> bool,
    ) -> Option<&'static str> {
        match variant_id {
            crate::base::op::VID_ERR => Some("!"),
            crate::base::op::VID_OK => Some(""),
            crate::base::op::VID_SOME => Some(if absent_payload() { "?" } else { "" }),
            _ => None,
        }
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

    /// [`Self::carrier_prefix_of`] for a heap carrier.
    pub(super) fn carrier_prefix(
        &self,
        variant_id: u32,
        payload: &RVec<Value>,
    ) -> Option<&'static str> {
        Self::carrier_prefix_of(variant_id, || {
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
        })
    }

    /// [`Self::carrier_prefix_of`] for a wire carrier.
    pub(super) fn carrier_prefix_wire(
        variant_id: u32,
        payload: &[WireValue],
    ) -> Option<&'static str> {
        Self::carrier_prefix_of(variant_id, || match payload.first() {
            Some(WireValue::Enum {
                variant_id: inner,
                payload: inner_payload,
                ..
            }) => {
                *inner == crate::base::op::VID_NONE_VARIANT
                    || (*inner == crate::base::op::VID_SOME
                        && Self::carrier_prefix_wire(*inner, inner_payload) == Some("?"))
            }
            _ => false,
        })
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
