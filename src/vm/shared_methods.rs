//! VM: Shared, RwShared, Atomic and Executor methods, executor drains, `finish_run`.
//! Split out of `src/vm/netio.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::BinaryOp;
use crate::base::builtin_method::{
    AtomicIntMethod, AtomicMethod, ExecutorMethod, RwSharedMethod, RwSharedPrim, SharedMethod,
};

impl Vm {
    /// `Shared[T]` methods: `get`, `set`, `update(f)`, `read(f)`. M3.3: the box holds ONE value by
    /// handle; every method takes the box's guard (its lock, [`Vm::take_update_guard`]) and works
    /// on the live value in this task. The receiver and the arguments are popped by the dispatcher,
    /// so they are rooted on the operand stack ([`Vm::with_method_rooted`]) across the guard's wait
    /// and the closure call.
    pub(super) fn shared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.with_method_rooted(h, args, |vm| vm.shared_method_rooted(h, method, args, span))
    }

    /// Run a box method `f` with its popped receiver `h` and `args` rooted on the operand stack.
    /// M1.8 — the receiver is a box's root: the box value is reachable through its handles only
    /// (`Heap::children`), so a method that waits for the guard detached (`Vm::gc_blocking`) keeps
    /// its handle in this `Vm`'s published roots.
    fn with_method_rooted<T>(
        &mut self,
        h: GcRef,
        args: &[Value],
        f: impl FnOnce(&mut Vm) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let mut roots = Vec::with_capacity(args.len() + 1);
        roots.push(Value::obj(h));
        roots.extend_from_slice(args);
        self.with_roots(&roots, f)
    }

    /// Take the guard `lock` in `mode`, as this task.
    fn box_guard<'a>(
        &mut self,
        lock: &'a Arc<core::BoxLock>,
        mode: GuardMode,
        span: Span,
    ) -> Result<core::UpdateGuard<'a>, RuntimeError> {
        let what = match mode {
            GuardMode::Write => "a Shared/RwShared write guard",
            GuardMode::Read => "a RwShared read guard",
        };
        self.take_update_guard(GuardKey { lock, mode }, what, span)
    }

    /// Run the closure `f` of a box method on the live value `cur`, in this task, under the guard
    /// the caller holds. A blocking op inside `f` demotes the worker (`Vm::reentered`), so the
    /// guard stays with this task until `f` returns.
    fn box_closure(&mut self, f: Value, cur: Value, span: Span) -> Result<Value, RuntimeError> {
        self.reentered(|vm| vm.invoke_value(f, vec![cur], span))
    }

    fn shared_method_rooted(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = SharedMethod::from_name(method) else {
            return Err(self.err(format!("type Shared has no method '{method}'"), span));
        };
        let core = self.shared_core(h);
        let lock = &core.lock;
        // EVERY arm takes the guard first (TICKET-016 / W8-3, the process-global wait-for graph),
        // so a same-box re-entry from the box's own closure FAULTS (the length-1 cycle) for `get` /
        // `read` / `set` / `update` alike, and a cross-box wait cycle faults.
        match which {
            SharedMethod::Get => {
                self.arity_err("get", args, 0, span)?;
                let _guard = self.box_guard(lock, GuardMode::Write, span)?;
                Ok(core.v.get())
            }
            SharedMethod::Set => {
                self.arity_err("set", args, 1, span)?;
                let _guard = self.box_guard(lock, GuardMode::Write, span)?;
                core.v.set(args[0]);
                Ok(Value::nil())
            }
            // `update(f)`: `f`'s result becomes the value; `read(f)` returns it. A fault leaves the
            // value as `f` left it.
            SharedMethod::Update | SharedMethod::Read => {
                self.arity_err(method, args, 1, span)?;
                let _guard = self.box_guard(lock, GuardMode::Write, span)?;
                let r = self.box_closure(args[0], core.v.get(), span)?;
                if which == SharedMethod::Update {
                    core.v.set(r);
                    return Ok(Value::nil());
                }
                Ok(r)
            }
        }
    }

    /// `RwShared[T]` methods. M3.3: one value by handle, behind a two-mode guard.
    ///
    /// - `write(f)` / `set` and the single-entry writers (`_set_key`, `_remove_key`,
    ///   `_get_or_insert`, `_add`, `_remove`, `_push`) take the WRITE guard and work on the live
    ///   value ([`Vm::rwshared_mutate`]): a writer is the plain `Map` / `Set` / `List` operation.
    /// - `get`, `read(f)` and the read views (`_len`, `_at`, `_slice`, `_contains`, `_has`,
    ///   `_get_key`) take the READ guard ([`Vm::rwshared_read`]) and return values by handle.
    ///   `read`'s closure, and a key's `eq`, run under it: a `write` nested inside them faults
    ///   (`already holds the read guard`).
    ///
    /// The receiver and the arguments are popped by the dispatcher, so they are rooted here across
    /// every user call (`hash`, `eq`, the closure).
    pub(super) fn rwshared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.with_method_rooted(h, args, |vm| {
            vm.rwshared_method_rooted(h, method, args, span)
        })
    }

    /// The `Option` value a RwShared map primitive returns (`_set_key` the value it replaced,
    /// `_remove_key` the value it removed, `_at` / `_get_key` the value found).
    fn option_of(&mut self, v: Option<Value>) -> Value {
        match v {
            Some(v) => self.alloc_enum("Option", "Some", &[v]),
            None => self.alloc_enum("Option", "None", &[]),
        }
    }

    /// The ONE reader of a `RwShared` value: run `pick` on the live value under the read guard.
    fn rwshared_read<R>(
        &mut self,
        core: &Arc<RwSharedCore>,
        span: Span,
        pick: impl FnOnce(&mut Vm, Value) -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        let _guard = self.box_guard(&core.lock, GuardMode::Read, span)?;
        let cur = core.v.get();
        // The reader's own list (`Vm::held_reads`), read on a failed acquire to find a re-entry.
        let held = self.held_reads.len();
        self.held_reads.push(Arc::as_ptr(&core.lock) as usize);
        let r = pick(self, cur);
        self.held_reads.truncate(held);
        r
    }

    /// The ONE single-entry writer of a `RwShared` container: run `op` on the live value under the
    /// write guard. `op` is the plain `Map` / `Set` / `List` operation.
    fn rwshared_mutate<R>(
        &mut self,
        core: &Arc<RwSharedCore>,
        span: Span,
        op: impl FnOnce(&mut Vm, Value) -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        let _guard = self.box_guard(&core.lock, GuardMode::Write, span)?;
        let cur = core.v.get();
        op(self, cur)
    }

    /// `"list"` / `"map"` / `"set"` for a container value, else `""`.
    fn elem_kind(&self, v: Value) -> &'static str {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::List(_)) => "list",
            Some(Obj::Map(_)) => "map",
            Some(Obj::Set(_)) => "set",
            _ => "",
        }
    }

    fn rwshared_method_rooted(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let core = self.rwshared_core(h);
        let want_elem = |vm: &Vm, st: Value, want: &'static str| {
            if vm.elem_kind(st) == want {
                Ok(st.as_obj().expect("a container is a heap object"))
            } else {
                Err(vm.err(format!("RwShared.{method} requires a {want} element"), span))
            }
        };
        if let Some(which) = RwSharedMethod::from_name(method) {
            return match which {
                RwSharedMethod::Get => {
                    self.arity_err("get", args, 0, span)?;
                    self.rwshared_read(&core, span, |_, cur| Ok(cur))
                }
                RwSharedMethod::Read => {
                    self.arity_err("read", args, 1, span)?;
                    self.rwshared_read(&core, span, |vm, cur| vm.box_closure(args[0], cur, span))
                }
                RwSharedMethod::Set => {
                    self.arity_err("set", args, 1, span)?;
                    self.rwshared_mutate(&core, span, |_, _| {
                        core.v.set(args[0]);
                        Ok(Value::nil())
                    })
                }
                // `write(f)` runs `f` on the live value, exclusive; its result becomes the value.
                RwSharedMethod::Write => {
                    self.arity_err("write", args, 1, span)?;
                    self.rwshared_mutate(&core, span, |vm, cur| {
                        let r = vm.box_closure(args[0], cur, span)?;
                        core.v.set(r);
                        Ok(Value::nil())
                    })
                }
            };
        }
        let Some(prim) = RwSharedPrim::from_name(method) else {
            return Err(self.err(format!("type RwShared has no method '{method}'"), span));
        };
        match prim {
            RwSharedPrim::SetKey => {
                self.arity_err(method, args, 2, span)?;
                let qh = self.hash_value(args[0], span)?;
                let replaced = self.rwshared_mutate(&core, span, |vm, st| {
                    let mh = want_elem(vm, st, "map")?;
                    vm.map_upsert_in_heap(mh, qh, args[0], args[1], span)
                })?;
                Ok(self.option_of(replaced))
            }
            RwSharedPrim::RemoveKey => {
                self.arity_err(method, args, 1, span)?;
                let qh = self.hash_value(args[0], span)?;
                let removed = self.rwshared_mutate(&core, span, |vm, st| {
                    let mh = want_elem(vm, st, "map")?;
                    let Some(i) = vm.map_probe(mh, qh, args[0], span)? else {
                        return Ok(None);
                    };
                    let heap = vm.heap.write(mh);
                    let Obj::Map(m) = heap.get(mh) else {
                        unreachable!()
                    };
                    Ok(m.remove_at(heap, i).map(|e| e.2))
                })?;
                Ok(self.option_of(removed))
            }
            // Compound-atomic: the probe and the insert run under ONE write guard.
            RwSharedPrim::GetOrInsert => {
                self.arity_err(method, args, 2, span)?;
                let qh = self.hash_value(args[0], span)?;
                self.rwshared_mutate(&core, span, |vm, st| {
                    let mh = want_elem(vm, st, "map")?;
                    if let Some(v) = vm.map_lookup(mh, qh, args[0], span)? {
                        return Ok(v);
                    }
                    let k = vm.snapshot_key(args[0]);
                    let heap = vm.heap.write(mh);
                    let Obj::Map(m) = heap.get(mh) else {
                        unreachable!()
                    };
                    m.push(heap, qh, k, args[1]);
                    Ok(args[1])
                })
            }
            RwSharedPrim::Add | RwSharedPrim::Remove => {
                self.arity_err(method, args, 1, span)?;
                let qh = self.hash_value(args[0], span)?;
                let add = prim == RwSharedPrim::Add;
                let changed = self.rwshared_mutate(&core, span, |vm, st| {
                    let sh = want_elem(vm, st, "set")?;
                    let pos = vm.set_probe(sh, qh, args[0], span)?;
                    let e = vm.snapshot_key(args[0]);
                    let heap = vm.heap.write(sh);
                    let Obj::Set(s) = heap.get(sh) else {
                        unreachable!()
                    };
                    Ok(match pos {
                        None if add => {
                            s.push(heap, qh, e);
                            true
                        }
                        Some(i) if !add => s.remove_at(heap, i).is_some(),
                        _ => false,
                    })
                })?;
                Ok(Value::bool(changed))
            }
            RwSharedPrim::Push => {
                self.arity_err(method, args, 1, span)?;
                self.rwshared_mutate(&core, span, |vm, st| {
                    let lh = want_elem(vm, st, "list")?;
                    let heap = vm.heap.write(lh);
                    let Obj::List(items) = heap.get(lh) else {
                        unreachable!()
                    };
                    items.push(heap, args[0]);
                    Ok(())
                })?;
                Ok(Value::nil())
            }
            RwSharedPrim::Len => {
                self.arity_err(method, args, 0, span)?;
                let n = self.rwshared_read(&core, span, |vm, st| {
                    match st.as_obj().map(|h| vm.heap.get(h)) {
                        Some(Obj::List(items)) => Ok(items.len()),
                        Some(Obj::Map(m)) => Ok(m.entries.len()),
                        Some(Obj::Set(s)) => Ok(s.entries.len()),
                        _ => Err(vm.err("RwShared._len requires a container element".into(), span)),
                    }
                })?;
                Ok(self.make_int(n as i64))
            }
            // `at(i) -> Option[E]` — out of range is `None`, not a fault (the safe accessor, like
            // `get_key`). A negative index counts from the end (`norm_index`).
            RwSharedPrim::At => {
                self.arity_err(method, args, 1, span)?;
                let i = self.int_of(args[0]);
                let got = self.rwshared_read(&core, span, |vm, st| {
                    match st.as_obj().map(|h| vm.heap.get(h)) {
                        Some(Obj::List(items)) => {
                            Ok(crate::slice::norm_index(i, items.len()).map(|u| items.get(u)))
                        }
                        _ => Err(vm.err("RwShared._at requires a list element".into(), span)),
                    }
                })?;
                Ok(self.option_of(got))
            }
            // `slice(r)` returns a new list of the selected elements, by handle. The `Range` is
            // resolved against the length under the guard, with the same `range_indices` every
            // other slice site calls.
            RwSharedPrim::Slice => {
                self.arity_err(method, args, 1, span)?;
                let Some(parts) = self.as_range(args[0]) else {
                    return Err(self.err("RwShared._slice requires a Range".into(), span));
                };
                self.rwshared_read(&core, span, |vm, st| {
                    let picked: Vec<Value> = match st.as_obj().map(|h| vm.heap.get(h)) {
                        Some(Obj::List(items)) => {
                            let idxs = crate::slice::range_indices(&parts, items.len())
                                .map_err(|e| vm.err(e.to_string(), span))?;
                            idxs.into_iter().map(|i| items.get(i)).collect()
                        }
                        _ => {
                            return Err(
                                vm.err("RwShared._slice requires a list element".into(), span)
                            );
                        }
                    };
                    Ok(Value::obj(vm.heap.alloc(Obj::List(picked.into()))))
                })
            }
            // The probes: the hash runs first, then the one heap probe under the read guard (a
            // user `eq` runs there, as under Go's `RLock`).
            RwSharedPrim::Contains => {
                self.arity_err(method, args, 1, span)?;
                let qh = self.hash_value(args[0], span)?;
                let hit = self.rwshared_read(&core, span, |vm, st| {
                    let sh = want_elem(vm, st, "set")?;
                    vm.set_probe(sh, qh, args[0], span)
                })?;
                Ok(Value::bool(hit.is_some()))
            }
            RwSharedPrim::Has => {
                self.arity_err(method, args, 1, span)?;
                let qh = self.hash_value(args[0], span)?;
                let hit = self.rwshared_read(&core, span, |vm, st| {
                    let mh = want_elem(vm, st, "map")?;
                    vm.map_probe(mh, qh, args[0], span)
                })?;
                Ok(Value::bool(hit.is_some()))
            }
            RwSharedPrim::GetKey => {
                self.arity_err(method, args, 1, span)?;
                let qh = self.hash_value(args[0], span)?;
                let got = self.rwshared_read(&core, span, |vm, st| {
                    let mh = want_elem(vm, st, "map")?;
                    vm.map_lookup(mh, qh, args[0], span)
                })?;
                Ok(self.option_of(got))
            }
        }
    }

    /// `Atomic[T]` methods: `load`, `store`, `exchange` (swap, returns old), `cas(expected, new) ->
    /// bool` (swap iff the box equals `expected`), `add`/`sub` (numeric RMW, returns the new value).
    /// M3.2: the box holds the value itself, so a store and a load copy one word (Go
    /// `atomic.Value`). Each method is a single lock-op-unlock, so the RMW is atomic across threads —
    /// no user closure runs under the lock (unlike `Shared.update`), so no `update_lock` is needed.
    /// `add`/`sub` use the language's `checked_add`/`checked_sub` (int overflow faults, like the
    /// `+`/`-` operators) and plain float arithmetic.
    pub(super) fn atomic_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = AtomicMethod::from_name(method) else {
            return Err(self.err(format!("type Atomic has no method '{method}'"), span));
        };
        match which {
            AtomicMethod::Load => {
                self.arity_err("load", args, 0, span)?;
                Ok(self.atomic_core(h).load())
            }
            AtomicMethod::Store => {
                self.arity_err("store", args, 1, span)?;
                self.atomic_core(h).store(args[0]);
                Ok(Value::nil())
            }
            AtomicMethod::Exchange => {
                self.arity_err("exchange", args, 1, span)?;
                let core = self.atomic_core(h);
                let old = std::mem::replace(&mut *core.lock(), args[0]);
                Ok(old)
            }
            AtomicMethod::Cas => {
                self.arity_err("cas", args, 2, span)?;
                let core = self.atomic_core(h);
                // Hold the value lock across compare+swap so the CAS is atomic. `values_equal`
                // borrows `self`, not the guard (which borrows the cloned `Arc`), so the lock can
                // stay held while it runs. M3.0a / Q5: another thread may write the compared object
                // meanwhile — a racy result, no UB.
                let mut g = core.lock();
                let cur = *g;
                // TICKET-144 / TICKET-177: a payload whose `==` is slot identity (a fn value, an
                // iterator) can never compare equal to a fresh `load()`, so `false` would spin the CAS
                // retry loop forever — Go's `atomic.Value.CompareAndSwap` panics on the same shape.
                // Every fn value is refused, a builtin included (TICKET-264); `Vm::slot_identity_in`
                // reads `Vm::callable` and `Obj::identity`. A handle payload compares by core and is
                // accepted. (`g` drops on the early return, leaving the box unchanged.)
                if let Some(what) = self.slot_identity_in(cur) {
                    return Err(self.err(
                        format!("Atomic.cas: the payload holds {what}, which cas cannot compare"),
                        span,
                    ));
                }
                // Propagate a cyclic-operand depth fault (`?`) instead of swallowing it — consistent
                // with `==` and every container membership site. The `?` runs BEFORE the store, so a
                // fault leaves the box unchanged (the lock guard `g` drops on the early return).
                //
                // This is the ONE equality site that stays STRUCTURAL: the compare runs under
                // the value lock so the compare-and-swap is atomic, and a user `eq` re-entering the
                // VM here could touch the same `Atomic` and deadlock a non-reentrant mutex. The
                // checker rejects a payload type that REACHES a user `eq` (`reject_eq_atomic_payload`),
                // but that walk cannot see through a `Protocol` existential or an unresolved type
                // param — so the property is ENFORCED here, by turning the hook off for the window,
                // instead of being asserted from the checker's exhaustiveness. Cleared on the next
                // statement (no `?` in between), so an `Err` compare cannot leave it stuck on.
                self.eq_hook_off = true;
                let cmp = self.values_equal_guarded(cur, args[0], 0, span);
                self.eq_hook_off = false;
                let swapped = cmp?;
                if swapped {
                    *g = args[1];
                }
                Ok(Value::bool(swapped))
            }
            AtomicMethod::Add | AtomicMethod::Sub => {
                self.arity_err(method, args, 1, span)?;
                let delta = args[0];
                let core = self.atomic_core(h);
                let mut g = core.lock();
                let cur = *g;
                // An int or a boxed float result allocates under the lock: an allocation reaches no
                // safepoint, so no collection waits for this lock.
                let new = if self.is_integral(cur) && self.is_integral(delta) {
                    let op = if which == AtomicMethod::Add {
                        BinaryOp::Add
                    } else {
                        BinaryOp::Sub
                    };
                    let n = self.int_op(op, self.int_of(cur), self.int_of(delta), span)?;
                    self.make_int(n)
                } else if cur.is_float() && delta.is_float() {
                    let (a, b) = (self.float_of(cur), self.float_of(delta));
                    self.box_float(if which == AtomicMethod::Add {
                        a + b
                    } else {
                        a - b
                    })
                } else {
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    return Err(self.err(format!("type Atomic has no method '{method}'"), span));
                };
                *g = new;
                Ok(new)
            }
        }
    }

    /// `AtomicInt` methods: `load`, `store`, `exchange`, `cas(expected, new) -> bool`, `add`/`sub`
    /// (returns the NEW value). Backed by a raw lock-free `AtomicI64` — every op uses `SeqCst` ordering
    /// (matches the sequential consistency `Atomic`'s Mutex gave — every op still appears to happen
    /// in some single global order).
    /// `add`/`sub` KEEP the i64-overflow fault via a CHECKED `compare_exchange` CAS-loop (NOT raw
    /// `fetch_add`/`fetch_sub`, which wrap silently) — error string byte-identical to `atomic_method`'s.
    pub(super) fn atomic_int_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use std::sync::atomic::Ordering::SeqCst;
        let Some(which) = AtomicIntMethod::from_name(method) else {
            return Err(self.err(format!("type AtomicInt has no method '{method}'"), span));
        };
        match which {
            AtomicIntMethod::Load => {
                self.arity_err("load", args, 0, span)?;
                let cur = self.atomic_int_core(h).v.load(SeqCst);
                Ok(self.make_int(cur))
            }
            AtomicIntMethod::Store => {
                self.arity_err("store", args, 1, span)?;
                let n = self.int_of(args[0]);
                self.atomic_int_core(h).v.store(n, SeqCst);
                Ok(Value::nil())
            }
            AtomicIntMethod::Exchange => {
                self.arity_err("exchange", args, 1, span)?;
                let n = self.int_of(args[0]);
                let old = self.atomic_int_core(h).v.swap(n, SeqCst);
                Ok(self.make_int(old))
            }
            AtomicIntMethod::Cas => {
                self.arity_err("cas", args, 2, span)?;
                let expected = self.int_of(args[0]);
                let new = self.int_of(args[1]);
                let swapped = self
                    .atomic_int_core(h)
                    .v
                    .compare_exchange(expected, new, SeqCst, SeqCst)
                    .is_ok();
                Ok(Value::bool(swapped))
            }
            AtomicIntMethod::Add | AtomicIntMethod::Sub => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.int_of(args[0]);
                let core = self.atomic_int_core(h);
                let op = if which == AtomicIntMethod::Add {
                    BinaryOp::Add
                } else {
                    BinaryOp::Sub
                };
                // Checked compare_exchange CAS-loop: raw fetch_add/fetch_sub wrap silently (behavior
                // regression vs Atomic's Mutex + checked_add). Retry on a racing writer.
                loop {
                    let cur = core.v.load(SeqCst);
                    let new = self.int_op(op, cur, delta, span)?;
                    if core.v.compare_exchange(cur, new, SeqCst, SeqCst).is_ok() {
                        return Ok(self.make_int(new));
                    }
                }
            }
        }
    }

    /// `Executor` natives (C5/escape hatch): `_submit_settled` (start a job with its Task's channel;
    /// a refused submit settles the Task instead of raising), `shutdown` (graceful: join the jobs)
    /// and `cancel` (discard pending, trip the cancel flag, join nothing). The executor handle is
    /// re-rooted on the operand stack across the drain, and
    /// each popped task is rooted across its nested call (the receiver was popped in
    /// `do_method_call`).
    pub(super) fn executor_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = ExecutorMethod::from_name(method) else {
            return Err(self.err(format!("type Executor has no method '{method}'"), span));
        };
        match which {
            // TICKET-219 — a std internal: `Executor.submit` starts its job with the fresh cap-1
            // channel of its Task, which the job transition seals with the job's cancel value or
            // fault message when the job ends without sealing its own value (TICKET-289).
            ExecutorMethod::StdSubmitSettled => {
                self.arity_err("_submit_settled", args, 2, span)?;
                let ch = self.task_channel("_submit_settled", args[1], span)?;
                let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                self.executor_submit(h, args[0], JobSettle { ch, flag }, span)?;
                Ok(Value::nil())
            }
            ExecutorMethod::Shutdown => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                // Every job started at its `submit`, so `shutdown` is purely the JOIN: wait for the
                // unfinished jobs, then reduce the submission-ordered slots.
                self.join_executor(&core, span)?;
                self.cancel_at_join(span)?; // TICKET-147 (W14-15): the join is a cancellation point
                Ok(Value::nil())
            }
            ExecutorMethod::Cancel => {
                self.arity_err("cancel", args, 0, span)?;
                let core = self.executor_core(h);
                core.inner.lock().unwrap().shut = true;
                // D4 — "attempts to stop", COOPERATIVE not preemptive: trip the per-core cancel
                // flag (the Executor scope's own flag) so a running job dies at its next
                // cancellation point, and drain the scope's parked fibers so each is cut at its
                // own wait. A RUNNING job with no cancellation point still runs to completion;
                // a held job never started, so it never starts (CPython
                // `shutdown(cancel_futures=True)`).
                crate::vm::trip_cancel_flag(&core.cancel);
                if let Some(sched) = core.sched() {
                    sched.drop_held();
                    sched.drain_family(0);
                }
                // TICKET-118 (W13-8) — a cancel is a wake source too.
                crate::vm::sched::poke_live_scheds(&self.sched_registry);
                // TICKET-289 — no join: `cancel()` returns at once, and `shutdown()` (or the run
                // end) waits for the cut jobs. A job's fault stays in its Task.
                Ok(Value::nil())
            }
        }
    }

    /// The channel core of a Task's `_ch`, which a std internal takes as `args`.
    fn task_channel(
        &self,
        method: &str,
        v: Value,
        span: Span,
    ) -> Result<Arc<ChannelCore>, RuntimeError> {
        match v.as_obj().map(|c| self.heap.get(c)) {
            Some(Obj::Channel(core)) => Ok(Arc::clone(core)),
            _ => Err(self.err(format!("{method} requires a Channel"), span)),
        }
    }

    /// TICKET-289 — `Task.cancel()` (`std.concurrency._cancel_job`): cancel the one job of
    /// Executor `h` whose Task channel is `ch`. A job that already ended is a no-op.
    pub(super) fn cancel_task_job(&self, h: GcRef, ch: &Arc<ChannelCore>) {
        if let Some(sched) = self.executor_core(h).sched() {
            sched.cancel_job(ch);
        }
        // A cancel is a wake source too (TICKET-118, W13-8).
        crate::vm::sched::poke_live_scheds(&self.sched_registry);
    }

    /// TICKET-289 — a submit the Executor refuses (shut down, or its creating job was cancelled)
    /// returns an already-settled Task: seal the fresh channel of `settle` with `Err(msg)`. The
    /// channel is unread, so no reader needs a wake.
    pub(super) fn refuse_job(&mut self, settle: JobSettle, msg: &str) -> Result<(), RuntimeError> {
        settle.seal_err(msg);
        Ok(())
    }

    /// `Executor.submit(f)`: start `f` as a job on the Executor's detached sched. `settle` is the
    /// job's [`JobSettle`] (TICKET-219, `_submit_settled`). A submit the Executor refuses returns
    /// `Ok` with its Task already settled (TICKET-289, [`Vm::refuse_job`]).
    pub(super) fn executor_submit(
        &mut self,
        h: GcRef,
        f: Value,
        settle: JobSettle,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // The job's closure starts on the spawn path (`spawn_into`), by handle (M3.4).
        let core = self.executor_core(h);
        // Cheap early reject so a shut executor costs no wiring work. Re-checked below under
        // the lock — this one is advisory, the one that decides is the atomic one.
        if core.inner.lock().unwrap().shut {
            return self.refuse_job(settle, super::EXEC_SHUT_MSG);
        }
        // W7-39 follow-up — the inherited chain (`creator_cancel`, captured at
        // `Op::NewExecutor`) is STICKY: nothing ever resets it, matching Go's derived context
        // (a cancelled parent stays cancelled). So once the creating job's executor has been
        // cancelled, every job this core dispatches would start already-cancelled and die
        // at its first checkpoint, silently. Keep the stickiness, drop the silence: this is a
        // `submit` the executor cannot honour, exactly like a `submit` after `shutdown()`, so its
        // Task settles with the refusal text (TICKET-289).
        //
        // Read-only after construction (no lock), and EMPTY for an executor created by `main`
        // or by a `parallel:`/`spawn` fiber — those are untouched. The core's OWN `cancel` is
        // deliberately NOT checked here: `cancel` sets `shut` first, so the check above
        // already owns that case and adding it would double-report.
        if core
            .creator_cancel
            .iter()
            .any(|c| c.load(std::sync::atomic::Ordering::Relaxed))
        {
            return self.refuse_job(settle, super::EXEC_CREATOR_CANCELLED_MSG);
        }
        // TICKET-208 — a job IS a spawned task. `spawn_into` is the one function behind
        // `spawn` and `submit`: it pins the globals view as of this submit, crosses the
        // closure by value, and starts a fiber on the Executor's detached sched, built on
        // first use. No `ExecutorCore` lock is held across it: a job that captures its own
        // Executor puts an `Obj::Executor` over THIS core into the worker heap, and the heap
        // walk locks the core. A submit racing a `shutdown()` is rejected by the slot
        // reservation itself (the join closes scope 0 under the sched's core lock), so a job
        // is either rejected or counted by that join.
        if self.callable(f).is_none() {
            return Err(self.err("submit requires a function or closure".to_string(), span));
        }
        let sched = {
            let mut g = core.scope.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_none() {
                *g = self.new_detached_sched(
                    core.created_at,
                    core.creator_cancel.clone(),
                    Some(&core),
                );
            }
            g.as_ref().map(|s| Arc::clone(&s.sched))
        };
        let Some(sched) = sched else {
            return Err(self.err(super::EXEC_NO_RUNNER_MSG.to_string(), span));
        };
        self.spawn_into(
            crate::vm::sched::SpawnTarget::Scope { sched, settle },
            crate::vm::sched::SpawnHead::Callee(Some(self.program.task_entry)),
            f,
            Vec::new(),
            span,
        )
    }

    /// C5 / A2 — at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, run-all — every queued job runs and the
    /// lowest-submission-index fault propagates, W7-5). A hard `std.os.exit` is not drained (the
    /// caller gates on `pending_exit`); a task that calls `os.exit` mid-drain stops the remaining
    /// drain.
    pub(super) fn drain_live_executors(&mut self) -> Result<(), RuntimeError> {
        self.drain_live_executors_from(0)
    }

    /// TICKET-195 — THE end of a run, for every driver: `r` is what the run returned. A clean
    /// finish drains the live executors and reports the drain's result. A run that ended in a
    /// deadlock verdict drains them too, so a finished job's buffered output still reaches the sink
    /// (`run_file`'s in-process sink holds it only in the job's slot); the drain's own result is
    /// discarded, and the verdict and its trace stand. Any other fault, and a pending exit, skip the
    /// drain as before.
    pub(crate) fn finish_run(&mut self, r: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
        // TICKET-208 — a run ended by a job fault drains too: every scope is already cancelled,
        // and a finished job's buffered output must still reach the sink.
        let job = self.quiesce.has_job_fault();
        match self.rank_end(r) {
            Ok(()) => self.drain_live_executors(),
            Err(e) if (e.is_deadlock || job) && self.pending_exit.is_none() => {
                let trace = (self.fault_trace.take(), self.fault_trace_depth);
                let _ = self.drain_live_executors();
                (self.fault_trace, self.fault_trace_depth) = trace;
                Err(e)
            }
            Err(e) => {
                // TICKET-270 — an exit joins nothing, but what a finished job printed still
                // reaches the sink (Go: a goroutine's writes before `os.Exit` are on stdout).
                if self.pending_exit.is_some() {
                    self.flush_finished_jobs_from(0);
                }
                Err(e)
            }
        }
    }

    /// Like [`Vm::drain_live_executors`], but skips the first `from` registry entries. `from` is a
    /// registry index taken earlier (e.g. [`Vm::exec_registry_mark`]) and is safe to reuse across
    /// this whole drain: `exec_registry`'s one mutation is an append (`push`), so indices never shift.
    /// This is what lets a per-test drain join only the executors ITS test created, leaving one built
    /// at module top level or in `before_all` alive for the tests that follow.
    pub(super) fn drain_live_executors_from(&mut self, from: usize) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            self.flush_finished_jobs_from(from);
            return Ok(());
        }
        // EAGER (D1) — the executor is DETACHED: its work is already running, and this is where
        // the program waits for it. Walk the heap-independent registry, not `self.executors`:
        // that list is heap-keyed, so an executor created inside a task never reached it and its
        // work was silently lost (W7-5b). Creation order.
        //
        // Re-scan from the top each round rather than snapshotting: a job joined here can itself
        // construct and submit to a NEW executor, which must also be joined.
        //
        // `shut` alone is NOT "already handled": a job that shuts down the executor it runs under,
        // or a joiner that was cut, marks it `shut` while reducing NOTHING. What says "still owed a
        // join" is the core's scope: `Some` until a join reduced its slots.
        //
        // Termination: a picked core is marked `shut` AND joined, and a join on this thread always
        // has `slack == 0` (`main` is never a fiber of the Executor's sched), so it ends with the
        // scope taken, or with an `Err` that leaves the loop.
        loop {
            let next = self
                .exec_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .skip(from)
                .find(|c| c.sched().is_some())
                .map(Arc::clone);
            let Some(core) = next else { break };
            core.inner.lock().unwrap_or_else(|e| e.into_inner()).shut = true;
            // TICKET-048 — the program-exit drain has no call site of its own, so it names where
            // this Executor was created.
            if let Err(e) = self.join_executor(&core, core.created_at) {
                if self.join_leave_flushes(&e) {
                    self.flush_finished_jobs_from(from);
                }
                return Err(e);
            }
            if self.pending_exit.is_some() {
                // A joined job called os.exit — hard halt, stop joining.
                self.flush_finished_jobs_from(from);
                break;
            }
            if core.sched().is_some() {
                break;
            }
        }
        Ok(())
    }
}
