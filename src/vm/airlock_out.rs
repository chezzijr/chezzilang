//! VM: the airlock outbound: `start_task`, deep clone, `to_wire` and the `WalkSrc` walker.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::wire::{WireMap, WireSet};
use super::*;

impl Vm {
    /// The copy walk's source for this `Vm`'s own heap.
    pub(super) fn walk_src(&self) -> WalkSrc<'_> {
        WalkSrc {
            heap: &self.heap,
            module_objs: &self.module_objs,
            program: &self.program,
            walk_base: self.walk_base,
        }
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context: push the callee's
    /// frame and return; the scheduler then runs it. The callee always pushes a frame (TICKET-235,
    /// [`Vm::task_entry`]), so a call that waits inside the task has an `ip` to rewind.
    pub(super) fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
                // Sample the `--max-heap` cap before dispatch (see `sample_mem_cap`). The callee and
                // args are plain Rust locals here, so they are NOT rooted — park them on the operand
                // stack (which `collect` traces) across the sample and take them back after. Under a
                // live cap only, so the uncapped path is byte-for-byte what it was.
                let (callee, args) = if self.heap.mem_cap() != 0 {
                    let argc = args.len();
                    self.push(callee);
                    for a in args {
                        self.push(a);
                    }
                    self.sample_mem_cap(span)?;
                    let args = self.stack.split_off(self.stack.len() - argc);
                    (self.pop(), args)
                } else {
                    (callee, args)
                };
                // TICKET-235: `Vm::task_entry` gave every callable head that pushes no frame an
                // entry thunk. A task queued any other way stops here, not at a park's rewind.
                // (A non-callable callee falls through to `invoke_value`'s own fault.)
                if self.callable(callee).is_some() && !self.enters_frame(callee) {
                    return Err(self.err(
                        "internal error: a task must start in a Chezzi frame".to_string(),
                        span,
                    ));
                }
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
        }
    }

    /// [`to_wire_at`](Vm::to_wire_at) against a CALLER-OWNED [`WireMemo`], so several roots that cross
    /// together share one serialization. A serialize memo's lifetime must equal its rebuild map's
    /// (`from_wire_memo`), or a `Backref` minted under one memo hits the other's empty map.
    pub(super) fn to_wire_memo_at(
        &self,
        v: Value,
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        self.to_wire_depth(v, 0, memo)
            .map_err(|e| self.err(e.message, span))
    }

    /// `to_wire` re-stamped with a real call-site `span`. `to_wire`'s `Err`s (a generator HARD ARM —
    /// suspended mid-`recover:`, a parked `defer`, a non-sendable parked slot — or a recursive local
    /// `fn`) carry a placeholder `Span{0,0}`; every method-level airlock site (`Channel.send`/
    /// `Shared.set`/`Atomic.store`/…) has a real span, so route through this so the catchable error
    /// reports the operation's location rather than line 0.
    #[cfg(test)]
    pub(super) fn to_wire_at(&self, v: Value, span: Span) -> Result<WireValue, RuntimeError> {
        self.to_wire(v).map_err(|e| self.err(e.message, span))
    }

    /// B3.0 — serialize a value into its [`WireValue`] form (the airlock's outbound half). A
    /// read-only walk of the heap, structurally identical to `deep_clone`'s old recursion but
    /// allocating nothing. Data (list/tuple/map/set/struct/enum) recurses; immutable / by-reference
    /// objects (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as
    /// [`WireValue::Handle`] (the existing handle, same heap in B3.0). `Map`/`Set` carry their
    /// cached hashes through so reconstruction never re-hashes.
    ///
    /// Every `Value` and every `Obj` variant maps to a wire arm — by-reference objects (callables,
    /// modules, `Channel`/`Shared`/`Executor`) cross as `Handle`. The ONE fallible arm is a
    /// frame-holding **generator** (`Obj::Generator`): its parked frames reference this heap, so it
    /// is not sendable and returns a graceful `a generator cannot be sent across tasks` error here
    /// (carrying a placeholder `Span{0,0}` that callers re-stamp with the real site via
    /// `to_wire_at`). Every other arm is infallible (the `?` only forwards the
    /// generator error up through container recursion).
    #[cfg(test)]
    pub(super) fn to_wire(&self, v: Value) -> Result<WireValue, RuntimeError> {
        // Fresh memo per root — correct for a SINGLE-root serialize. Roots that belong together
        // must instead share one memo via `to_wire_memo_at`, or two sibling closures over the same
        // local arrive as two cells (W7-4). TICKET-100: within a
        // memo, containers, closures and cells are ALL preserved for the whole crossing — one
        // source object produces one copy per crossing (see [`WireMemo`]).
        let mut memo = WireMemo::default();
        self.to_wire_depth(v, 0, &mut memo)
    }

    /// [`WalkSrc::to_wire_depth`] over this `Vm`'s own heap.
    #[inline]
    pub(super) fn to_wire_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        self.walk_src().to_wire_depth(v, depth, memo)
    }
}

