//! Checker: loop variables, the frame entry, bound call slots, subscript keys and
//! module-scope names.
//! Split out of `src/checker/setup.rs`. `super::*` is the parent module.

use super::*;

impl Checker {
    /// Mark `name` (already declared in the current scope) as an immutable `for`-loop variable.
    pub(super) fn mark_loop_var(&mut self, name: &str) {
        if let Some(set) = self.loop_vars.last_mut() {
            set.insert(name.to_string());
        }
    }
    /// Is `name`'s nearest binding a `for`-loop variable? Resolves to the binding's defining scope
    /// so an inner `:=` shadow (a fresh local) is correctly reported as not-a-loop-var.
    pub(super) fn is_loop_var(&self, name: &str) -> bool {
        self.owning_scope(name)
            .is_some_and(|i| self.loop_vars[i].contains(name))
    }
    /// A fn or closure body is its own frame: it has a caller, so the blocks around its
    /// definition stop at its boundary. Returns the stack to restore.
    pub(super) fn enter_own_frame(&mut self) -> Vec<(boundary::Block, usize)> {
        std::mem::take(&mut self.blocks)
    }

    pub(super) fn exit_own_frame(&mut self, saved: Vec<(boundary::Block, usize)>) {
        self.blocks = saved;
    }
    /// TICKET-189 — the source of each declaration slot of call `call_id`, read from the one binder's
    /// plan ([`Self::bind_call`], `call_plans`), so keyword arguments, default fills and a variadic
    /// pack land in their compiled slot. With no plan the call is positional. A value callee's
    /// callee-filled trailing slots are absent from the plan: they are built inside the task.
    pub(super) fn bound_slots<'a>(
        &self,
        call_id: crate::ast::NodeId,
        args: &'a [Expr],
        named: &'a [(String, Expr)],
    ) -> Vec<fn_writes::SlotSrc<'a>> {
        let Some(plan) = self.call_plans.get(&(self.graph_module_idx, call_id.0)) else {
            return args.iter().map(fn_writes::SlotSrc::Arg).collect();
        };
        let combined = |i: usize| {
            args.get(i)
                .or_else(|| named.get(i - args.len()).map(|(_, v)| v))
        };
        plan.iter()
            .filter_map(|fill| match fill {
                crate::checker::ArgFill::Arg(i) => combined(*i).map(fn_writes::SlotSrc::Arg),
                // A pack with an index the call does not hold has no positional shape.
                crate::checker::ArgFill::Pack(idx) => Some(
                    idx.iter()
                        .map(|i| combined(*i))
                        .collect::<Option<Vec<_>>>()
                        .map_or(fn_writes::SlotSrc::Default { inline: false }, |elems| {
                            fn_writes::SlotSrc::Pack(elems)
                        }),
                ),
                crate::checker::ArgFill::Inline { .. } | crate::checker::ArgFill::Literal(_) => {
                    Some(fn_writes::SlotSrc::Default { inline: true })
                }
                crate::checker::ArgFill::Provider(_) => {
                    Some(fn_writes::SlotSrc::Default { inline: false })
                }
            })
            .collect()
    }

    pub(super) fn subscript_key(&self, index_span: Span) -> crate::checker::CarrierKey {
        crate::checker::carrier_key(
            self.graph_module_idx,
            self.kw_frag_ctx,
            self.kw_frag_ord,
            index_span,
        )
    }

    pub(super) fn is_fresh_subscript(&self, index_span: Span) -> bool {
        self.fresh_subscripts
            .contains(&self.subscript_key(index_span))
    }

    /// Does `name` resolve to the MODULE scope (index 0) — i.e. is it a module-level binding rather
    /// than a local/param shadow? Used by the from-imported-global rebind gate, so a fn-local `:=`
    /// shadow of an imported name stays assignable.
    pub(super) fn resolves_at_module_scope(&self, name: &str) -> bool {
        self.owning_scope(name) == Some(0)
    }

    // ===== pass 1: hoist declarations =====
}
