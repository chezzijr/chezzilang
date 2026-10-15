//! Compiler: capture and free-variable analysis: frame binds, boundary-free names, call sites,
//! `free_names_*`, witness needs.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;

/// Whether a block directly contains a `defer` statement (so it needs defer-scope brackets). Nested
/// blocks own their own scope, so they are NOT inspected here.
pub(super) fn block_has_defer(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s.kind, StmtKind::Defer(_)))
}

// ===== Uniform by-reference capture (Task A): the compile-time capture pre-pass. =====
// Purely syntactic, name-based over-approximation (spec B7). Computes THIS frame's local names that
// are captured by a directly-nested capture boundary (a closure body, `defer:` block, or `spawn:`
// block) so their slots must be boxed (`Obj::Cell`). Never DROPS a captured name (the free-var side
// is conservative); intersects with this frame's binds so only slots that live in this frame box.
// UNWIRED this task — nothing reads the result yet (emit-routing + boxing land in a later task).

/// Compute this frame's boxed-name set for `body` (its params are `params`). See the section comment.
pub(super) fn captured_names_of_body(
    body: &Block,
    params: &[crate::ast::Param],
) -> HashSet<String> {
    // 1. Free names of every DIRECTLY-nested capture boundary. The free-var walk descends THROUGH
    //    inner closures subtracting their binds, so a name captured two levels down still surfaces
    //    here (via the direct child's free set) — do NOT flat-collect deeper boundaries as direct
    //    children (that would leak a middle closure's own locals into this frame's set).
    let mut captured = HashSet::new();
    find_boundary_free_block(body, &mut captured);
    // 2. Names bound in THIS frame: params + every binding reachable through this body's control
    //    flow, but NOT inside a nested capture boundary (those are separate frames).
    let mut frame_binds: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    collect_frame_binds(body, &mut frame_binds);
    // 3. A boxed local is a captured name whose slot lives in THIS frame.
    captured.retain(|n| frame_binds.contains(n));
    captured
}

/// Names bound in THIS frame reachable through `stmts`'s control flow (descends if/for/while/match/
/// wait/parallel sub-blocks — those share the frame — but STOPS at capture boundaries, whose bodies
/// are separate frames). Adds let/for/match/wait/nested-fn binding names to `out`: a set for
/// "is it bound", a `Vec` for one entry per binding site.
pub(crate) fn collect_frame_binds(stmts: &[Stmt], out: &mut impl Extend<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { target, value, .. } => {
                out.extend(target.binder_names());
                // A binding inside the VALUE expression (a `match`/`if`/comprehension/`recover`
                // expression) is a frame-level local too — collect it so it boxes when captured.
                collect_frame_binds_expr(value, out);
            }
            // A nested named fn binds its name in this frame (its body is a separate frame).
            StmtKind::Fn(decl) => {
                out.extend([decl.name.clone()]);
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    collect_frame_binds_expr(cond, out);
                    collect_frame_binds(body, out);
                }
                if let Some(eb) = else_block {
                    collect_frame_binds(eb, out);
                }
            }
            StmtKind::For { target, iter, body } => {
                out.extend(target.binder_names());
                collect_frame_binds_expr(iter, out);
                collect_frame_binds(body, out);
            }
            StmtKind::While { cond, body } => {
                collect_frame_binds_expr(cond, out);
                collect_frame_binds(body, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                collect_frame_binds_expr(scrutinee, out);
                for arm in arms {
                    out.extend(arm.pattern.binder_names());
                    if let Some(g) = &arm.guard {
                        collect_frame_binds_expr(g, out);
                    }
                    collect_frame_binds(&arm.body, out);
                }
            }
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            collect_frame_binds_expr(chan, out);
                            out.extend(target.binder_names());
                        }
                        WaitArmKind::Send { call } => collect_frame_binds_expr(call, out),
                    }
                    collect_frame_binds(&arm.body, out);
                }
                if let Some(eb) = else_block {
                    collect_frame_binds(eb, out);
                }
            }
            StmtKind::Parallel { body } => collect_frame_binds(body, out),
            StmtKind::Assign { target, value, .. } => {
                collect_frame_binds_expr(target, out);
                collect_frame_binds_expr(value, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                collect_frame_binds_expr(e, out)
            }
            StmtKind::Assert { cond, msg } => {
                collect_frame_binds_expr(cond, out);
                if let Some(m) = msg {
                    collect_frame_binds_expr(m, out);
                }
            }
            // `defer f(x)` / `spawn f(x)` evaluate the call in THIS frame, so a binding inside the
            // call expression is a frame local. The BLOCK forms are separate frames (skipped).
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                collect_frame_binds_expr(e, out)
            }
            // `defer:` / `spawn:` blocks are SEPARATE frames — their binds are not this frame's.
            // Everything else binds no frame-local name.
            StmtKind::Struct { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Return(_)
            | StmtKind::Defer(_)
            | StmtKind::Spawn(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. } => {}
        }
    }
}