/// Identity-preservation state threaded through [`Vm::to_wire_depth`] — every identity-preserved node
/// kind (`Obj::Cell`/`Obj::Closure` AND the container arms `List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/
/// `Iter`) records its GcRef here so ANY value cycle round-trips: a recursive local `fn`'s
/// letrec self-cell, a mutually-recursive closure pair, a self-referential struct/list/map, or a mixed
/// struct+closure cycle. `path` maps each such GcRef **currently on the serialize DFS stack** to the
/// `id` assigned on its first visit; `next_id` is the monotonic id counter. On a REVISIT of a node still
/// in `path` (a true back-edge) the arm emits `WireValue::Backref(id)` and stops.
///
/// TICKET-100 — one source object now produces ONE copy per crossing, for data, closures and cells
/// alike. `path` is popped on DFS exit, but `nodes` (never popped) also records every identity-preserved
/// node reached anywhere in this crossing, so an OFF-stack DAG alias back-references too, exactly like
/// an on-stack cycle: `b := a` inside a task means the same thing it means outside one, and CPython's
/// `copy.deepcopy` agrees (it memoizes by source identity). A `Generator` is the sole remaining
/// unpreserved container on the DFS stack (its parked frame is mid-serialization, so it cannot be a
/// `Backref` target while it is being written). A cycle re-entering the SAME generator on the DFS stack
/// is REJECTED cleanly (`gens_on_stack`): with the containers now back-referencing, the depth cap no
/// longer trips on such a cycle, and re-serializing the generator would silently DUPLICATE it — the
/// e8dcad7 wrong-result class. TICKET-137: a later OFF-stack reach (two module globals or two list
/// slots holding the same generator) is a `Backref` to the id its first reach minted in `nodes`, so
/// the crossing holds ONE copy with the alias intact. An acyclic parked slot too deep still trips the
/// depth cap.
///
/// TICKET-154 — do NOT re-emit container or cell definitions per depth-1 subtree to make pieces
/// self-contained: that is O(n²) wire size. (M3.3: the `RwShared` views read the live value by
/// handle and encode nothing.)
#[derive(Default)]
pub(super) struct WireMemo {
    /// GcRef of an identity-preserved node (`Closure`/container) currently on the serialize DFS
    /// stack → the `id` assigned on its first visit. A revisit while still in `path` is a true
    /// back-edge → `Backref(id)`. Popped on DFS exit (`WireMemo::exit`); the off-stack case is
    /// covered by `nodes` below, not by `path` itself.
    pub(super) path: super::fxhash::FxHashMap<GcRef, u32>,
    /// TICKET-100 — GcRef of every identity-preserved node (`Closure`/container) seen ANYWHERE in this
    /// crossing → its `id`. NEVER popped, so an off-stack DAG alias also back-references: one source
    /// object now produces one copy per crossing, not one copy per DFS path to it. See
    /// `WireMemo::seen`/`enter`.
    pub(super) nodes: super::fxhash::FxHashMap<GcRef, u32>,
    /// W7-4 — GcRef of every `Obj::Cell` seen ANYWHERE in this serialization → its `id`. Never
    /// removed: a cell is a binding, so reaching it again (off-stack sibling closure, or on-stack
    /// letrec back-edge) emits `Backref` and the far side
    /// rebuilds exactly one cell per binding. Scope discipline: a serialize memo's lifetime must equal
    /// its `from_wire_memo` rebuild map's, or a `Backref` minted under one memo hits the other's
    /// `.expect` — see [`Vm::to_wire_memo_at`].
    pub(super) cells: super::fxhash::FxHashMap<GcRef, u32>,
    pub(super) next_id: u32,
    /// GcRefs of `Obj::Generator`s currently on the serialize DFS stack. Re-entering one still on the
    /// stack is a cycle through a node whose parked frame is mid-serialization → reject (never
    /// duplicate). Removed on DFS exit. An OFF-stack revisit is a `Backref` through `nodes`
    /// (TICKET-137), not a reject.
    pub(super) gens_on_stack: super::fxhash::FxHashSet<GcRef>,
}

