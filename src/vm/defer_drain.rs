//! VM: the `defer` drain on the dispatch loop: the trampoline proto, the per-task drain stack, the
//! continuation each drain runs when it ends, and the one call of a `defer` record.
//!
//! A drain the dispatch loop starts ([`Vm::drain_then`]) pushes a trampoline frame above the
//! drained frame. The trampoline's ops ([`DrainStep`]) run each record above the drain's marker
//! through `Op::Call`'s or `Op::CallMethod`'s own path, so a wait in a `defer` body parks the task
//! and rewinds to `DrainStep::Call` like any call. A record stays in its frame's `deferred` list
//! until its call returns, so the GC keeps it rooted ([`super::root_registry`]). A drain reached
//! from a synchronous native caller (`native_reentry > 0`), and the unwind of a hard halt, run
//! the records on the Rust stack instead ([`Vm::run_one_deferred`]); both paths call a record with
//! [`Vm::call_loaded`].

use super::frames::Unwind;
use super::*;
use crate::base::op::{DrainStep, Proto};
use std::sync::LazyLock;

/// The trampoline's proto id. It indexes no `Program::protos` entry: the VM owns the one trampoline
/// proto ([`drain_proto`]), which the dispatch loop fetches for this id.
pub(super) const DRAIN_PROTO: ProtoId = ProtoId::MAX;

const STEPS: [DrainStep; 2] = [DrainStep::Call, DrainStep::Next];

/// The trampoline proto, built once per process: one op per [`DrainStep`], in declaration order, so
/// a step's `ip` is `step as usize`.
pub(super) fn drain_proto() -> &'static Proto {
    static PROTO: LazyLock<Proto> = LazyLock::new(|| Proto {
        name: "<defer drain>".to_string(),
        arity: 0,
        min_arity: 0,
        n_slots: 0,
        code: STEPS.map(Op::Drain).to_vec(),
        lines: vec![Span::RUNTIME; STEPS.len()],
        has_implicit_nursery: false,
        is_generator: false,
        is_test: false,
        decl_span: Span::RUNTIME,
        capture_names: Vec::new(),
    });
    &PROTO
}

/// One live drain of this task: an entry of [`Vm::drains`], pushed with its trampoline frame and
/// popped with it.
pub(super) struct Drain {
    /// The trampoline frame's index; the drained frame is the one below it.
    frame: usize,
    /// The drain runs the drained frame's records above this count.
    marker: usize,
    /// The latest fault a record of this drain raised (a later one replaces it), or the exit that
    /// cut the drain.
    err: Option<RuntimeError>,
    then: DrainCont,
}

/// What runs when a drain ends ([`Vm::drain_done`]), with the drain's latest fault.
pub(super) enum DrainCont {
    /// A `return` (or a function-scoped `?`): the rest of `do_return`; `err` is the fault the
    /// escaped levels raised.
    Return { err: Option<RuntimeError> },
    /// The body `defer`s of escaped `parallel:` level `top` of a `return`.
    ReturnLevel {
        top: usize,
        err: Option<RuntimeError>,
    },
    /// `Op::LeaveDeferScope` / `Op::DrainHandlerDefers`: the fault is the op's.
    Op,
    /// A recover-scoped `?` that escapes a `parallel:` body ran the body's `defer`s; `parked` is
    /// the body fault an earlier run of the `?` parked with.
    TryBody {
        span: Span,
        parked: Option<RuntimeError>,
    },
    /// A recover-scoped `?` ran its recover block's `defer`s; `body` is the body's fault and `ip`
    /// the recover's exit.
    TryRecover {
        ip: usize,
        body: Option<RuntimeError>,
    },
    /// A fault or cancel unwind ran the `defer`s of the frame it is at. Boxed: a drain moves its
    /// continuation on every return, and an unwind is the one large one.
    Unwind(Box<Unwind>),
    /// A `recover:` that caught `rte` ran its recover block's `defer`s ([`Vm::catch_drained`]).
    Catch {
        base_level: usize,
        nursery_len: usize,
        markers_len: usize,
        rte: RuntimeError,
    },
}