/// Walk `stmts`, and at each DIRECTLY-nested capture boundary (closure / `defer:` / `spawn:` block)
/// add that boundary's FREE names to `out`. Descends non-boundary structure to find boundaries, but
/// does NOT descend past a boundary (its transitive captures are already in its free set).
pub(super) fn find_boundary_free_block(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } => find_boundary_free_expr(value, out),
            StmtKind::Assign { target, value, .. } => {
                find_boundary_free_expr(target, out);
                find_boundary_free_expr(value, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                find_boundary_free_expr(e, out)
            }
            StmtKind::Assert { cond, msg } => {
                find_boundary_free_expr(cond, out);
                if let Some(m) = msg {
                    find_boundary_free_expr(m, out);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, body) in branches {
                    find_boundary_free_expr(c, out);
                    find_boundary_free_block(body, out);
                }
                if let Some(eb) = else_block {
                    find_boundary_free_block(eb, out);
                }
            }
            StmtKind::For { iter, body, .. } => {
                find_boundary_free_expr(iter, out);
                find_boundary_free_block(body, out);
            }
            StmtKind::While { cond, body } => {
                find_boundary_free_expr(cond, out);
                find_boundary_free_block(body, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                find_boundary_free_expr(scrutinee, out);
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        find_boundary_free_expr(g, out);
                    }
                    find_boundary_free_block(&arm.body, out);
                }
            }
            StmtKind::Parallel { body } => find_boundary_free_block(body, out),
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    match &arm.kind {
                        WaitArmKind::Recv { chan, .. } => find_boundary_free_expr(chan, out),
                        WaitArmKind::Send { call } => find_boundary_free_expr(call, out),
                    }
                    find_boundary_free_block(&arm.body, out);
                }
                if let Some(eb) = else_block {
                    find_boundary_free_block(eb, out);
                }
            }
            // `defer:` / `spawn:` blocks ARE capture boundaries — collect their free names (relative
            // to an empty scope) and stop. `defer f(x)` / `spawn f(x)` evaluate the call in THIS
            // frame, so scan the call expression for closure boundaries instead.
            StmtKind::Defer(DeferTarget::Block(b)) | StmtKind::Spawn(SpawnTarget::Block(b)) => {
                out.extend(free_names_of_block(b, &HashSet::new()))
            }
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                find_boundary_free_expr(e, out)
            }
            // A nested named fn IS a capture boundary (a closure-with-a-name): collect its body's
            // free names relative to its PARAMS ONLY — NOT its own name — so BOTH captured outer
            // locals AND the recursive self-name surface as free here. Intersected with this frame's
            // binds (which include the fn's name, via `collect_frame_binds`) → the captured names box
            // in THIS frame, exactly like a closure's captures. (Descending no further: the fn's own
            // inner boundaries capture ITS frame, already folded into its free set.)
            StmtKind::Fn(decl) => {
                let params: HashSet<String> = decl.params.iter().map(|p| p.name.clone()).collect();
                out.extend(free_names_of_block(&decl.body, &params));
            }
            // Remaining statements contain no capture boundary.
            StmtKind::Struct { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Return(_)
            | StmtKind::Break
            | StmtKind::Continue
            | StmtKind::Pass
            | StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. } => {}
        }
    }
}

/// The fragment expressions of an interpolated literal (`ExprKind::Interp`).
pub(crate) fn chunk_exprs(chunks: &[Chunk]) -> impl Iterator<Item = &Expr> {
    // The value, then the spec's nested width/precision fields — a name read ONLY inside a nested
    // field (`"{s:<{w}}"`) must still be seen as a free variable.
    chunks.iter().flat_map(|c| match c {
        Chunk::Expr(e, _, fields) => Some(e).into_iter().chain(fields.iter()),
        Chunk::Lit(_) => None.into_iter().chain([].iter()),
    })
}