impl WireMemo {
    /// TICKET-100 — the id `h` already crosses under: an on-stack back-edge (`path`) first, else an
    /// off-stack DAG alias already seen this crossing (`nodes`).
    fn seen(&self, h: GcRef) -> Option<u32> {
        self.path
            .get(&h)
            .copied()
            .or_else(|| self.nodes.get(&h).copied())
    }

    /// TICKET-100 — record `h`'s first visit: on the DFS stack, and in `nodes` so a later off-stack
    /// reach still back-references.
    fn enter(&mut self, h: GcRef, id: u32) {
        self.path.insert(h, id);
        self.nodes.insert(h, id);
    }

    /// TICKET-100 — pop `h` off the DFS stack on exit. `nodes` is never popped.
    fn exit(&mut self, h: GcRef) {
        self.path.remove(&h);
    }

    /// The id to mint for a node's FIRST reach this crossing.
    fn mint_node(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// TICKET-246 — everything the copy walk reads, borrowed. The walk is `&self` over these fields and
/// nothing else ([`Vm::walk_src`]). It never charges a heap.
pub(super) struct WalkSrc<'a> {
    pub(super) heap: &'a Heap,
    pub(super) module_objs: &'a super::module_table::ModuleTable,
    pub(super) program: &'a Program,
    pub(super) walk_base: usize,
}

impl<'a> WalkSrc<'a> {
    fn err(&self, message: String, span: Span) -> RuntimeError {
        super::exec::plain_err(message, span)
    }

    fn depth_exceeded_err(&self, span: Span) -> RuntimeError {
        super::exec::depth_exceeded(span)
    }

    /// B3.3c — the index of a `home` module `GcRef` in the source's `module_objs`, so the receiver
    /// can resolve the corresponding rebuilt module. `None` for a home not in the table.
    pub(super) fn home_index(&self, home: GcRef) -> Option<usize> {
        self.module_objs.index_of(home)
    }

    /// Resolve a struct instance's `tid` to its type key. Returns `"?"` for
    /// [`crate::base::op::TID_NONE`] / an out-of-range id.
    pub(super) fn struct_name_of_tid(&self, tid: u32) -> &'a str {
        self.program
            .struct_names
            .get(tid as usize)
            .map_or("?", |s| s)
    }
}

/// The handle-reject backstop of a test worker's wired result ([`ReadyWorker::run`]).
#[cfg(test)]
pub(super) fn ensure_crossable_wire(w: &WireValue, span: Span) -> Result<(), RuntimeError> {
    if w.has_handle() {
        return Err(super::exec::plain_err(
            "this task value can't cross a worker boundary — plain data, closures/functions, \
             native/FFI fns, and Channel/Shared/Executor handles are sendable, but a module handle cannot cross"
                .to_string(),
            span,
        ));
    }
    Ok(())
}

