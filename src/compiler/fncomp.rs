//! Compiler: the per-function compiler state `FnComp`: slots, jumps, scopes, locals.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;

pub(super) struct FnComp {
    pub(super) name: String,
    pub(super) arity: usize,
    /// See [`crate::vm::op::Proto::min_arity`]. Starts at `arity`; only the default prologue lowers it.
    pub(super) min_arity: usize,
    is_toplevel: bool,
    /// W13-21 — see [`crate::vm::op::Proto::decl_span`]. Set from `FnDecl::name_span` only in
    /// `Compiler::compile_fn_body`; every other `FnComp::new` site keeps `Span::RUNTIME`.
    pub(super) decl_span: Span,
    pub(super) code: Vec<Op>,
    pub(super) lines: Vec<Span>,
    pub(super) locals: Vec<LocalVar>,
    scope_depth: usize,
    /// Locals below this index are hidden from every NAME lookup; read only by
    /// [`Self::visible_locals`]. Raised only by [`Compiler::emit_default_param_prologue`], so a
    /// default sees module scope while its own binders still allocate from `slot_count`
    /// (slot == `locals` index).
    pub(super) name_floor: usize,
    /// Number of slots currently in use (== next free slot).
    slot_count: usize,
    pub(super) max_slots: usize,
    /// Names this function captures from an enclosing scope (closures only).
    pub(super) captured_names: Vec<String>,
    /// Uniform by-reference capture — this frame's local names that are captured by a nested
    /// closure / `defer:` / `spawn:` body, so their slots are BOXED (`Obj::Cell`): declared with a
    /// `NewCell`, read via `CellLoad`, written via `CellStore` (see [`emit_get_named`]/
    /// [`emit_set_named`]). Computed by [`captured_names_of_body`] at each fn/closure body construction.
    pub(super) boxed_names: std::collections::HashSet<String>,
    /// Stack of enclosing loops (innermost last), for `break`/`continue` jump patching. Empty
    /// outside any loop. A closure compiles in its own `FnComp` with an empty stack, so a
    /// `break`/`continue` there has no current loop (the checker rejects it; we defend anyway).
    pub(super) loops: Vec<LoopCtx>,
    /// Count of defer scopes currently open during compilation (incremented at `EnterDeferScope`,
    /// decremented at the matching `LeaveDeferScope`). Read by `break`/`continue` to know how many
    /// scopes to drain down to the loop body.
    pub(super) defer_scopes: usize,
    /// TASK B — count of `parallel:` nursery scopes currently open during compilation (incremented at
    /// the `EnterNursery` emit in `compile_parallel`, decremented at the matching `JoinNursery`). Read
    /// by `break`/`continue` to know how many `ReclaimNursery`s to emit before jumping out of the loop.
    pub(super) nursery_scopes: usize,
    /// W14-4 -- one entry per open `parallel:` level, innermost last: the `defer_scopes` count when
    /// the level was entered, before its body's `EnterDeferScope`. Implicit nurseries push nothing;
    /// they open at `FnComp` entry, below every loop.
    pub(super) nursery_defer_marks: Vec<usize>,
    /// M-C — this body opened an implicit nursery at entry (its `Op::EnterNursery` is the first body
    /// op) because it contains a bare `spawn`. Stamped onto the [`Proto`] in `finish`; the VM's
    /// `do_return` JOINS this nursery at the body's `return`/end.
    pub(super) has_implicit_nursery: bool,
    /// Experimental — this proto is a generator body (its `Op::Yield`s suspend the generator).
    pub(super) is_generator: bool,
    /// This proto is a `test fn` body (free test or suite method). Stamped onto the [`Proto`] in
    /// `finish`; used only by `chezzi test` discovery.
    pub(super) is_test: bool,
    /// TICKET-190, TICKET-240 — the checker's verdict per private frame slot name of this
    /// generator body (empty for every other body). Read only by [`FnComp::add_frame_local`].
    pub(super) gen_slots: HashMap<String, crate::vm::crossing::SlotFresh>,
    /// TICKET-190 — per slot, the AND of every binder's privacy claim (`None` = never claimed). A
    /// slot reused by sibling scopes is private only if every binding in it is.
    pub(super) slot_private: Vec<Option<bool>>,
    /// TICKET-240 — the same AND for the claim that the slot's whole graph is private.
    pub(super) slot_deep: Vec<Option<bool>>,
}