impl Vm {
    /// Run the top frame's `defer`s above `marker`, LIFO, then `then`. From the dispatch loop the
    /// drain is a trampoline frame with its first record loaded, and `then` runs at the
    /// `DrainStep::Next` that finds no record left; under a synchronous native caller it runs here.
    /// A frame with no record to run above `marker` runs `then` at once.
    pub(super) fn drain_then(
        &mut self,
        marker: usize,
        then: DrainCont,
    ) -> Result<(), RuntimeError> {
        let fi = self.frames.len() - 1;
        if self.frames[fi].deferred.len() <= marker {
            return self.drain_done(then, None);
        }
        if !self.drains_on_frames() {
            let err = self.drain_deferred(fi, marker);
            return self.drain_done(then, err);
        }
        match self.has_next_deferred(fi, marker) {
            Ok(true) => {}
            Ok(false) => return self.drain_done(then, None),
            Err(exit) => return self.drain_done(then, Some(exit)),
        }
        let base = self.stack.len();
        let home = self.frames[fi].home;
        self.frames.push(CallFrame {
            proto: DRAIN_PROTO,
            ip: 0,
            base,
            home,
            closure: None,
            counted: false,
            is_toplevel: false,
            deferred: Vec::new(),
            defer_markers: Vec::new(),
            nursery_len: self.nurseries.len(),
            has_implicit_nursery: false,
            call_span: Span::RUNTIME,
            argc: 0,
            ret: RetAction::Push,
        });
        self.cur_base = base;
        self.drains.push(Drain {
            frame: fi + 1,
            marker,
            err: None,
            then,
        });
        self.load_deferred(fi);
        Ok(())
    }

    /// THE end of a drain: run its continuation with the drain's latest fault.
    pub(super) fn drain_done(
        &mut self,
        then: DrainCont,
        err: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        match then {
            DrainCont::Return { err: prior } => self.return_finish(err.or(prior)),
            DrainCont::ReturnLevel { top, err: prior } => {
                self.return_level_done(top, err.or(prior))
            }
            DrainCont::Op => match err {
                Some(e) => Err(e),
                None => Ok(()),
            },
            DrainCont::TryBody { span, parked } => self.try_escape(span, err.or(parked)),
            DrainCont::TryRecover { ip, body } => self.try_recover(ip, body, err),
            DrainCont::Unwind(u) => self.unwind_drained(*u, err),
            DrainCont::Catch {
                base_level,
                nursery_len,
                markers_len,
                rte,
            } => self.catch_drained(base_level, nursery_len, markers_len, rte, err),
        }
    }

    /// One trampoline op.
    pub(super) fn defer_drain_step(&mut self, step: DrainStep) -> Result<(), RuntimeError> {
        let d = self
            .drains
            .last()
            .expect("a trampoline frame has its drain");
        let (fi, marker) = (d.frame - 1, d.marker);
        match step {
            DrainStep::Call => {
                if let Err(e) = self.call_loaded(fi) {
                    self.drain_fault(e);
                }
                Ok(())
            }
            DrainStep::Next => {
                let base = self.frames[fi + 1].base;
                self.stack.truncate(base);
                self.frames[fi].deferred.pop();
                match self.has_next_deferred(fi, marker) {
                    Ok(true) => {
                        self.load_deferred(fi);
                        self.jump(DrainStep::Call as usize);
                        Ok(())
                    }
                    Ok(false) => self.drain_end(),
                    Err(exit) => {
                        self.drain_mut().err = Some(exit);
                        self.drain_end()
                    }
                }
            }
        }
    }

    /// The innermost drain ran its last record: pop its trampoline frame and run its continuation.
    fn drain_end(&mut self) -> Result<(), RuntimeError> {
        let d = self.drains.pop().expect("a trampoline frame has its drain");
        let frame = self.frames.pop().expect("the trampoline frame");
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        // An unwind's or a catch's continuation that fails has already run the fault arm
        // ([`Vm::on_step_fault`]) on its fault: the fault arm only hands it on.
        let unwound = matches!(d.then, DrainCont::Unwind(_) | DrainCont::Catch { .. });
        let r = self.drain_done(d.then, d.err);
        self.unwound = unwound && r.is_err();
        r
    }

    fn drain_mut(&mut self) -> &mut Drain {
        self.drains
            .last_mut()
            .expect("a trampoline frame has its drain")
    }