impl WalkSrc<'_> {
    /// Depth-counted worker behind [`Vm::to_wire`]. A **self-referential** sendable (e.g. a struct
    /// whose field points back at itself) now ROUND-TRIPS via the identity-preserving id + `Backref`
    /// machinery (every container arm + `Cell`/`Closure`), so it never recurses unbounded. This depth
    /// bound remains as the backstop for a genuinely-unbounded ACYCLIC nest (which would otherwise
    /// recurse on the HOST stack and `SIGABRT`, uncatchable) — the SAME `MAX_STRUCTURAL_DEPTH` limit
    /// and message the display / `==` paths use (`stmt.rs` / `arith.rs`), turned into a recoverable
    /// `RuntimeError` (placeholder `Span{0,0}`, re-stamped with the real airlock site by `to_wire_at`).
    fn to_wire_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        #[cfg(test)]
        super::WIRE_ENCODE_NODES.with(|c| c.set(c.get() + 1));
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::default()));
        }
        Ok(match v.view() {
            ValueView::Int(n) => WireValue::Int(n),
            ValueView::Bool(b) => WireValue::Bool(b),
            ValueView::Nil => WireValue::Nil,
            // A boxed float is Float-tagged → `view` yields `Obj(h)` → the `Obj::FloatBox` arm below
            // maps it to `WireValue::Float` (a boxed `BigInt` likewise → `WireValue::Int`).
            ValueView::Obj(h) => match self.heap.get(h) {
                // B3.3a: `str` crosses by value (owned bytes) so it can survive an OS-thread heap
                // boundary; immutable + value-compared, so a fresh handle on reconstruction is
                // observationally identical to sharing this one.
                Obj::Str(s) => WireValue::Str(s.as_str().into()),
                // Boxed scalars cross by value, exactly like the inline `Int`/`Float` arms above —
                // `from_wire` re-inlines (or re-boxes, once Phase 1 is live) on the far heap.
                Obj::BigInt(n) => WireValue::Int(*n),
                Obj::FloatBox(f) => WireValue::Float(*f),
                // `bytes` crosses by value (owned raw bytes), exactly like `str` — immutable +
                // value-compared, so a fresh handle on reconstruction is observationally identical.
                Obj::Bytes(b) => WireValue::Bytes(b.clone()),
                // `bytearray` crosses by value as a DEEP COPY (a fresh independent buffer on the other
                // side, like `list`) — never a shared mutable view. `from_wire` rebuilds a new heap
                // `bytearray`, so cross-thread mutation never aliases.
                Obj::ByteArray(b) => WireValue::ByteArray(b.to_vec().into_boxed_slice()),
                // A first-class builtin fn crosses the airlock BY VALUE (its name) — pure code, no
                // `GcRef` and no captured heap state, so it genuinely crosses an OS-thread boundary
                // (unlike a `Func`/`Closure` handle). `from_wire` re-allocs a fresh `Obj::Builtin`;
                // builtins are name-compared, so that is observationally identical.
                Obj::Builtin(b) => WireValue::Builtin(*b),
                // B3.3: a closure crosses the airlock BY VALUE — its `proto` (shared via
                // `Arc<Program>`), its captures wired recursively in slot order (paired with the
                // proto's `capture_names`), and its `home` as an index into `module_objs` — never a
                // heap-local `GcRef`.
                //
                // Identity preservation: a recursive local `fn`'s letrec self-cell makes this
                // closure's capture graph cyclic (`Closure h -> captured[Cell] -> Cell.inner = h`,
                // or the `Closure_f -> Cell_g -> Closure_g -> Cell_f` mutual-recursion loop). We
                // assign `h` an `id` and record it via `memo.enter` BEFORE recursing captures; a
                // nested revisit of `h` (the back-edge) emits `WireValue::Backref(id)` and stops —
                // `from_wire` ties the knot back. TICKET-100: `h` is popped off `path` on exit but
                // kept in `nodes`, so an off-stack alias also back-references.
                Obj::Closure {
                    proto,
                    captured,
                    home,
                } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.enter(h, id);
                        let names = &self.program.protos[*proto].capture_names;
                        let mut wcap = Vec::with_capacity(captured.len());
                        for (i, cv) in captured.iter().enumerate() {
                            let w = self.to_wire_depth(cv, depth + 1, memo)?;
                            let name = names.get(i).cloned().unwrap_or_default();
                            wcap.push((name.into_boxed_str(), w));
                        }
                        memo.exit(h);
                        WireValue::Closure {
                            id,
                            proto: *proto,
                            captured: wcap,
                            home: self.home_index(*home),
                        }
                    }
                }
                // B3.3: a bare fn crosses BY VALUE as its own distinct `Func` arm (NOT an
                // empty-capture `Closure`), so it keeps rendering `<fn NAME>` after a round-trip (a
                // `Closure` renders `<closure>`).
                Obj::Func { proto, home } => WireValue::Func {
                    proto: *proto,
                    home: self.home_index(*home),
                },
                // A module's mutable globals genuinely can't cross an OS-thread heap boundary — its
                // `GcRef` is meaningless on the receiver heap — so it stays a by-reference `Handle`
                // (the worker airlock rejects it; source-unreachable, defensive only).
                Obj::Module(_) => WireValue::Handle(h),
                // A native fn is pure code (fn ptr + name, no `GcRef`) and a Cffi is a shared `Arc`
                // — both cross BY VALUE like `Builtin`. `has_handle` leaves both `false`, so the
                // worker airlock accepts them.
                Obj::Native { name, func, kind } => WireValue::Native {
                    name: name.clone(),
                    func: *func,
                    kind: *kind,
                },
                Obj::Cffi(c) => WireValue::Cffi(Arc::clone(c)),
                // B3.1: the shared cores cross as the `Arc` itself (clone = refcount bump), so a
                // `from_wire` in any heap reaches the same mailbox/box/queue.
                Obj::Channel(core) => WireValue::Channel(Arc::clone(core)),
                Obj::Shared(core) => WireValue::Shared(Arc::clone(core)),
                Obj::RwShared(core) => WireValue::RwShared(Arc::clone(core)),
                Obj::Atomic(core) => WireValue::Atomic(Arc::clone(core)),
                Obj::AtomicInt(core) => WireValue::AtomicInt(Arc::clone(core)),
                Obj::Executor(core) => WireValue::Executor(Arc::clone(core)),
                // D6: a socket/listener handle crosses as its shared `Arc` core (a spawned fiber
                // reaches the same fd) — same shape as `Channel`/`Shared`/`Executor`.
                Obj::Socket(core) => WireValue::Socket(Arc::clone(core)),
                Obj::Listener(core) => WireValue::Listener(Arc::clone(core)),
                // R2: a `Writer` handle crosses as its shared `Arc` core (a spawned fiber reaches the
                // same output) — same shape as `Socket`. Cross-task write ordering is unspecified.
                Obj::Writer(core) => WireValue::Writer(Arc::clone(core)),
                // R2b: a `Reader` handle crosses as its shared `Arc` core (a spawned fiber reaches the
                // same fd) — same shape as `Writer`. Cross-task read ordering is unspecified.
                Obj::Reader(core) => WireValue::Reader(Arc::clone(core)),
                // An opaque `ptr` handle crosses by value — its raw address is heap-independent, so a
                // fresh `Obj::Ptr` on the other side is observationally identical (immutable +
                // value-compared). Cross-safe for both the serial and M:N engines.
                Obj::Ptr(a) => WireValue::Ptr(*a),
                // Identity-preserved container (see the `Obj::Cell`/`Obj::Closure` arms): assign `h` an
                // `id` and record it via `memo.enter` BEFORE recursing, so a self-referential list
                // (`xs.push(xs)`) or any cycle passing through it back-references instead of overflowing
                // the depth cap. TICKET-100: removed from `path` on DFS exit but kept in `nodes`, so an
                // off-stack alias back-references too.
                Obj::List(items) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::List { id, items: out }
                    }
                }
                Obj::Tuple(items) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::Tuple { id, items: out }
                    }
                }
                Obj::Map(m) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(m.entries.len());
                        for (hash, k, val) in &m.entries {
                            out.push((
                                hash,
                                self.to_wire_depth(k, depth + 1, memo)?,
                                self.to_wire_depth(val, depth + 1, memo)?,
                            ));
                        }
                        memo.exit(h);
                        WireValue::Map {
                            id,
                            entries: WireMap::new(out),
                        }
                    }
                }
                Obj::Set(s) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(s.entries.len());
                        for (hash, e) in &s.entries {
                            out.push((hash, self.to_wire_depth(e, depth + 1, memo)?));
                        }
                        memo.exit(h);
                        WireValue::Set {
                            id,
                            entries: WireSet::new(out),
                        }
                    }
                }
                // Identity-preserved container (see `Obj::List`): a self-referential struct
                // (`a.next = b; b.next = a`) back-references instead of overflowing the cap.
                Obj::Struct { tid, fields, .. } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        // Positional layout: recover the declaration-order field names from the
                        // StructDef (cold cross-task path) so the WireValue encoding is unchanged.
                        // The instance carries only `tid`; resolve the type key from it (owned — the
                        // wire format still carries the name string, receiver re-derives its tid).
                        let name: Box<str> = self.struct_name_of_tid(*tid).into();
                        let names: Vec<Box<str>> = self
                            .program
                            .structs
                            .get(name.as_ref())
                            .map(|d| {
                                d.fields
                                    .iter()
                                    .map(|f| f.clone().into_boxed_str())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let vals: Vec<Value> = fields.to_vec();
                        let mut out = Vec::with_capacity(vals.len());
                        for (i, val) in vals.iter().enumerate() {
                            let k = names
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| i.to_string().into_boxed_str());
                            out.push((k, self.to_wire_depth(*val, depth + 1, memo)?));
                        }
                        memo.exit(h);
                        WireValue::Struct {
                            id,
                            name,
                            fields: out,
                        }
                    }
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(payload.len());
                        for x in payload {
                            out.push(self.to_wire_depth(x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        // M19 lever #2 — carry the dense `variant_id` directly on the COLD wire path. All
                        // workers share one `Arc<Program>`, so the id is meaningful on both sides;
                        // carrying it (not the name) preserves native-vs-user identity under shadowing.
                        WireValue::Enum {
                            id,
                            variant_id: *variant_id,
                            payload: out,
                        }
                    }
                }
                // A frame-holding generator crosses the airlock BY VALUE as a DEEP COPY: its `proto`
                // (shared via `Arc<Program>`), `home` index, backing closure, and lifecycle state, with
                // every parked slot wired recursively so a non-sendable slot rejects AT SERIALIZE TIME.
                // Both a FRAME-LOCAL generator (F3 path C — direct `spawn`/`Channel.send` crossing) and a
                // MODULE-GLOBAL generator (backlog item B — via `to_snap`'s slow arm, which re-enters
                // here) take this same by-value path; the old reach-gate + Option-B poison→Nil model is
                // retired. The single-frame Suspended invariant is load-bearing (the checker resets `in_generator`
                // at every fn/closure boundary, so a suspension always parks exactly the generator body
                // frame); a mid-`recover:` handler, a pending `defer`, or >1 parked frame is a HARD ARM
                // rejected cleanly below (never mis-serialized).
                Obj::Generator(g) => {
                    // TICKET-041(a) — `generator_next` parks `GenState::Done` as a placeholder in the
                    // heap object for the DURATION of a resume (`src/vm/exec.rs:510`), so a generator
                    // crossed WHILE it is running would otherwise serialize that placeholder verbatim
                    // and hand the receiver a copy that reports itself as already exhausted. Reject at
                    // the serializer instead: the same oracle `generator_next`'s own re-entrancy guard
                    // already applies (`src/vm/exec.rs:497`, "generator already running"). CPython
                    // agrees — `copy.deepcopy(gen)` on a running generator raises
                    // `TypeError: cannot pickle 'generator' object` (measured 2026-09-03).
                    // M2.4 — a run holds the parked box while the generator runs (on this thread
                    // or another); the copy below is what this crossing serializes.
                    let Some(parked) = g.parked.with(|p| p.cloned()) else {
                        return Err(self.err(
                            "a running generator cannot be sent across tasks".to_string(),
                            Span::default(),
                        ));
                    };
                    // A cycle re-entering THIS generator on the DFS stack closes a value cycle through a
                    // node whose parked frame cannot be a mid-rebuild `Backref` target → reject cleanly
                    // (re-serializing it would silently DUPLICATE the generator, the e8dcad7
                    // wrong-result class). Insert BEFORE recursing; remove on exit. A recursive closure
                    // PARKED in the generator is still fine: its self-cell cycle is identity-preserved
                    // and back-refs.
                    if memo.gens_on_stack.contains(&h) {
                        return Err(self.err(
                            "a generator cannot be sent across tasks as part of a reference cycle"
                                .to_string(),
                            Span::default(),
                        ));
                    }
                    // TICKET-137 — a generator reached a second time OFF the stack (two module globals,
                    // or two list slots, aliasing one generator) is ONE copy per crossing: `nodes` holds
                    // the id its first reach minted and `from_wire_memo` registered, so the alias is a
                    // `Backref`.
                    if let Some(id) = memo.nodes.get(&h).copied() {
                        return Ok(WireValue::Backref(id));
                    }
                    memo.gens_on_stack.insert(h);
                    let id = memo.mint_node();
                    memo.nodes.insert(h, id);
                    let home = self.home_index(g.home);
                    let closure = match g.closure {
                        Some(c) => Some(Box::new(self.to_wire_depth(
                            Value::obj(c),
                            depth + 1,
                            memo,
                        )?)),
                        None => None,
                    };
                    let state = match &parked.state {
                        GenState::Pending(args) => {
                            let mut wargs = Vec::with_capacity(args.len());
                            for a in args {
                                wargs.push(self.to_wire_depth(*a, depth + 1, memo)?);
                            }
                            // TICKET-190: the creation stamp rides along; the rebuild arm reads it.
                            WireGenState::Pending(wargs, g.stamp.get())
                        }
                        GenState::Done => WireGenState::Done,
                        GenState::Suspended => {
                            // CHECKER-UNREACHABLE HARD ARMS — reject cleanly rather than silently
                            // mis-serialize. Neither shape can arise from checker-valid source; these
                            // are defensive guards against the type-blind compiler path.
                            //  - multi-frame (a): `yield` only fires in the generator's own body frame
                            //    (`in_generator` resets at every fn/closure boundary), so a suspended
                            //    generator always has exactly one frame.
                            //  - pending `defer` (c): `defer` is banned inside a generator body.
                            // A mid-`recover:` suspension (arm b) is NO LONGER rejected — a live
                            // handler stack is pure plain-data and now serialized below.
                            if parked.ctx.frames.len() != 1 {
                                return Err(self.err(
                                    "a generator suspended across more than one call frame cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            let frame = &parked.ctx.frames[0];
                            if !frame.deferred.is_empty() {
                                return Err(self.err(
                                    "a generator suspended with a pending `defer` cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            // The sole body frame's home/closure equal the core's by construction
                            // (`push_frame(proto, g.home, g.closure, …)`); assert defensively and
                            // reject rather than wire a mismatched frame.
                            if frame.home != g.home || frame.closure != g.closure {
                                return Err(self.err(
                                    "a generator with an inconsistent parked frame cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            let mut wstack = Vec::with_capacity(parked.ctx.stack.len());
                            for v in &parked.ctx.stack {
                                wstack.push(self.to_wire_depth(*v, depth + 1, memo)?);
                            }
                            // Backlog arm (b): carry the live `recover:` handlers. Each `Handler` is
                            // `Copy` plain-data (all `usize`, no `GcRef`/`Value`), so it crosses as-is
                            // with no value recursion; its indices address the frame/stack serialized
                            // above and are reconstructed coherently in `from_wire`.
                            WireGenState::Suspended {
                                frame: Box::new(WireCallFrame {
                                    proto: frame.proto,
                                    ip: frame.ip,
                                    base: frame.base,
                                    counted: frame.counted,
                                    is_toplevel: frame.is_toplevel,
                                    defer_markers: frame.defer_markers.clone(),
                                    nursery_len: frame.nursery_len,
                                    has_implicit_nursery: frame.has_implicit_nursery,
                                    call_span: frame.call_span,
                                    argc: frame.argc,
                                }),
                                stack: wstack,
                                call_depth: parked.ctx.call_depth,
                                cur_base: parked.ctx.cur_base,
                                handlers: parked.ctx.handlers.clone(),
                                stamp: g.stamp.get(),
                            }
                        }
                    };
                    // Off the DFS stack; `nodes` keeps `h`, so a later reach is a `Backref`.
                    memo.gens_on_stack.remove(&h);
                    WireValue::Generator {
                        id,
                        proto: g.proto,
                        home,
                        closure,
                        state,
                    }
                }
                // A `Cell` (a by-reference-captured local's box) crosses the airlock by value as a
                // DEEP COPY: wire its inner value; `from_wire` rebuilds a FRESH independent cell,
                // so a plain captured local sent into a task is an isolated copy (design §4 F1) —
                // ONE fresh cell per BINDING, not per reference.
                //
                // W7-4: unlike every other arm this uses `memo.cells`, which is never popped. A
                // cell is a binding's identity, so ANY later reach (an off-stack sibling closure —
                // `Ctr(inc, get)` over one local `n` — as well as an on-stack
                // letrec/mutual-recursion back-edge) emits `Backref(id)` and the far side ties both
                // references to the one rebuilt cell. TICKET-100: data containers now keep identity
                // the same way, via the separate `nodes` map (see [`WireMemo`]) — a cell and a
                // container are two cases of one rule. The `id` is stable per cell for the whole
                // serialization: its first reach writes the definition, every later reach a
                // `Backref`.
                Obj::Cell(v) => {
                    if let Some(&id) = memo.cells.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.cells.insert(h, id);
                        let inner = self.to_wire_depth(v.get(), depth + 1, memo)?;
                        WireValue::Cell {
                            id,
                            inner: Box::new(inner),
                        }
                    }
                }
                // A cursor crosses by value as a DEEP COPY (like `List`): wire each snapshot item and
                // carry `pos`. It is plain data (a `Vec` + index), so — unlike a generator — it is
                // genuinely sendable, and `from_wire` rebuilds an independent cursor on the other side.
                // `deep_clone` already deep-copies a cursor the same way across the airlock for
                // `spawn`, so there is no separate gate here. Recursing through items means a cursor
                // over a non-sendable element faults recoverably, like a `list` of that element would.
                Obj::Iter { items, pos } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node();
                        memo.enter(h, id);
                        let pos = pos.get();
                        let items = items.to_vec();
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::Iter {
                            id,
                            items: out,
                            pos,
                        }
                    }
                }
            },
        })
    }
}