impl FnComp {
    /// A NEW `is_toplevel = false` proto needs no W8-2 checker flag: the discarded-carrier warning
    /// (`src/checker/sig.rs`, the `StmtKind::Expr` arm) fires on every proto now, top level included.
    pub(super) fn new(name: String, arity: usize, is_toplevel: bool) -> Self {
        FnComp {
            name,
            arity,
            // Short entry is opt-in: `emit_default_param_prologue` lowers this when, and only when,
            // it actually emits a fill for a trailing defaulted parameter.
            min_arity: arity,
            is_toplevel,
            decl_span: Span::RUNTIME,
            code: Vec::new(),
            lines: Vec::new(),
            locals: Vec::new(),
            scope_depth: 0,
            name_floor: 0,
            slot_count: 0,
            max_slots: 0,
            captured_names: Vec::new(),
            boxed_names: std::collections::HashSet::new(),
            loops: Vec::new(),
            defer_scopes: 0,
            nursery_scopes: 0,
            nursery_defer_marks: Vec::new(),
            has_implicit_nursery: false,
            is_generator: false,
            is_test: false,
            gen_slots: HashMap::new(),
            slot_private: Vec::new(),
            slot_deep: Vec::new(),
        }
    }

    /// The innermost loop being compiled, or `None` outside any loop.
    pub(super) fn current_loop(&mut self) -> Option<&mut LoopCtx> {
        self.loops.last_mut()
    }

    /// Uniform by-reference capture — is local `slot`'s name in this frame's boxed set (so
    /// reads/writes go through an `Obj::Cell`)? Consulted by [`emit_get_named`]/[`emit_set_named`].
    pub(super) fn is_boxed_slot(&self, slot: usize) -> bool {
        self.locals
            .get(slot)
            .is_some_and(|l| self.boxed_names.contains(&l.name))
    }

    /// Will a local NAMED `name` be boxed if declared now? (Same predicate as [`is_boxed_slot`] but by
    /// name — for deciding a binding's slot strategy BEFORE the slot is allocated, e.g. a `match`
    /// variant binding the VM writes positionally.)
    pub(super) fn is_boxed_name(&self, name: &str) -> bool {
        self.boxed_names.contains(name)
    }

    // ----- slot emit routing (B1: exactly these are the only ways to touch a slot) -----
    //
    // A boxed slot holds an `Obj::Cell` HANDLE, never the value directly. A bare `GetLocal` on a
    // boxed slot would leave a raw cell handle on the stack — and the peephole fuser can then fold it
    // into an int superinstruction (pointer-as-int → crash). So every named user local read/write
    // routes through `emit_get_named`/`emit_set_named` (cell-aware), and every hidden compiler temp
    // routes through `emit_hidden_get`/`emit_hidden_set` (raw + a debug-assert that the slot is
    // genuinely unnamed). Param prologues, which legitimately move a boxed slot's handle before the
    // box exists, use the private raw helpers.

    /// Raw `GetLocal` — no cell-awareness, no assert. For the named/prologue paths that deliberately
    /// touch a boxed slot to move its handle.
    pub(super) fn emit_get_local_raw(&mut self, slot: usize, span: Span) {
        self.emit(Op::GetLocal(slot), span);
    }

    /// Raw `SetLocal` — no cell-awareness, no assert. See [`emit_get_local_raw`].
    pub(super) fn emit_set_local_raw(&mut self, slot: usize, span: Span) {
        self.emit(Op::SetLocal(slot), span);
    }