/// Walk `e`, and at each closure boundary add its FREE names to `out` (stopping at the closure).
/// Descends every non-closure sub-expression to find closures nested in them.
pub(super) fn find_boundary_free_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Closure { params, body, .. } => {
            let bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
            out.extend(free_names_of_expr(body, &bound));
        }
        // A `{…}` fragment is an ordinary child (a closure could nest in one).
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                find_boundary_free_expr(ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_) => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| find_boundary_free_expr(x, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            find_boundary_free_expr(k, out);
            find_boundary_free_expr(v, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            for c in clauses {
                find_boundary_free_expr(&c.iter, out);
                c.guards
                    .iter()
                    .for_each(|g| find_boundary_free_expr(g, out));
            }
            if let Some(k) = key {
                find_boundary_free_expr(k, out);
            }
            find_boundary_free_expr(elem, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => find_boundary_free_expr(expr, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
            find_boundary_free_expr(lhs, out);
            find_boundary_free_expr(rhs, out);
        }
        ExprKind::Compare { operands, .. } => {
            for o in operands {
                find_boundary_free_expr(o, out);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                find_boundary_free_expr(start, out);
            }
            if let Some(end) = end {
                find_boundary_free_expr(end, out);
            }
        }
        ExprKind::Call {
            callee,
            args,
            named,
            bracket,
            ..
        } => {
            find_boundary_free_expr(callee, out);
            args.iter().for_each(|a| find_boundary_free_expr(a, out));
            bracket.iter().for_each(|b| find_boundary_free_expr(b, out));
            named
                .iter()
                .for_each(|(_, v)| find_boundary_free_expr(v, out));
        }
        ExprKind::Field { obj, .. } => find_boundary_free_expr(obj, out),
        ExprKind::OptChain { obj, call, .. } => {
            find_boundary_free_expr(obj, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| find_boundary_free_expr(a, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| find_boundary_free_expr(v, out));
            }
        }
        ExprKind::Index { obj, index, .. } => {
            find_boundary_free_expr(obj, out);
            if let Some(index) = index {
                find_boundary_free_expr(index, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            find_boundary_free_expr(scrutinee, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    find_boundary_free_expr(g, out);
                }
                find_boundary_free_expr(&arm.body, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            find_boundary_free_expr(cond, out);
            find_boundary_free_expr(then, out);
            find_boundary_free_expr(els, out);
        }
        ExprKind::Recover(block, _) => find_boundary_free_block(block, out),
        ExprKind::ElseGuard { value, body, .. } => {
            find_boundary_free_expr(value, out);
            find_boundary_free_block(body, out);
        }
    }
}

/// Keep only the capture entries whose name is FREE in a statement-block body (`bound` = the body's
/// own params). Used at the nested-fn / `spawn:` / `defer:` MakeClosure sites so a closure captures
/// only the names it references — not every visible local (Finding D: over-capture dragged unused
/// non-sendable siblings across the spawn airlock). Positional GetCaptured slot indices stay aligned
/// because both `MakeClosure`'s entries and the child's `captured_names` derive from THIS filtered vec.
///
/// Returns the walk alongside the entries: M24-2's witness question is asked of the SAME walk, never
/// a second one over the same body.
pub(super) fn filter_entries_free_block(
    entries: &[CapEntry],
    stmts: &[Stmt],
    params: &[crate::ast::Param],
) -> (Vec<CapEntry>, FreeNames) {
    let bound: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    let mut free = FreeNames::default();
    free_names_block(stmts, &bound, &mut free);
    let kept = entries
        .iter()
        .filter(|e| free.names.contains(&e.name))
        .cloned()
        .collect();
    (kept, free)
}

/// What a free-variable walk collects. `names` is the free-variable answer every capture consumer
/// wants. `calls` is the walk's second channel: the CALL STRUCTURE of every bare (`f(…)`) or
/// module-qualified (`m.f(…)`) call it passed, which M24's witness-forwarding charge asks
/// per-call-site (`Checker::witness_params_of`). A name alone cannot answer that question — naming
/// `lib` and having *some* member spelled `reset` is not a call to `lib.reset`, and a call whose
/// arguments are all concrete forwards nothing of the caller's own type param.
#[derive(Default)]
pub(crate) struct FreeNames {
    pub names: HashSet<String>,
    pub calls: Vec<CallSite>,
    /// True once the walk passed a call whose callee it could NOT put in `calls` — a method on a
    /// non-ident receiver (`a.b.c()`), an indexed one (`xs[i].m()`), a call result (`f()()`), a
    /// bound-name head. Such a call can still be a witness call: M24 Task 5 threads witnesses
    /// through MEMBERS too, and the checker keys those on the method-NAME TOKEN, a coordinate this
    /// syntactic walk does not carry. So the readers that must not UNDER-approximate
    /// ([`nested_body_needs_witness`]) treat it as "could be one".
    pub opaque_calls: bool,
    /// Every `<anything>.m(…)` / `<anything>?.m(…)` the walk passed — recorded for EVERY field-headed
    /// callee, including the ones that also land in `calls` (`lib.f(…)`, `T.default()`), since the
    /// head's MEANING is exactly what those two channels disagree about.
    /// `Checker::witness_params_of` uses it to charge a body whose only witness use is a MEMBER
    /// forward (`h.build[T](x)`), which appears in neither `names` (`T` is a type ARGUMENT) nor
    /// `calls` (the head is a receiver).
    ///
    /// Empty unless [`Self::record_members`] is set — only the checker reads this channel, and the
    /// compiler runs the same walk at every capture site.
    pub member_calls: Vec<MemberCall>,
    /// Opt in to [`Self::member_calls`]. Off by default so the compiler's per-capture-site walks —
    /// which read only `names` / `opaque_calls` / `calls` — allocate nothing for it.
    pub record_members: bool,
}

/// M24-2 — one MEMBER call (`recv.m(…)`), carrying the three things a PRE-TYPE walk can hand the
/// forwarding charge: the method NAME, the call's own TYPE ARGUMENTS, and every identifier occurring
/// anywhere in an argument expression. `Checker::member_call_forwards_a_witness` charges its
/// enclosing fn only when BOTH halves answer yes — this call site carries something of that fn's (a
/// type parameter in the turbofish `h.make[T](x)`, or a value parameter whose annotation mentions
/// one, `h.make(x)` with `x: T`) AND the NAME is declared as a witness-taking member somewhere in
/// the module graph. The name is a NECESSARY condition, never a sufficient one: alone it made one
/// unpinnable `get` poison every `m.get("a")` in the program; alone the call-site half charged
/// `sink.push(x)` on a BUILTIN `List`, which can never take a witness at all.
pub(crate) struct MemberCall {
    /// The method name (`h.make(x)` → `make`). Matched against the graph-wide index of method names
    /// that some declaration DOES take a witness for — a receiver whose method name is nowhere
    /// declared witness-taking is a builtin or a plain method, and cannot be a forward.
    pub name: String,
    /// The call's explicit type arguments (`h.make[T](x)` → `[T]`); empty for an inferred call and
    /// for `a?.m(…)`, which has no type-argument syntax.
    pub type_args: Vec<crate::ast::Type>,
    /// Every identifier occurring in any positional or named argument, at any depth and with NO
    /// bound-name subtraction — `h.make(f(xs[0]))` records `f`, `xs`. Depth and the empty bound set
    /// are both the CHARGE direction: a name that turns out to be a local rather than the enclosing
    /// fn's parameter merely over-charges, while missing one under-charges.
    pub arg_idents: Vec<String>,
}

/// One call the free-name walk passed, recorded only when the callee is a plain name or a
/// `module.name` pair (every other callee shape — a method receiver, an indexed value, a closure
/// result — records nothing and is invisible to the readers of this channel).
pub(crate) struct CallSite {
    /// `Some(m)` for `m.f(…)`, `None` for a bare `f(…)`.
    pub module: Option<String>,
    /// The callee name `f`.
    pub name: String,
    /// `Some(heads)` when EVERY argument (positional and named) is provably closed — a literal, or
    /// an ident-headed call whose own arguments are all closed — and `heads` lists those ident
    /// heads, so a reader can ask what each one names. `None` means "not provably closed", which is
    /// the DEFAULT for every shape not listed in [`closed_expr`]: an EMPTY argument list (a
    /// zero-arg generic call takes its type from the expected type, invisible here), an explicit
    /// turbofish (`f[T](1)` names the caller's own type param), an identifier, a field read, an
    /// operator — anything whose type this syntactic walk cannot pin.
    pub closed_arg_heads: Option<Vec<String>>,
    /// The call's explicit type arguments (`f[T](n)` → `[T]`); empty for an inferred call.
    pub type_args: Vec<crate::ast::Type>,
    /// How many positional arguments the call supplies, and the labels of its named ones: the
    /// parameters it fills. An omitted defaulted parameter pins no type parameter.
    pub supplied: (usize, Vec<String>),
}

/// Record `f(…)` / `m.f(…)` on `out`. A callee that is neither a free name nor a free name's field
/// is not recorded at all — it raises [`FreeNames::opaque_calls`] instead, so a reader that must not
/// under-approximate can see that SOME call went unclassified.
pub(super) fn record_call_site(
    callee: &Expr,
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: &[crate::ast::Type],
    bound: &HashSet<String>,
    out: &mut FreeNames,
) {
    let (module, name) = match &callee.kind {
        ExprKind::Ident(n) if !bound.contains(n) => (None, n.clone()),
        ExprKind::Field { obj, name, .. } => {
            // Every field-headed callee is a possible MEMBER call, whatever its head turns out to be.
            if out.record_members {
                out.member_calls
                    .push(member_call(name, args, named, type_args.to_vec()));
            }
            match &obj.kind {
                ExprKind::Ident(m) if !bound.contains(m) => (Some(m.clone()), name.clone()),
                ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Interp(_)
                | ExprKind::Bytes(_)
                | ExprKind::RawStr(_)
                | ExprKind::Bool(_)
                | ExprKind::NoneLit
                | ExprKind::Pass
                | ExprKind::Ident(_)
                | ExprKind::List(..)
                | ExprKind::Tuple(_)
                | ExprKind::Map(_)
                | ExprKind::Set(_)
                | ExprKind::Comprehension { .. }
                | ExprKind::Unary { .. }
                | ExprKind::Binary { .. }
                | ExprKind::Compare { .. }
                | ExprKind::Range { .. }
                | ExprKind::Call { .. }
                | ExprKind::Field { .. }
                | ExprKind::Index { .. }
                | ExprKind::Try(_)
                | ExprKind::OptChain { .. }
                | ExprKind::NullCoalesce { .. }
                | ExprKind::Closure { .. }
                | ExprKind::Match { .. }
                | ExprKind::IfElse { .. }
                | ExprKind::Recover(..)
                | ExprKind::ElseGuard { .. } => {
                    out.opaque_calls = true;
                    return;
                }
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_)
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => {
            out.opaque_calls = true;
            return;
        }
    };
    out.calls.push(CallSite {
        module,
        name,
        closed_arg_heads: closed_arg_heads(args, named, type_args, bound),
        type_args: type_args.to_vec(),
        supplied: (args.len(), named.iter().map(|(k, _)| k.clone()).collect()),
    });
}

/// Build a [`MemberCall`] from one member call site's name, arguments and type arguments.
pub(super) fn member_call(
    name: &str,
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: Vec<crate::ast::Type>,
) -> MemberCall {
    let nothing_bound = HashSet::new();
    let arg_idents = args
        .iter()
        .chain(named.iter().map(|(_, v)| v))
        .flat_map(|a| free_names_of_expr(a, &nothing_bound))
        .collect();
    MemberCall {
        name: name.to_string(),
        type_args,
        arg_idents,
    }
}

/// `Some(heads)` iff every argument is provably closed (see [`CallSite::closed_arg_heads`]).
pub(super) fn closed_arg_heads(
    args: &[Expr],
    named: &[(String, Expr)],
    type_args: &[crate::ast::Type],
    bound: &HashSet<String>,
) -> Option<Vec<String>> {
    if !type_args.is_empty() || (args.is_empty() && named.is_empty()) {
        return None;
    }
    let mut heads = Vec::new();
    for a in args.iter().chain(named.iter().map(|(_, v)| v)) {
        if !closed_expr(a, bound, &mut heads) {
            return None;
        }
    }
    Some(heads)
}

/// Is `e`'s type fixed by its own syntax, independent of any enclosing type parameter? Literals are
/// (`"a{x}"` is `str` whatever `x` is — and a call INSIDE the interpolation is its own [`CallSite`],
/// judged separately), and so is an ident-headed call whose arguments are all closed — its head is
/// pushed onto `heads` for the reader to identify. A head SHADOWED by a binding in scope is not the
/// declaration the reader would look up, so it is not closed. Everything else defaults to NOT closed
/// — a bare ident included, `nil` with it: `nil` has no value spelling in this language (`f(nil)`,
/// `x: int? = nil` and a `= nil` parameter default are all "unknown name 'nil'"), so an arm admitting
/// it could only ever loosen the charge for a program the checker already rejects.
pub(super) fn closed_expr(e: &Expr, bound: &HashSet<String>, heads: &mut Vec<String>) -> bool {
    match &e.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Str(_)
        | ExprKind::RawStr(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_) => true,
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
            ..
        } => match &callee.kind {
            ExprKind::Ident(n) if type_args.is_empty() && !bound.contains(n) => {
                heads.push(n.clone());
                args.iter()
                    .chain(named.iter().map(|(_, v)| v))
                    .all(|a| closed_expr(a, bound, heads))
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => false,
        },
        ExprKind::Ident(_)
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => false,
    }
}

/// M24-2 — can this nested body (closure, nested `fn`, `spawn:`/`defer:` block) REACH a witness for
/// the enclosing frame's type param `t`? `free` is the body's already-computed free-name walk;
/// `is_witness_fn` answers "could a call spelled `(module, name)` take hidden witness arguments?".
///
/// **The invariant: the compiler captures `$w:t` into a nested body whenever that body can reach a
/// witness for `t`.** `$w:` is unspellable (`$` is not an identifier character), so a body reaches a
/// witness only by CALLING something that consumes one, and every call the walk passes lands in
/// exactly one of the three channels below — so the disjunction is exhaustive over calls:
/// * `free.names` — a DIRECT `t.static(…)` parses as a `Field` on `Ident(t)`, so `t` is a free name;
/// * `free.opaque_calls` — a callee [`record_call_site`] could not classify at all (a non-ident
///   receiver, an indexed head, a call result, a name bound inside the body): always yes;
/// * `free.calls` — a classified `f(…)` / `head.f(…)`, put to `is_witness_fn`.
///
/// `is_witness_fn` (`Compiler::call_may_take_witnesses`) answers NO in exactly ONE case: a BARE-IDENT
/// callee `f(…)` whose resolved key is absent from [`crate::checker::WitnessTable::fns`] — i.e. a
/// callee the compiler itself would thread no witness into at a real call site. A bare ident is the
/// only head this walk resolves the way the emitter does, because a bare call carries no head to
/// misread.
///
/// EVERY field-headed call `X.f(…)` answers yes, whatever `X` turns out to name — an imported module,
/// a TYPE, a receiver value, or a module name shadowed by an enclosing binding. The walk cannot tell
/// those apart (its `bound` set holds only names bound INSIDE the body, and it sees no type
/// namespace), and a Task-5 MEMBER witness call is keyed on the method-name token, a coordinate it
/// does not carry, while naming `T` only as a type ARGUMENT, which is not a free name. Two earlier
/// cuts tried to classify the head and each shipped a check-ok/run-fault; not classifying it at all
/// is what makes the disjunction exhaustive rather than merely untested.
///
/// The narrowing that remains is therefore modest and worth stating exactly: a nested body keeps
/// every witness unless it does only arithmetic, indexing, literal/name work and bare calls to
/// non-witness fns. Any `x.m(…)`, `a.b.c()`, `xs[i].m()`, `f()()` or `a?.m()` keeps them all.
///
/// [`free_names_block`]/[`free_names_expr`] flatten every nested body's names AND calls into the
/// enclosing walk, so a `t.default()` two levels down keeps the outer capture alive. Hence
/// `FnComp::witness_ref` stays a SUPERSET of the checker's `witness_scope`, which is carried
/// unconditionally into nested bodies and needs no mirroring rule.
///
/// Being too narrow here is not a silent wrong value: [`FnComp::witness_ref`] returns `None` and the
/// compile fails loudly ("no type witness in scope to forward").
pub(crate) fn nested_body_needs_witness(
    free: &FreeNames,
    t: &str,
    is_witness_fn: &dyn Fn(Option<&str>, &str) -> bool,
) -> bool {
    free.names.contains(t)
        || free.opaque_calls
        || free
            .calls
            .iter()
            .any(|c| is_witness_fn(c.module.as_deref(), &c.name))
}

/// [`free_names_block`] for callers that want only the free-variable set.
pub(crate) fn free_names_of_block(stmts: &[Stmt], bound: &HashSet<String>) -> HashSet<String> {
    let mut f = FreeNames::default();
    free_names_block(stmts, bound, &mut f);
    f.names
}

/// [`free_names_expr`] for callers that want only the free-variable set.
pub(crate) fn free_names_of_expr(e: &Expr, bound: &HashSet<String>) -> HashSet<String> {
    let mut f = FreeNames::default();
    free_names_expr(e, bound, &mut f);
    f.names
}

/// Free names of a capture-boundary BLOCK body (`defer:`/`spawn:`): names referenced but not bound
/// within, relative to `bound`. Threads bindings left-to-right (a later stmt sees earlier lets).
pub(crate) fn free_names_block(stmts: &[Stmt], bound: &HashSet<String>, out: &mut FreeNames) {
    let mut b = bound.clone();
    for s in stmts {
        match &s.kind {
            StmtKind::Let { target, value, .. } => {
                free_names_expr(value, &b, out);
                b.extend(target.binder_names());
            }
            StmtKind::Assign { target, op, value } => {
                // TICKET-142 (W14-32): a plain `_ = e` target is the blank identifier, never a
                // variable — listing it would make a nested fn capture a non-existent `_`.
                if *op != AssignOp::Eq {
                    free_names_expr(target, &b, out);
                } else if let ExprKind::Tuple(ts) = &target.kind {
                    for t in ts.iter().filter(|t| !t.is_blank()) {
                        free_names_expr(t, &b, out);
                    }
                } else if !target.is_blank() {
                    free_names_expr(target, &b, out);
                }
                free_names_expr(value, &b, out);
            }
            StmtKind::Fn(decl) => {
                // Over-approximate: a nested fn body may reference an outer name → treat as free
                // (relative to its params + name), so any capture surfaces (never dropped).
                b.insert(decl.name.clone());
                let mut inner = b.clone();
                inner.extend(decl.params.iter().map(|p| p.name.clone()));
                free_names_block(&decl.body, &inner, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => {
                free_names_expr(e, &b, out)
            }
            StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Pass => {}
            StmtKind::Assert { cond, msg } => {
                free_names_expr(cond, &b, out);
                if let Some(m) = msg {
                    free_names_expr(m, &b, out);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, body) in branches {
                    free_names_expr(c, &b, out);
                    free_names_block(body, &b, out);
                }
                if let Some(eb) = else_block {
                    free_names_block(eb, &b, out);
                }
            }
            StmtKind::For { target, iter, body } => {
                free_names_expr(iter, &b, out);
                let mut b2 = b.clone();
                b2.extend(target.binder_names());
                free_names_block(body, &b2, out);
            }
            StmtKind::While { cond, body } => {
                free_names_expr(cond, &b, out);
                free_names_block(body, &b, out);
            }
            StmtKind::Match { scrutinee, arms } => {
                free_names_expr(scrutinee, &b, out);
                for arm in arms {
                    let mut b2 = b.clone();
                    b2.extend(arm.pattern.binder_names());
                    if let Some(g) = &arm.guard {
                        free_names_expr(g, &b2, out);
                    }
                    free_names_block(&arm.body, &b2, out);
                }
            }
            StmtKind::Parallel { body } => free_names_block(body, &b, out),
            StmtKind::Defer(DeferTarget::Block(blk)) | StmtKind::Spawn(SpawnTarget::Block(blk)) => {
                free_names_block(blk, &b, out)
            }
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                free_names_expr(e, &b, out)
            }
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    let mut b2 = b.clone();
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            free_names_expr(chan, &b, out);
                            b2.extend(target.binder_names());
                        }
                        WaitArmKind::Send { call } => free_names_expr(call, &b, out),
                    }
                    free_names_block(&arm.body, &b2, out);
                }
                if let Some(eb) = else_block {
                    free_names_block(eb, &b, out);
                }
            }
            // Type/import/native/extern decls inside a block reference no frame value.
            StmtKind::Struct { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Enum { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_)
            | StmtKind::Extern { .. }
            | StmtKind::Native(_)
            | StmtKind::NativeStruct { .. }
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. } => {}
        }
    }
}

/// Free names of an expression relative to `bound`: referenced idents not in `bound`. Descends
/// THROUGH nested closures/comprehensions/matches, subtracting each inner scope's binds.
pub(crate) fn free_names_expr(e: &Expr, bound: &HashSet<String>, out: &mut FreeNames) {
    match &e.kind {
        ExprKind::Ident(n) => {
            if !bound.contains(n) {
                out.names.insert(n.clone());
            }
        }
        // A `{…}` fragment references names too — collect their free names or a
        // capture-via-interpolation is missed.
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                free_names_expr(ie, bound, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| free_names_expr(x, bound, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            free_names_expr(k, bound, out);
            free_names_expr(v, bound, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            let mut b = bound.clone();
            for c in clauses {
                free_names_expr(&c.iter, &b, out);
                b.extend(c.target.binder_names());
                c.guards.iter().for_each(|g| free_names_expr(g, &b, out));
            }
            if let Some(k) = key {
                free_names_expr(k, &b, out);
            }
            free_names_expr(elem, &b, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => free_names_expr(expr, bound, out),
        ExprKind::Compare { operands, .. } => {
            for o in operands {
                free_names_expr(o, bound, out);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
            free_names_expr(lhs, bound, out);
            free_names_expr(rhs, bound, out);
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                free_names_expr(start, bound, out);
            }
            if let Some(end) = end {
                free_names_expr(end, bound, out);
            }
        }
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
            bracket,
        } => {
            record_call_site(callee, args, named, type_args, bound, out);
            free_names_expr(callee, bound, out);
            args.iter().for_each(|a| free_names_expr(a, bound, out));
            if let Some(b) = bracket {
                free_names_expr(b, bound, out);
            }
            named
                .iter()
                .for_each(|(_, v)| free_names_expr(v, bound, out));
        }
        ExprKind::Field { obj, .. } => {
            free_names_expr(obj, bound, out);
        }
        ExprKind::OptChain {
            obj, name, call, ..
        } => {
            free_names_expr(obj, bound, out);
            if let Some(c) = call {
                // `a?.m(…)` is a member call `record_call_site` never sees — and a member call is a
                // possible witness call (see [`FreeNames::opaque_calls`] / `member_calls`).
                out.opaque_calls = true;
                // No type-argument channel on an `?.` call (`a?.m[T]()` does not parse), and the
                // arguments are the ordinary ones — so the forward question is asked exactly as it
                // is for a plain member call.
                if out.record_members {
                    out.member_calls
                        .push(member_call(name, &c.args, &c.named, Vec::new()));
                }
                c.args.iter().for_each(|a| free_names_expr(a, bound, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| free_names_expr(v, bound, out));
            }
        }
        ExprKind::Index { obj, index, .. } => {
            free_names_expr(obj, bound, out);
            if let Some(index) = index {
                free_names_expr(index, bound, out);
            }
        }
        ExprKind::Closure { params, body, .. } => {
            let mut b = bound.clone();
            b.extend(params.iter().map(|p| p.name.clone()));
            free_names_expr(body, &b, out);
        }
        ExprKind::Match { scrutinee, arms } => {
            free_names_expr(scrutinee, bound, out);
            for arm in arms {
                let mut b = bound.clone();
                b.extend(arm.pattern.binder_names());
                if let Some(g) = &arm.guard {
                    free_names_expr(g, &b, out);
                }
                free_names_expr(&arm.body, &b, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            free_names_expr(cond, bound, out);
            free_names_expr(then, bound, out);
            free_names_expr(els, bound, out);
        }
        ExprKind::Recover(block, _) => free_names_block(block, bound, out),
        ExprKind::ElseGuard { value, err, body } => {
            free_names_expr(value, bound, out);
            let mut inner = bound.clone();
            inner.extend(err.iter().map(|(name, _)| name.clone()));
            free_names_block(body, &inner, out);
        }
    }
}

/// The closure/expression analogue of [`captured_names_of_body`]: compute the boxed-name set for a
/// closure whose body is the single expression `body` with parameters `params`. (A `fn(x): expr`
/// closure has no statement block — its frame binds come from `params` plus any `match`/`recover`/
/// comprehension binds inside the body expression.)
pub(super) fn captured_names_of_closure(
    body: &Expr,
    params: &[crate::ast::Param],
) -> HashSet<String> {
    let mut captured = HashSet::new();
    find_boundary_free_expr(body, &mut captured);
    let mut frame_binds: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    collect_frame_binds_expr(body, &mut frame_binds);
    captured.retain(|n| frame_binds.contains(n));
    captured
}

/// Names bound in THIS frame by an EXPRESSION body (`match`/`recover`/comprehension/if-else binds),
/// descending non-closure sub-expressions but STOPPING at a nested closure (a separate frame).
pub(super) fn collect_frame_binds_expr(e: &Expr, out: &mut impl Extend<String>) {
    match &e.kind {
        // A nested closure is a separate frame — its binds are not this frame's.
        ExprKind::Closure { .. } => {}
        // A `{…}` interpolation expr may itself bind (a `match`/comprehension expression); its binds
        // land in this frame (the interpolation is compiled inline).
        ExprKind::Interp(chunks) => {
            for ie in chunk_exprs(chunks) {
                collect_frame_binds_expr(ie, out);
            }
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_) => {}
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            es.iter().for_each(|x| collect_frame_binds_expr(x, out))
        }
        ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
            collect_frame_binds_expr(k, out);
            collect_frame_binds_expr(v, out);
        }),
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            for c in clauses {
                collect_frame_binds_expr(&c.iter, out);
                out.extend(c.target.binder_names());
                c.guards
                    .iter()
                    .for_each(|g| collect_frame_binds_expr(g, out));
            }
            if let Some(k) = key {
                collect_frame_binds_expr(k, out);
            }
            collect_frame_binds_expr(elem, out);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => collect_frame_binds_expr(expr, out),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
            collect_frame_binds_expr(lhs, out);
            collect_frame_binds_expr(rhs, out);
        }
        ExprKind::Compare { operands, .. } => {
            for o in operands {
                collect_frame_binds_expr(o, out);
            }
        }
        ExprKind::Range { start, end, .. } => {
            if let Some(start) = start {
                collect_frame_binds_expr(start, out);
            }
            if let Some(end) = end {
                collect_frame_binds_expr(end, out);
            }
        }
        ExprKind::Call {
            callee,
            args,
            named,
            bracket,
            ..
        } => {
            collect_frame_binds_expr(callee, out);
            args.iter().for_each(|a| collect_frame_binds_expr(a, out));
            bracket
                .iter()
                .for_each(|b| collect_frame_binds_expr(b, out));
            named
                .iter()
                .for_each(|(_, v)| collect_frame_binds_expr(v, out));
        }
        ExprKind::Field { obj, .. } => collect_frame_binds_expr(obj, out),
        ExprKind::OptChain { obj, call, .. } => {
            collect_frame_binds_expr(obj, out);
            if let Some(c) = call {
                c.args.iter().for_each(|a| collect_frame_binds_expr(a, out));
                c.named
                    .iter()
                    .for_each(|(_, v)| collect_frame_binds_expr(v, out));
            }
        }
        ExprKind::Index { obj, index, .. } => {
            collect_frame_binds_expr(obj, out);
            if let Some(index) = index {
                collect_frame_binds_expr(index, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            collect_frame_binds_expr(scrutinee, out);
            for arm in arms {
                out.extend(arm.pattern.binder_names());
                if let Some(g) = &arm.guard {
                    collect_frame_binds_expr(g, out);
                }
                collect_frame_binds_expr(&arm.body, out);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            collect_frame_binds_expr(cond, out);
            collect_frame_binds_expr(then, out);
            collect_frame_binds_expr(els, out);
        }
        ExprKind::Recover(block, _) => collect_frame_binds(block, out),
        ExprKind::ElseGuard { value, err, body } => {
            collect_frame_binds_expr(value, out);
            out.extend(err.iter().map(|(name, _)| name.clone()));
            collect_frame_binds(body, out);
        }
    }
}