    /// A fault of the running record of the innermost drain, which is the top frame: it becomes
    /// the drain's latest fault and the drain goes on with the next record.
    pub(super) fn drain_fault(&mut self, e: RuntimeError) {
        debug_assert_eq!(self.frames.last().map(|f| f.proto), Some(DRAIN_PROTO));
        self.drain_mut().err = Some(e);
        self.jump(DrainStep::Next as usize);
    }

    /// Does a drain run on the dispatch loop (a trampoline frame)? Not under a synchronous native
    /// caller (`native_reentry > 0`): its Rust frame holds the loop, so a drain runs to completion
    /// on the Rust stack.
    pub(super) fn drains_on_frames(&self) -> bool {
        self.native_reentry == 0
    }

    /// The innermost drain's trampoline frame, when the dispatch loop entered at `base_level` owns
    /// it. A fault above it is that drain's: [`Vm::on_step_fault`] unwinds no lower.
    pub(super) fn owned_drain(&self, base_level: usize) -> Option<usize> {
        self.drains
            .last()
            .map(|d| d.frame)
            .filter(|&t| t >= base_level)
    }

    /// Is a `defer` of this task running (a trampoline on this task, or a synchronous drain)?
    /// A cancel is never delivered then: a `defer` is the cleanup a cancel exists to run.
    pub(super) fn in_defer(&self) -> bool {
        self.deferring > 0 || !self.drains.is_empty()
    }

    /// A synchronous unwind drops trampoline frame `fi` (a hard halt met a drain): end its drain,
    /// and drop the record it was running (a live trampoline always has one loaded), as the
    /// synchronous drain pops a record after it ran. Returns the latest fault the drain's records raised, which
    /// the unwind ranks as a `defer` fault, as the synchronous drain's fault is; the rest of the
    /// drained frame's `defer`s run next, in the same unwind. The continuation does not run: the
    /// unwind replaces it.
    pub(super) fn drop_drain_frame(&mut self, fi: usize) -> Option<RuntimeError> {
        let d = self.drains.pop().expect("a trampoline frame has its drain");
        debug_assert_eq!(d.frame, fi);
        self.frames[fi - 1].deferred.pop();
        d.err
    }

    /// Push frame `fi`'s next record's callee (or receiver) and args. The record stays in the
    /// frame, so it is rooted while it runs.
    fn load_deferred(&mut self, fi: usize) {
        let Vm { frames, stack, .. } = self;
        let (head, args) = match frames[fi].deferred.last().expect("a loaded record") {
            Deferred::Call { callee, args, .. } => (*callee, args),
            Deferred::Method { recv, args, .. } => (*recv, args),
        };
        stack.push(head);
        stack.extend_from_slice(args);
    }

    /// THE call of frame `fi`'s next record, loaded by [`Vm::load_deferred`]: `Op::Call`'s path for
    /// a call, `Op::CallMethod`'s for a method (through the record's own `defer`-site IC, for the
    /// trampoline and the synchronous drain alike). A frame it pushes returns its value onto the
    /// stack.
    fn call_loaded(&mut self, fi: usize) -> Result<(), RuntimeError> {
        match self.frames[fi].deferred.last().expect("a loaded record") {
            Deferred::Call { args, span, .. } => {
                let (argc, span) = (args.len(), *span);
                self.do_call(argc, span)
            }
            Deferred::Method {
                name,
                ic,
                args,
                span,
                ..
            } => {
                let (name, ic, argc, span) = (Arc::clone(name), *ic, args.len(), *span);
                self.do_method_call(&name, argc, ic, span)
            }
        }
    }

    /// Run frame `fi`'s next record to completion on the Rust stack and drop its result: the
    /// synchronous drain's body ([`Vm::run_one_deferred`]).
    pub(super) fn run_loaded(&mut self, fi: usize) -> Result<(), RuntimeError> {
        let level = self.frames.len();
        let height = self.stack.len();
        self.load_deferred(fi);
        let mut r = self.call_loaded(fi);
        if r.is_ok() && self.frames.len() > level {
            r = self.run_until(level);
        }
        self.stack.truncate(height);
        r
    }
}