    /// Read a HIDDEN compiler temp (loop bookkeeping, match scrutinee, …). Debug-asserts the slot is
    /// unnamed so a misrouted named slot (which might be boxed) panics loudly instead of leaking a
    /// raw cell handle.
    pub(super) fn emit_hidden_get(&mut self, slot: usize, span: Span) {
        debug_assert!(
            self.locals.get(slot).is_none_or(|l| l.name.is_empty()),
            "hidden get on a NAMED slot {slot}"
        );
        self.emit(Op::GetLocal(slot), span);
    }

    /// Write a HIDDEN compiler temp. See [`emit_hidden_get`].
    pub(super) fn emit_hidden_set(&mut self, slot: usize, span: Span) {
        debug_assert!(
            self.locals.get(slot).is_none_or(|l| l.name.is_empty()),
            "hidden set on a NAMED slot {slot}"
        );
        self.emit(Op::SetLocal(slot), span);
    }

    /// Read a NAMED user local. A boxed slot dereferences its cell (`GetLocal; CellLoad`); a plain
    /// slot is a bare `GetLocal` (byte-identical to before boxing existed).
    pub(super) fn emit_get_named(&mut self, slot: usize, span: Span) {
        self.emit_get_local_raw(slot, span);
        if self.is_boxed_slot(slot) {
            self.emit(Op::CellLoad, span);
        }
    }

    /// Store to a NAMED user local — the value is already on the stack top. A boxed slot pushes the
    /// cell handle on top (stack `[val, handle]`) and `CellStore`s (which pops handle-first, then
    /// value); a plain slot is a bare `SetLocal`.
    pub(super) fn emit_set_named(&mut self, slot: usize, span: Span) {
        if self.is_boxed_slot(slot) {
            self.emit_get_local_raw(slot, span);
            self.emit(Op::CellStore, span);
        } else {
            self.emit_set_local_raw(slot, span);
        }
    }

    /// Declare a NEW named local `name` from the value currently on the stack top, returning its
    /// slot. A boxed name wraps the value in a FRESH cell (`NewCell`) and stores the handle; a plain
    /// name stores the value directly. Used at every named-binding site (`:=`, destructuring,
    /// `match`/`if let` bind, wait-assign) — each fresh binding gets its own cell.
    pub(super) fn emit_decl_named(&mut self, name: String, span: Span) -> usize {
        let slot = self.add_local(name);
        self.emit_decl_into(slot, span)
    }

    /// TICKET-190 — [`Self::emit_decl_named`] for a single-name `let`: its slot claims the checker's
    /// frame privacy verdict ([`Self::add_frame_local`]).
    pub(super) fn emit_decl_frame(&mut self, name: String, span: Span) -> usize {
        let slot = self.add_frame_local(name);
        self.emit_decl_into(slot, span)
    }

    fn emit_decl_into(&mut self, slot: usize, span: Span) -> usize {
        if self.is_boxed_slot(slot) {
            self.emit(Op::NewCell, span);
        }
        self.emit_set_local_raw(slot, span);
        slot
    }

    /// The MECHANISM slot for a loop variable `user`: a fresh HIDDEN slot when `user` is boxed (the
    /// loop bookkeeping — `MatchArm` binds / index reads — writes the raw value here, and the user
    /// cell is refreshed from it per iteration by [`emit_loopvar_refresh`]), else `user` itself
    /// (byte-identical to before boxing when the loop var isn't captured).
    pub(super) fn loopvar_raw_slot(&mut self, user: usize) -> usize {
        if self.is_boxed_slot(user) {
            self.add_hidden()
        } else {
            user
        }
    }

    /// Per-iteration fresh-cell wrap for a boxed loop variable (C1, Go ≥1.22): read the mechanism's
    /// raw slot, box it in a FRESH cell, store the handle in the user slot — so each iteration's
    /// closure captures its OWN cell. No-op when `user` isn't boxed (then `raw == user`). Emitted at
    /// the top of the loop body, after the item value is produced, before the body runs.
    pub(super) fn emit_loopvar_refresh(&mut self, user: usize, raw: usize, span: Span) {
        if self.is_boxed_slot(user) {
            debug_assert_ne!(
                user, raw,
                "boxed loop var needs a separate raw mechanism slot"
            );
            self.emit_get_local_raw(raw, span);
            self.emit(Op::NewCell, span);
            self.emit_set_local_raw(user, span);
        }
    }

    /// Patch a previously-emitted jump to land at an explicit `target` instruction index (used for
    /// `continue`, whose target — the loop's increment — is *before* the current position).
    pub(super) fn patch_jump_to(&mut self, at: usize, target: usize) {
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other @ Op::ConstInt(_)
            | other @ Op::ConstFloat(_)
            | other @ Op::ConstStr(_)
            | other @ Op::ConstBytes(_)
            | other @ Op::True
            | other @ Op::False
            | other @ Op::Nil
            | other @ Op::Pop
            | other @ Op::StampGen(_)
            | other @ Op::Assert { .. }
            | other @ Op::LoadBuiltin(_)
            | other @ Op::GetLocal(_)
            | other @ Op::SetLocal(_)
            | other @ Op::GetGlobalSlot(_)
            | other @ Op::DefineGlobalSlot(_)
            | other @ Op::SetGlobalSlot(_)
            | other @ Op::GetCaptured(_)
            | other @ Op::Add
            | other @ Op::Sub
            | other @ Op::Mul
            | other @ Op::Div
            | other @ Op::Mod
            | other @ Op::Neg
            | other @ Op::Not
            | other @ Op::Lt
            | other @ Op::LtEq
            | other @ Op::Gt
            | other @ Op::GtEq
            | other @ Op::Eq
            | other @ Op::NotEq
            | other @ Op::BitAnd
            | other @ Op::BitOr
            | other @ Op::BitXor
            | other @ Op::Shl
            | other @ Op::Shr
            | other @ Op::Contains
            | other @ Op::AsBool
            | other @ Op::AsInt
            | other @ Op::BinLocalLocal { .. }
            | other @ Op::BinLocalConst { .. }
            | other @ Op::IncLocal { .. }
            | other @ Op::PopHandler
            | other @ Op::Call(_)
            | other @ Op::CallMethod { .. }
            | other @ Op::CallHook { .. }
            | other @ Op::CallStatic { .. }
            | other @ Op::CallStaticDyn { .. }
            | other @ Op::CallBuiltin(..)
            | other @ Op::CallPrint(_)
            | other @ Op::CallPrintSep { .. }
            | other @ Op::Return
            | other @ Op::Yield
            | other @ Op::DeferCall(_)
            | other @ Op::DeferMethod(..)
            | other @ Op::EnterDeferScope
            | other @ Op::LeaveDeferScope
            | other @ Op::DrainHandlerDefers
            | other @ Op::Try
            | other @ Op::JsonDecode(_)
            | other @ Op::JsonToValue
            | other @ Op::NewList(_)
            | other @ Op::NewTuple(_)
            | other @ Op::NewMap(_)
            | other @ Op::NewSet(_)
            | other @ Op::NewStruct(..)
            | other @ Op::NewCell
            | other @ Op::CellLoad
            | other @ Op::CellStore
            | other @ Op::NewEnum { .. }
            | other @ Op::MakeFunc(_)
            | other @ Op::MakeFuncIn(_)
            | other @ Op::MakeMethodFunc { .. }
            | other @ Op::JumpIfProvided(..)
            | other @ Op::MakeCffi(_)
            | other @ Op::MakeClosure(..)
            | other @ Op::GetField { .. }
            | other @ Op::GetIndex
            | other @ Op::SliceRange
            | other @ Op::SetField { .. }
            | other @ Op::SetIndex
            | other @ Op::Dup
            | other @ Op::Dup2
            | other @ Op::ToStr
            | other @ Op::ToStrFmt(_)
            | other @ Op::BuildStr(_)
            | other @ Op::ListClone
            | other @ Op::ArrLen
            | other @ Op::HasNextHook
            | other @ Op::IsGenerator
            | other @ Op::IsCursor
            | other @ Op::IterableToCursor
            | other @ Op::IsChannel
            | other @ Op::ChanRecvOrClosed
            | other @ Op::EnsureEnum(_)
            | other @ Op::MatchNoArm(_)
            | other @ Op::EnterNursery
            | other @ Op::JoinNursery
            | other @ Op::ReclaimNursery
            | other @ Op::SpawnCall(..)
            | other @ Op::SpawnRecv(..)
            | other @ Op::SpawnBlock(..)
            | other @ Op::WaitPoll(_)
            | other @ Op::NewChannel(_)
            | other @ Op::NewShared
            | other @ Op::NewRwShared
            | other @ Op::NewAtomic
            | other @ Op::NewAtomicInt
            | other @ Op::NewTimer
            | other @ Op::NewExecutor
            | other @ Op::NewExecutorN => panic!("patch_jump_to on non-jump op: {other:?}"),
        }
    }

    pub(super) fn emit(&mut self, op: Op, span: Span) {
        self.code.push(op);
        self.lines.push(span);
    }

    /// Overwrite a previously-emitted op in place — used to back-patch `Op::WaitPoll`'s arm/else
    /// targets once the arm bodies have been laid out.
    pub(super) fn set_code(&mut self, at: usize, op: Op) {
        self.code[at] = op;
    }

    /// Emit a jump-like op, returning its index so it can be patched once the target is known.
    pub(super) fn emit_jump(&mut self, op: Op, span: Span) -> usize {
        let at = self.code.len();
        self.emit(op, span);
        at
    }

    /// The current instruction index — a jump target.
    pub(super) fn here(&self) -> usize {
        self.code.len()
    }

    /// Patch a previously-emitted jump to land at the current position.
    pub(super) fn patch_jump(&mut self, at: usize) {
        let target = self.code.len();
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other @ Op::ConstInt(_)
            | other @ Op::ConstFloat(_)
            | other @ Op::ConstStr(_)
            | other @ Op::ConstBytes(_)
            | other @ Op::True
            | other @ Op::False
            | other @ Op::Nil
            | other @ Op::Pop
            | other @ Op::StampGen(_)
            | other @ Op::Assert { .. }
            | other @ Op::LoadBuiltin(_)
            | other @ Op::GetLocal(_)
            | other @ Op::SetLocal(_)
            | other @ Op::GetGlobalSlot(_)
            | other @ Op::DefineGlobalSlot(_)
            | other @ Op::SetGlobalSlot(_)
            | other @ Op::GetCaptured(_)
            | other @ Op::Add
            | other @ Op::Sub
            | other @ Op::Mul
            | other @ Op::Div
            | other @ Op::Mod
            | other @ Op::Neg
            | other @ Op::Not
            | other @ Op::Lt
            | other @ Op::LtEq
            | other @ Op::Gt
            | other @ Op::GtEq
            | other @ Op::Eq
            | other @ Op::NotEq
            | other @ Op::BitAnd
            | other @ Op::BitOr
            | other @ Op::BitXor
            | other @ Op::Shl
            | other @ Op::Shr
            | other @ Op::Contains
            | other @ Op::AsBool
            | other @ Op::AsInt
            | other @ Op::BinLocalLocal { .. }
            | other @ Op::BinLocalConst { .. }
            | other @ Op::IncLocal { .. }
            | other @ Op::PopHandler
            | other @ Op::Call(_)
            | other @ Op::CallMethod { .. }
            | other @ Op::CallHook { .. }
            | other @ Op::CallStatic { .. }
            | other @ Op::CallStaticDyn { .. }
            | other @ Op::CallBuiltin(..)
            | other @ Op::CallPrint(_)
            | other @ Op::CallPrintSep { .. }
            | other @ Op::Return
            | other @ Op::Yield
            | other @ Op::DeferCall(_)
            | other @ Op::DeferMethod(..)
            | other @ Op::EnterDeferScope
            | other @ Op::LeaveDeferScope
            | other @ Op::DrainHandlerDefers
            | other @ Op::Try
            | other @ Op::JsonDecode(_)
            | other @ Op::JsonToValue
            | other @ Op::NewList(_)
            | other @ Op::NewTuple(_)
            | other @ Op::NewMap(_)
            | other @ Op::NewSet(_)
            | other @ Op::NewStruct(..)
            | other @ Op::NewCell
            | other @ Op::CellLoad
            | other @ Op::CellStore
            | other @ Op::NewEnum { .. }
            | other @ Op::MakeFunc(_)
            | other @ Op::MakeFuncIn(_)
            | other @ Op::MakeMethodFunc { .. }
            | other @ Op::JumpIfProvided(..)
            | other @ Op::MakeCffi(_)
            | other @ Op::MakeClosure(..)
            | other @ Op::GetField { .. }
            | other @ Op::GetIndex
            | other @ Op::SliceRange
            | other @ Op::SetField { .. }
            | other @ Op::SetIndex
            | other @ Op::Dup
            | other @ Op::Dup2
            | other @ Op::ToStr
            | other @ Op::ToStrFmt(_)
            | other @ Op::BuildStr(_)
            | other @ Op::ListClone
            | other @ Op::ArrLen
            | other @ Op::HasNextHook
            | other @ Op::IsGenerator
            | other @ Op::IsCursor
            | other @ Op::IterableToCursor
            | other @ Op::IsChannel
            | other @ Op::ChanRecvOrClosed
            | other @ Op::EnsureEnum(_)
            | other @ Op::MatchNoArm(_)
            | other @ Op::EnterNursery
            | other @ Op::JoinNursery
            | other @ Op::ReclaimNursery
            | other @ Op::SpawnCall(..)
            | other @ Op::SpawnRecv(..)
            | other @ Op::SpawnBlock(..)
            | other @ Op::WaitPoll(_)
            | other @ Op::NewChannel(_)
            | other @ Op::NewShared
            | other @ Op::NewRwShared
            | other @ Op::NewAtomic
            | other @ Op::NewAtomicInt
            | other @ Op::NewTimer
            | other @ Op::NewExecutor
            | other @ Op::NewExecutorN => panic!("patch_jump on non-jump op: {other:?}"),
        }
    }

    /// At global scope only when this is the toplevel proto and no block is open.
    pub(super) fn is_global_scope(&self) -> bool {
        self.is_toplevel && self.scope_depth == 0
    }

    pub(super) fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    pub(super) fn end_scope(&mut self) {
        self.scope_depth -= 1;
        while let Some(l) = self.locals.last() {
            if l.depth > self.scope_depth {
                self.locals.pop();
                self.slot_count -= 1;
            } else {
                break;
            }
        }
    }

    pub(super) fn next_slot(&self) -> usize {
        self.slot_count
    }

    /// Add a named local, returning its slot. A redeclaration in the same scope shadows by getting
    /// a fresh slot (later lookups find the newest).
    pub(super) fn add_local(&mut self, name: String) -> usize {
        self.add_local_claiming(name, crate::vm::crossing::SlotFresh::Marked)
    }

    /// TICKET-190 — [`Self::add_local`] for a declared param or a single-name `let`: the slot is
    /// private when the checker named `name` private in this generator frame. Every other binder
    /// claims `false`.
    pub(super) fn add_frame_local(&mut self, name: String) -> usize {
        let claim = self
            .gen_slots
            .get(&name)
            .copied()
            .unwrap_or(crate::vm::crossing::SlotFresh::Marked);
        self.add_local_claiming(name, claim)
    }

    fn add_local_claiming(&mut self, name: String, claim: crate::vm::crossing::SlotFresh) -> usize {
        use crate::vm::crossing::SlotFresh;
        let slot = self.slot_count;
        if self.slot_private.len() <= slot {
            self.slot_private.resize(slot + 1, None);
            self.slot_deep.resize(slot + 1, None);
        }
        self.slot_private[slot] =
            Some(self.slot_private[slot].unwrap_or(true) && claim != SlotFresh::Marked);
        self.slot_deep[slot] =
            Some(self.slot_deep[slot].unwrap_or(true) && claim == SlotFresh::All);
        self.locals.push(LocalVar {
            name,
            depth: self.scope_depth,
        });
        self.slot_count += 1;
        if self.slot_count > self.max_slots {
            self.max_slots = self.slot_count;
        }
        slot
    }

    /// A hidden compiler-temp slot (loop bookkeeping, match scrutinee) — no name to collide with.
    pub(super) fn add_hidden(&mut self) -> usize {
        self.add_local(String::new())
    }

    /// The ONE answer to "which of this frame's locals can a name see here", each paired with its
    /// slot (enumerate before skip, so the index stays the slot). Every name lookup and every
    /// capture snapshot iterates this; slot allocation (`add_local_claiming`, `end_scope`) and
    /// slot-indexed reads (`is_boxed_slot`, `emit_hidden_get`/`emit_hidden_set`) use `locals`
    /// directly because they are about slots, not names.
    fn visible_locals(&self) -> impl DoubleEndedIterator<Item = (usize, &LocalVar)> {
        self.locals.iter().enumerate().skip(self.name_floor)
    }

    /// Resolve a name to a local slot (innermost first). `None` ⇒ not a local.
    pub(super) fn resolve_local(&self, name: &str) -> Option<usize> {
        self.visible_locals()
            .rev()
            .find(|(_, l)| l.name == name)
            .map(|(slot, _)| slot)
    }

    pub(super) fn captures(&self, name: &str) -> bool {
        self.captured_names.iter().any(|n| n == name)
    }

    /// M24 — THE derivation of "is the hidden type witness for `t` reachable in this frame?": it is
    /// either this body's own trailing `$w:t` parameter, or — Task 4 — a capture appended by
    /// [`Compiler::with_witness_captures`] at the nested-body construction site. Nothing else can produce a
    /// `$w:` binding, so this answer IS the compiler's real capture behavior, which is what the
    /// checker's `witness_scope` predicate has to mirror.
    pub(super) fn witness_ref(&self, t: &str) -> Option<WitnessRef> {
        let name = witness_local(t);
        if let Some(slot) = self.resolve_local(&name) {
            return Some(WitnessRef::Local(slot));
        }
        self.captured_names
            .iter()
            .position(|n| *n == name)
            .map(|s| WitnessRef::Captured(s as u32))
    }

    /// Push a witness [`Self::witness_ref`] already resolved. Infallible on purpose: taking the
    /// resolved reference (never a name to re-look-up) makes "the guard said yes but nothing was
    /// pushed" — a wrong `argc` / an operand read as a witness — unrepresentable.
    pub(super) fn emit_witness(&mut self, w: WitnessRef, span: Span) {
        match w {
            WitnessRef::Local(slot) => self.emit_get_local_raw(slot, span),
            // The captured witness is the raw `str` (never boxed, because `$w:T` is unspellable and
            // so can never be in `boxed_names`), so NO trailing `CellLoad` — the one capture read
            // that legitimately skips it.
            WitnessRef::Captured(slot) => self.emit(Op::GetCaptured(slot), span),
        }
    }

    /// All bindings visible in this frame, to snapshot into a closure being created here. Locals
    /// (innermost wins) plus, if this frame is itself a closure, its captured names.
    pub(super) fn snapshot_entries(&self) -> Vec<CapEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        // Innermost local of each name wins; skip the hidden (unnamed) temps.
        for (slot, l) in self.visible_locals().rev() {
            if l.name.is_empty() || !seen.insert(l.name.clone()) {
                continue;
            }
            entries.push(CapEntry {
                name: l.name.clone(),
                src: CapSrc::Slot(slot),
            });
        }
        // A name not bound as a local here resolves against *this* frame's captured env (this frame
        // is itself a closure). Its enclosing-proto slot is its position in `self.captured_names`
        // (positional captures, lever #3) — stamp it so `MakeClosure` reads `captured[parent_slot]`.
        for (parent_slot, name) in self.captured_names.iter().enumerate() {
            if seen.insert(name.clone()) {
                entries.push(CapEntry {
                    name: name.clone(),
                    src: CapSrc::Captured(parent_slot as u32),
                });
            }
        }
        entries
    }
}
