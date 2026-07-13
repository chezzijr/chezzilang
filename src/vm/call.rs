// vm::call — split out of vm/mod.rs. `super::*` == the `vm` module.
// Calls: value/native/method/static dispatch, builtins, defer/return.

use super::*;

impl Vm {
    pub(super) fn do_call(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Fast path — a plain `Func`/`Closure` runs directly over the args already contiguous on the
        // stack, skipping the `split_off` `Vec` alloc + the re-push in `push_frame`. The callee sits
        // one slot below the args; we drop it (shifting the args down one) so they become the new
        // frame's parameter slots in place. Native / not-callable callees fall through to the `Vec`
        // path (`invoke_native` needs an owned `Vec`, HOFs build args off-stack).
        let callee = self.stack[at - 1];
        if let Value::Obj(h) = callee {
            let kind = match self.heap.get(h) {
                Obj::Func { proto, home } => Some((*proto, *home, None)),
                Obj::Closure { proto, home, .. } => Some((*proto, *home, Some(h))),
                _ => None,
            };
            if let Some((proto, home, clo)) = kind {
                // Arity check BEFORE disturbing the stack — identical messages to `invoke_value`, and
                // the error path leaves `[callee, args…]` intact for the trace / `recover:`.
                let arity = self.program.protos[proto].arity;
                if clo.is_none() {
                    self.check_arity(
                        "function",
                        &self.program.protos[proto].name,
                        arity,
                        argc,
                        span,
                    )?;
                } else if argc != arity {
                    return Err(self.err(
                        format!("closure expects {arity} argument(s), got {argc}"),
                        span,
                    ));
                }
                // Experimental generators — calling a generator function does NOT run its body; it
                // allocates a suspendable generator object over the args. (Arity was just checked.)
                if self.program.protos[proto].is_generator {
                    let args: Vec<Value> = self.stack.split_off(at); // the argc args
                    self.stack.pop(); // drop the callee left beneath them
                    let g = self.alloc_generator(proto, home, clo, args);
                    self.push(g);
                    return Ok(());
                }
                // Drop the callee from beneath the args (argc-element memmove; `Value: Copy`).
                self.stack.copy_within(at.., at - 1);
                self.stack.pop();
                // M19 call-flattening: push the callee frame and let the *running* `run_until` loop
                // execute it, instead of recursing into a fresh `run_until` (which cost a native Rust
                // stack frame + an `Arc::clone(&self.program)` per call). The frame lands at
                // `frames.len()-1` with `ip = 0`; the loop already advanced the caller's `ip` past
                // this `Call` (on the captured caller index, before `step`), so the next iteration
                // runs the callee from its start. The callee's eventual `Op::Return` → `do_return`
                // pushes the result onto the caller's stack and pops the frame, and the loop resumes
                // the caller — no synchronous result to push here. Pause/`recover:`/`defer` are caught
                // by the loop body's own checks (they operate on `self.frames`, not the Rust stack).
                self.push_frame_in_place(proto, home, clo, at - 1, span)?;
                return Ok(());
            }
        }
        // Slow path — native, struct, or not-callable.
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.pop();
        let v = self.invoke_value(callee, args, span)?;
        if self.paused() {
            return Ok(()); // B1/D3: callee parked on `recv` or yielded; don't push a sentinel result.
        }
        self.push(v);
        Ok(())
    }

    /// Dispatch an already-evaluated callable `Value` on evaluated args, *returning* the result
    /// instead of pushing it. Shared by `do_call` (which pushes) and the higher-order list methods
    /// (which call it per element while keeping their source/result lists rooted on the stack).
    /// `args.len()` is the explicit arg count for arity checks.
    pub(super) fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argc = args.len();
        match callee {
            Value::Obj(h) => {
                // Borrow the heap object only long enough to read its `Copy` fields. The old code
                // `self.heap.get(h).clone()` deep-cloned the whole `Obj` on *every* call — for a
                // closure that meant cloning its captured-environment `HashMap` each time — just to
                // read `proto`/`home`. `Native` still clones its (small) name `String`, but the hot
                // user-function/closure paths now copy three scalars and allocate nothing.
                enum Callee {
                    Func {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Closure {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Native {
                        func: crate::native::NativeFn,
                        name: Box<str>,
                    },
                    Builtin(Box<str>),
                    Cffi(std::sync::Arc<crate::native::cffi::Cffi>),
                    NotCallable,
                }
                let kind = match self.heap.get(h) {
                    Obj::Func { proto, home } => Callee::Func {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Closure { proto, home, .. } => Callee::Closure {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Native { func, name } => Callee::Native {
                        func: *func,
                        name: name.clone(),
                    },
                    Obj::Builtin(name) => Callee::Builtin(name.clone()),
                    Obj::Cffi(c) => Callee::Cffi(std::sync::Arc::clone(c)),
                    _ => Callee::NotCallable,
                };
                match kind {
                    Callee::Func { proto, home } => {
                        // `&...name` (no clone): `check_arity` only formats the message on mismatch.
                        self.check_arity(
                            "function",
                            &self.program.protos[proto].name,
                            self.program.protos[proto].arity,
                            argc,
                            span,
                        )?;
                        // Experimental generators — allocate, don't run (see `do_call`'s fast path).
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, None, args));
                        }
                        self.run_proto(proto, home, None, args, true, false, span)
                    }
                    Callee::Closure { proto, home } => {
                        if argc != self.program.protos[proto].arity {
                            return Err(self.err(
                                format!(
                                    "closure expects {} argument(s), got {argc}",
                                    self.program.protos[proto].arity
                                ),
                                span,
                            ));
                        }
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, Some(h), args));
                        }
                        self.run_proto(proto, home, Some(h), args, true, false, span)
                    }
                    Callee::Native { func, name } => self.invoke_native(func, &name, args, span),
                    // A first-class universe builtin fn value (`print`/`ord`/`chr`/`panic`) — route
                    // back into the SAME logic direct calls use. `print` replicates `do_print`'s
                    // value-form defaults (space-join + trailing '\n'; sep=/end= are direct-call-only
                    // via `CallPrintSep`). `panic` returns `Err` (mirrors `do_builtin`'s panic arm) so
                    // defers still unwind. `ord`/`chr` reuse `builtin_ord`/`builtin_chr` directly.
                    Callee::Builtin(name) => match name.as_ref() {
                        "print" => {
                            // ROOT the args on the operand stack while stringifying. `args` was
                            // `split_off` the stack (do_call slow path), so it is NOT a GC root; a
                            // `Stringable` `str` method runs user code that can `collect()` at a
                            // safepoint and would sweep the LATER (still-unrendered) args — a
                            // use-after-free. `do_print` guards this exact hazard by keeping the args
                            // on the operand stack across the whole stringify loop; mirror it here:
                            // push them back, render from the rooted slots, then truncate.
                            let at = self.stack.len();
                            for v in &args {
                                self.push(*v);
                            }
                            let mut parts = Vec::with_capacity(args.len());
                            for i in 0..args.len() {
                                let v = self.stack[at + i];
                                parts.push(self.stringify(v, span, 0)?);
                            }
                            self.stack.truncate(at);
                            self.out.push_str(&parts.join(" "));
                            self.out.push('\n');
                            Ok(Value::Nil)
                        }
                        "ord" => self.builtin_ord(&args, span),
                        "chr" => self.builtin_chr(&args, span),
                        "panic" => {
                            let message = match args.first() {
                                Some(Value::Obj(h)) => match self.heap.get(*h) {
                                    Obj::Str(s) => s.to_string(),
                                    _ => self.type_name(args[0]).to_string(),
                                },
                                Some(other) => self.type_name(*other).to_string(),
                                None => String::new(),
                            };
                            Err(self.err(message, span))
                        }
                        _ => unreachable!("non-first-class builtin {name} reached invoke_value"),
                    },
                    Callee::Cffi(cffi) => {
                        // Arity is checker-guaranteed, but guard defensively (a hand-built program
                        // could bypass the checker) so a wrong arg count never indexes out of bounds.
                        self.check_arity("function", cffi.name(), cffi.param_count(), argc, span)?;
                        let mut host = VmHost { vm: self, args };
                        let ret = cffi.call(&mut host).map_err(|e| RuntimeError {
                            message: e.message,
                            span,
                        })?;
                        Ok(self.lower_native(ret))
                    }
                    Callee::NotCallable => Err(self.err(
                        format!("'{}' is not callable", self.type_name(callee)),
                        span,
                    )),
                }
            }
            other => Err(self.err(format!("'{}' is not callable", self.type_name(other)), span)),
        }
    }

    /// Invoke a native (Rust) function value (M6c). Builds a [`VmHost`] over the evaluated args,
    /// runs the binding, then lowers its engine-neutral [`NativeRet`] into a heap-allocated `Value`
    /// and pushes it. Lowering (the only allocation) happens here — at an instruction boundary,
    /// after the call returns — so the "collect only at instruction boundaries" GC invariant holds.
    pub(super) fn invoke_native(
        &mut self,
        func: crate::native::NativeFn,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // D6 — `std.net.connect` / `listen` are intercepted: they allocate a `Socket`/`Listener`
        // handle (a heap object over an `Arc`'d core), which a pure off-heap native cannot do. Run
        // inline in the VM (the `func` placeholder in `net.rs` never executes).
        if name == "connect" || name == "listen" {
            return self.net_connect_or_listen(name, args, span);
        }
        // D5 — under the M:N engine, a blocking native call (`read_file` / `sleep_ms` / `fs.*`) is
        // OFFLOADED to the dirty pool rather than run inline, so it can't pin a core worker (the G3
        // starvation). Gated on `native_reentry == 0`: a blocking native reached inside a native
        // callback can't park the fiber (its caller's loop state is on the Rust host stack), so it
        // falls through to inline. Record the call + extracted primitive args; the worker loop hands
        // it to the pool ([`Disp::Offload`]) and `paused()` skips the (missing) result-push here. The
        // result is lowered + pushed by the worker that resumes the fiber after completion.
        if self.mn.is_some()
            && self.native_reentry == 0
            && crate::native::is_blocking(name)
            && let Some(nargs) = self.extract_native_args(&args)
        {
            // D5 owe #2 — `sleep_ms` rides the timer thread (park + deadline-wake), not a pool thread
            // (`timer_ms = Some(ms)`). A non-positive (or non-int) `sleep_ms` has nothing to wait for,
            // so it is NOT offloaded — `offload` stays `None` and execution falls through to the
            // inline path below (which returns `Nil` instantly). Every other blocking native (the
            // `io`/`fs`/`request`/`process` set) keeps `timer_ms = None` → the dirty pool.
            let offload = match name {
                "sleep_ms" => {
                    // Copy the duration out first (ends the `nargs` borrow before the move below).
                    let ms = match nargs.first() {
                        Some(crate::native::NativeArg::Int(ms)) if *ms > 0 => Some(*ms as u64),
                        _ => None, // sleep_ms(<=0) / non-int: inline no-op
                    };
                    ms.map(|ms| OffloadReq {
                        func,
                        args: nargs,
                        span,
                        timer_ms: Some(ms),
                    })
                }
                _ => Some(OffloadReq {
                    func,
                    args: nargs,
                    span,
                    timer_ms: None,
                }),
            };
            if let Some(req) = offload {
                self.offload = Some(req);
                return Ok(Value::Nil); // sentinel; never pushed (the `paused()` gate at the call site)
            }
        }
        // D5 owe #3 Path C (#3) — a `sleep_ms(ms>0)` reached INSIDE a native callback (the offload gate
        // above is skipped here because it requires `native_reentry == 0`). Rather than run inline and
        // pin the worker for `ms`, DEMOTE the worker: spawn a replacement + sleep in place + resume. A
        // non-positive / non-int arg has nothing to wait for → falls through to the inline no-op.
        if self.mn.is_some()
            && self.native_reentry > 0
            && name == "sleep_ms"
            && let Some(Value::Int(ms)) = args.first()
            && *ms > 0
        {
            return self.demote_block_sleep(*ms as u64, span);
        }
        let mut host = VmHost { vm: self, args };
        let ret = func(&mut host).map_err(|e| RuntimeError {
            message: e.message,
            span,
        })?;
        Ok(self.lower_native(ret))
    }

    /// D5 — materialize a blocking native's already-evaluated `Value` args into `Send` primitives so
    /// the dirty-pool thread can run the call without the heap. Returns `None` if any arg is not a
    /// primitive (int / float / bool / str) — the scoped blocking fns only ever take primitives, so a
    /// non-primitive means "don't offload, run inline" (a safe fallback, never a fault).
    pub(super) fn extract_native_args(
        &self,
        args: &[Value],
    ) -> Option<Vec<crate::native::NativeArg>> {
        use crate::native::NativeArg as A;
        args.iter()
            .map(|v| match v {
                Value::Int(n) => Some(A::Int(*n)),
                Value::Float(f) => Some(A::Float(*f)),
                Value::Bool(b) => Some(A::Bool(*b)),
                Value::Obj(h) => match self.heap.get(*h) {
                    Obj::Str(s) => Some(A::Str(s.to_string())),
                    // A `Map[str, str]` arg (today only `request`'s headers) is snapshotted into
                    // owned pairs so it survives the off-heap handoff. Any non-str key/value reverts
                    // to `None` → run inline (safe fallback; the checker guarantees str/str for
                    // typed code, so this is unreachable from a well-typed program).
                    Obj::Map(m) => {
                        let mut pairs = Vec::with_capacity(m.entries.len());
                        for (_, k, v) in &m.entries {
                            let (Value::Obj(kh), Value::Obj(vh)) = (k, v) else {
                                return None;
                            };
                            let (Obj::Str(ks), Obj::Str(vs)) =
                                (self.heap.get(*kh), self.heap.get(*vh))
                            else {
                                return None;
                            };
                            pairs.push((ks.to_string(), vs.to_string()));
                        }
                        Some(A::Map(pairs))
                    }
                    // A `List[str]` arg (today only `run_args`'s argv) is snapshotted into owned
                    // strings so it survives the off-heap handoff. Any non-str element reverts to
                    // `None` → run inline (the checker guarantees str for typed code).
                    Obj::List(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for v in items {
                            let Value::Obj(eh) = v else {
                                return None;
                            };
                            let Obj::Str(s) = self.heap.get(*eh) else {
                                return None;
                            };
                            out.push(s.to_string());
                        }
                        Some(A::List(out))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into a VM `Value`, allocating
    /// heap objects for the reference kinds. `Ok`/`Err`/`Some`/`None` become the built-in
    /// `Result` / `Option` enum objects.
    /// Map a SCALAR callback result [`Value`] back to an engine-neutral [`crate::native::NativeRet`]
    /// for the FFI trampoline to write into C's return slot (the reverse of `lower_native`, scalar-
    /// only: a callback return is checker-restricted to int/float/bool/ptr). A non-scalar is a
    /// checker-prevented case; default to `Int(0)` defensively (the trampoline then writes a zeroed
    /// register, never UB).
    pub(super) fn value_to_native_ret(&self, v: Value) -> crate::native::NativeRet {
        use crate::native::NativeRet as N;
        match v {
            Value::Int(n) => N::Int(n),
            Value::Float(f) => N::Float(f),
            Value::Bool(b) => N::Bool(b),
            Value::Nil => N::Nil,
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Ptr(a) => N::Ptr(*a),
                _ => N::Int(0),
            },
        }
    }

    pub(super) fn lower_native(&mut self, ret: crate::native::NativeRet) -> Value {
        use crate::native::NativeRet as N;
        match ret {
            N::Int(n) => Value::Int(n),
            N::Float(f) => Value::Float(f),
            N::Bool(b) => Value::Bool(b),
            N::Nil => Value::Nil,
            N::Ptr(a) => Value::Obj(self.heap.alloc(Obj::Ptr(a))),
            N::Str(s) => self.alloc_str(s),
            N::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for x in items {
                    vs.push(self.lower_native(x));
                }
                Value::Obj(self.heap.alloc(Obj::List(vs)))
            }
            N::Struct { name, fields } => {
                // Positional layout: lower the named native fields, then place them into a flat Vec
                // at the StructDef's declaration-order index (native emit order already matches, but
                // resolving by name keeps it robust to drift). Lower first (each may allocate), then
                // allocate the struct — keeps every allocation at this boundary (GC invariant).
                let tid = self.struct_tid(&name);
                let order: Option<Vec<String>> =
                    self.program.structs.get(&name).map(|d| d.fields.clone());
                let mut lowered: Vec<(Box<str>, Value)> = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    let lv = self.lower_native(v);
                    lowered.push((k.into_boxed_str(), lv));
                }
                let fs: Vec<Value> = match order {
                    // Registered type: place each lowered value at its declaration-order slot.
                    Some(order) => order
                        .iter()
                        .map(|fname| {
                            lowered
                                .iter()
                                .find(|(k, _)| k.as_ref() == fname.as_str())
                                .map(|(_, v)| *v)
                                .unwrap_or(Value::Nil)
                        })
                        .collect(),
                    // Ad-hoc / unregistered (TID_NONE): keep native emit order positionally.
                    None => lowered.into_iter().map(|(_, v)| v).collect(),
                };
                Value::Obj(self.heap.alloc(Obj::Struct {
                    name: name.into_boxed_str(),
                    tid,
                    fields: fs,
                }))
            }
            N::Map(entries) => {
                // Native maps have unique scalar (str) keys — hash them directly (no re-entry, no
                // dedup needed).
                let mut map = MapData::default();
                for (k, v) in entries {
                    let lk = self.lower_native(k);
                    let lv = self.lower_native(v);
                    let hk = self.scalar_hash(lk);
                    map.push(hk, lk, lv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(map)))
            }
            N::Ok(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Result", "Ok", vec![p])
            }
            N::Err(msg) => {
                let p = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", vec![p])
            }
            N::Some(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Option", "Some", vec![p])
            }
            N::None => self.alloc_enum("Option", "None", Vec::new()),
        }
    }

    /// Build a NATIVE `Result`/`Option` enum instance (the native / std construction path: `Ok`/`Err`/
    /// `Some`/`None`, list `pop`, regex/request/json/fs returns). M19 lever #2 — stamps the FIXED native
    /// `VID_OK`/`VID_ERR`/`VID_SOME`/`VID_NONE_VARIANT` constant DIRECTLY, never a name lookup through
    /// `Program::variants`: a user enum declaring a variant named `Ok`/`Err`/`Some`/`None` SHADOWS that
    /// name in the `variants` map (its own dense id at `4..`), so resolving by name here would stamp the
    /// user's id onto a genuine native value — collapsing native-vs-user identity (broken `==`) and
    /// missing `?`'s `variant_id == VID_SOME`/`VID_OK` gate. The reserved 0..=3 ids are disjoint from
    /// every user id, so stamping the constant keeps native and user variants distinguishable. `ty` is
    /// retained in the signature so call sites read self-documentingly, but it is not stored.
    pub(super) fn alloc_enum(&mut self, ty: &str, variant: &str, payload: Vec<Value>) -> Value {
        let _ = ty;
        use crate::vm::op::{VID_ERR, VID_NONE, VID_NONE_VARIANT, VID_OK, VID_SOME};
        let variant_id = match variant {
            "Ok" => VID_OK,
            "Err" => VID_ERR,
            "Some" => VID_SOME,
            "None" => VID_NONE_VARIANT,
            // `alloc_enum` is the NATIVE construction path; it is only ever called with the four
            // reserved names above. The fallback is defensive only.
            _ => VID_NONE,
        };
        Value::Obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    pub(super) fn json_decode(
        &mut self,
        desc: &crate::json_decode::TypeDescriptor,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let res = self.pop();
        let bad = "decode: parse did not return a Result".to_string();
        let (rty, variant, payload) = self
            .enum_parts(res)
            .ok_or_else(|| self.err(bad.clone(), span))?;
        if rty != "Result" {
            return Err(self.err(bad, span));
        }
        match variant.as_str() {
            "Err" => {
                self.push(res); // a Result Err(str) is already a valid Result[T]
                Ok(())
            }
            "Ok" if payload.len() == 1 => {
                let jv = payload[0];
                match self.coerce_json(jv, desc, "$") {
                    Ok(v) => {
                        let r = self.alloc_enum("Result", "Ok", vec![v]);
                        self.push(r);
                    }
                    Err(msg) => {
                        let s = self.alloc_str(msg);
                        let r = self.alloc_enum("Result", "Err", vec![s]);
                        self.push(r);
                    }
                }
                Ok(())
            }
            _ => Err(self.err(bad, span)),
        }
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    pub(super) fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // M19 lever #2 — cold path: resolve the type + variant names from the id.
                    let (ty, variant) = self.enum_names(*variant_id);
                    Some((ty.to_string(), variant.to_string(), payload.clone()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. `path` is a
    /// JSON-pointer-ish breadcrumb for error messages. Mirrors the interpreter's `coerce_json`.
    pub(super) fn coerce_json(
        &mut self,
        jv: Value,
        desc: &crate::json_decode::TypeDescriptor,
        path: &str,
    ) -> Result<Value, String> {
        use crate::json_decode::TypeDescriptor as D;
        let (_jty, variant, payload) = self
            .enum_parts(jv)
            .ok_or_else(|| format!("decode: expected a JSON value at {path}"))?;
        let mismatch = |want: &str| {
            format!(
                "decode: expected {want} at {path}, found {}",
                crate::json_decode::json_kind(&variant)
            )
        };
        match desc {
            D::Int => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                // `f as i64` saturates, so range-check first. Use a strict `> 2^63` upper bound so
                // i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating
                // cast, while everything strictly above 2^63 / below -2^63 is rejected.
                if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 {
                    return Err(format!(
                        "decode: integer {f} at {path} is out of range for int"
                    ));
                }
                Ok(Value::Int(f as i64))
            }
            D::Float => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("float"))?;
                Ok(Value::Float(f))
            }
            D::Bool => match (variant.as_str(), payload.first()) {
                ("Bool", Some(Value::Bool(b))) => Ok(Value::Bool(*b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => {
                if variant == "Str" {
                    let s = self.val_str(payload[0]).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str"))
                }
            }
            D::Option(inner) => {
                if variant == "Null" {
                    Ok(self.alloc_enum("Option", "None", Vec::new()))
                } else {
                    let v = self.coerce_json(jv, inner, path)?;
                    Ok(self.alloc_enum("Option", "Some", vec![v]))
                }
            }
            D::List(inner) => {
                if variant != "Arr" {
                    return Err(mismatch("array"));
                }
                let items = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::List(items) => items.clone(),
                    _ => return Err(mismatch("array")),
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.into_iter().enumerate() {
                    out.push(self.coerce_json(it, inner, &format!("{path}[{i}]"))?);
                }
                Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
            }
            D::Map(inner) => {
                if variant != "Obj" {
                    return Err(mismatch("object"));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut out = MapData::default();
                for (hk, k, v) in entries {
                    let key = self.val_str(k).unwrap_or_default();
                    let coerced = self.coerce_json(v, inner, &format!("{path}.{key}"))?;
                    out.push(hk, k, coerced); // str keys unchanged → reuse the cached hash
                }
                Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
            }
            D::Struct {
                key,
                display,
                fields,
            } => {
                if variant != "Obj" {
                    // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
                    return Err(mismatch(&format!("object for {display}")));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                // Positional layout: `fields` (the type descriptor) is already in the struct's
                // declaration order (see `json_decode::struct_descriptor`), so push values in order.
                let mut field_vals: Vec<Value> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = entries
                        .iter()
                        .find(|(_, k, _)| self.val_str(*k).as_deref() == Some(fname.as_str()));
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, fdesc, &fpath)?,
                        None => match fdesc {
                            // A missing Option field decodes to None; anything else is an error.
                            D::Option(_) => self.alloc_enum("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push(v);
                }
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (so downstream
                // field/method lookups + `struct_tid` hit the right layout); display renders bare.
                let tid = self.struct_tid(key);
                let h = self.heap.alloc(Obj::Struct {
                    name: key.clone().into_boxed_str(),
                    tid,
                    fields: field_vals,
                });
                Ok(Value::Obj(h))
            }
        }
    }

    /// The `f64` of a JSON `Num`, else `None`.
    pub(super) fn json_num(&self, variant: &str, payload: &[Value]) -> Option<f64> {
        if variant == "Num" {
            match payload.first() {
                Some(Value::Float(f)) => Some(*f),
                Some(Value::Int(n)) => Some(*n as f64),
                _ => None,
            }
        } else {
            None
        }
    }

    /// The owned text of a str value, else `None`.
    pub(super) fn val_str(&self, v: Value) -> Option<String> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The heap handle of an `Obj` value (caller guarantees it is one).
    pub(super) fn as_obj(&self, v: Value) -> GcRef {
        match v {
            Value::Obj(h) => h,
            _ => unreachable!("as_obj on non-object"),
        }
    }

    pub(super) fn check_arity(
        &self,
        _kind: &str,
        name: &str,
        want: usize,
        got: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if want != got {
            return Err(self.err(
                format!("function '{name}' expects {want} argument(s), got {got}"),
                span,
            ));
        }
        Ok(())
    }

    /// `ic`: the per-call-site method inline-cache id from the `CallMethod` op, or [`NO_IC`] for the
    /// native-re-entry callers (`spawn`/`defer` method tasks) that need a *synchronous* result and so
    /// must take the re-entrant `run_proto` path (never the in-place frame flatten). A real `ic` ⟺ the
    /// caller is the running dispatch loop (the sole emit path), so a real `ic` is exactly the
    /// "flatten-safe" signal: the pushed frame is executed by the `run_until` that called us.
    pub(super) fn do_method_call(
        &mut self,
        method: &str,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let recv = self.pop();
        // `compare` on a primitive (int/float/str): they intrinsically satisfy `Comparable`, so an
        // erased generic body may call `.compare()` on a concrete primitive. Return the sign of the
        // ordering. Structs with their own `compare` fall through to the normal dispatch below.
        // Mirrors `interp::eval_method_call`.
        if method == "compare" && args.len() == 1 {
            let is_prim = matches!(recv, Value::Int(_) | Value::Float(_))
                || matches!(recv, Value::Obj(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_prim && let Some(ord) = self.compare(recv, args[0]) {
                self.push(Value::Int(ord as i64));
                return Ok(());
            }
        }
        // `str` on a scalar: int/float/bool/str intrinsically satisfy `Stringable`, so an erased
        // generic body may call `.str()` on a concrete scalar. Render via `stringify` (the same path
        // `str(x)`/interpolation use) and alloc a fresh `Obj::Str`. The already-`Str` receiver (T=str)
        // MUST be intercepted here too — otherwise it would fall to struct-method dispatch and fault;
        // `stringify` returns its raw unquoted contents. GC-safe: `recv` is already popped, and for a
        // scalar receiver `stringify` runs no user code, so the owned `String` exists before the alloc.
        // Mirrors the `compare` scalar branch above. Structs with their own `str` fall through below.
        if method == "str" && args.is_empty() {
            let is_scalar = matches!(
                recv,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Nil
            ) || matches!(recv, Value::Obj(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_scalar {
                let s = self.stringify(recv, span, 0)?;
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::Obj(h));
                return Ok(());
            }
        }
        let Value::Obj(h) = recv else {
            return Err(self.err(
                format!("type {} has no method '{method}'", self.type_name(recv)),
                span,
            ));
        };
        // M19 Phase 6 / N-way poly — method-call inline-cache fast path (struct methods only). Scan
        // the site's ways for a way whose cached `tid` matches the receiver layout: a hit collapses the
        // `program.structs` clone + name-keyed `def.methods` probe to a short int-compare scan AND
        // flattens the call: `[recv, args…]` go on the stack and the method frame is installed in place,
        // so the running `run_until` executes the body and its `Return` pushes the result — no re-entrant
        // `run_proto`. A megamorphic-but-bounded site (≤4 distinct receiver types) hits a way for each,
        // so it never thrashes the monomorphic refill. Only the dispatch loop reaches here (real `ic`);
        // the arity guard re-runs per hit (cheap) so a hit can never enter a frame with the wrong slot
        // count, and the tid re-compare on every probe bars a wrong body. A `sticky` site (overflowed
        // past 4 types) skips the probe and falls straight through to the slow path.
        if ic != NO_IC {
            let site = self.method_ic[ic as usize];
            if !site.sticky
                && let Obj::Struct { tid, .. } = self.heap.get(h)
            {
                let recv_tid = *tid;
                let mut hit: Option<MethodIcCell> = None;
                for way in &site.ways {
                    if way.tid != TID_NONE && way.tid == recv_tid {
                        hit = Some(*way);
                        break;
                    }
                }
                if let Some(cell) = hit {
                    let proto = cell.proto;
                    let arity = self.program.protos[proto].arity;
                    if arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    let home = self.module_objs[cell.module_idx as usize];
                    // Experimental generators — a generator method allocates rather than running (else its
                    // `Op::Yield` would poison the host run with no `generator_next` to drive it).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    let base = self.stack.len();
                    self.stack.push(recv);
                    self.stack.extend(args);
                    return self.push_frame_in_place(proto, home, None, base, span);
                }
            }
        }
        // Higher-order list methods (`map`/`filter`/`fold`) call a closure per element, which runs
        // nested VM frames that may GC at instruction boundaries. They keep the source + result
        // (and fold's accumulator) rooted on the operand stack across the loop — see `list_hof`.
        if matches!(self.heap.get(h), Obj::List(_)) && matches!(method, "map" | "filter" | "fold") {
            let result = self.list_hof(h, method, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by` also runs a closure per comparison, but sorts in place and returns nil.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by" {
            let result = self.list_sort_by(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by_key` calls a key extractor once per element, then sorts in place by key.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by_key" {
            let result = self.list_sort_by_key(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and `update`
        // re-enters the VM), so dispatch them directly off the handle, like the core-type methods.
        if matches!(self.heap.get(h), Obj::Channel(_)) {
            let result = self.channel_method(h, method, &args, span)?;
            if self.suspend.is_some() {
                return Ok(()); // B1: `recv` parked this fiber and re-rooted the receiver itself.
            }
            self.push(result);
            return Ok(());
        }
        // A cursor (`Obj::Iter`, the `Iterable` `.iter()` result) exposes `.next()` (advance the
        // snapshot, idempotent `None` past the end) and `.iter()` (returns self — every Iterator IS
        // Iterable, idempotently). Intrinsic, like the generator arm just below.
        if matches!(self.heap.get(h), Obj::Iter { .. }) {
            if !args.is_empty() {
                return Err(self.err(format!("a cursor's '{method}' takes no arguments"), span));
            }
            match method {
                "iter" => self.push(recv), // idempotent: iter() on a cursor returns self
                "next" => {
                    let Obj::Iter { items, pos } = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    let item = if *pos < items.len() {
                        let item = items[*pos];
                        *pos += 1;
                        Some(item)
                    } else {
                        None
                    };
                    let result = match item {
                        Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                        None => self.alloc_enum("Option", "None", Vec::new()),
                    };
                    self.push(result);
                }
                _ => {
                    return Err(self.err(
                        format!("a cursor has no method '{method}' (only `next()`/`iter()`)"),
                        span,
                    ));
                }
            }
            return Ok(());
        }
        // Experimental generators — `.next()` is intrinsic (resumes the coroutine), so a generator
        // result drives `for x in g():` through the same lazy `next()` step as a struct iterator.
        // `.iter()` on a generator returns self (a generator IS an Iterator, hence Iterable).
        if matches!(self.heap.get(h), Obj::Generator(_)) {
            if method == "iter" && args.is_empty() {
                self.push(recv); // idempotent: a generator's iter() is itself
                return Ok(());
            }
            if method != "next" || !args.is_empty() {
                return Err(self.err(
                    format!("a generator has no method '{method}' (only `next()`)"),
                    span,
                ));
            }
            let result = self.generator_next(h, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Shared(_)) {
            let result = self.shared_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::RwShared(_)) {
            let result = self.rwshared_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Atomic(_)) {
            let result = self.atomic_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Executor(_)) {
            let result = self.executor_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // D6: `Socket` / `Listener` methods operate on the fd in the `Arc`'d core and may park the
        // fiber on the netpoller (a would-block `read`/`write`/`accept`). Dispatch off the handle, like
        // the other core handles; gate the result-push on `poll_park` (mirrors the channel `recv` park
        // gate just above, but routed to the poller — strictly separate from `suspend`).
        if matches!(self.heap.get(h), Obj::Socket(_)) {
            let result = self.socket_method(h, method, &args, span)?;
            if self.poll_park.is_some() {
                return Ok(()); // D6: the op `WouldBlock`ed and re-rooted the receiver itself.
            }
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Listener(_)) {
            let result = self.listener_method(h, method, &args, span)?;
            if self.poll_park.is_some() {
                return Ok(());
            }
            self.push(result);
            return Ok(());
        }
        // `.iter()` on a built-in collection (str/list/map/set/bytes/bytearray) → a FRESH cursor that
        // SNAPSHOTS the current contents in the SAME order/elements as `for x in X` (list/set elems,
        // map → keys, str → per-char str, bytes/bytearray → per-byte int). Reuses `drain_iterable`
        // (the for-loop's single source of truth), then wraps the snapshot in an `Obj::Iter`. Placed
        // BEFORE the per-type dispatch so it intercepts `iter` for every collection in one spot; a
        // collection has no user-defined `iter`, so there is no precedence concern.
        if method == "iter"
            && args.is_empty()
            && matches!(
                self.heap.get(h),
                Obj::Str(_)
                    | Obj::List(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
            )
        {
            // `drain_iterable` may alloc (str per-char); root the receiver across the call.
            self.push(recv);
            let items = self.drain_iterable(recv, span)?;
            self.pop(); // unroot receiver
            let cursor = self.heap.alloc(Obj::Iter { items, pos: 0 });
            self.push(Value::Obj(cursor));
            return Ok(());
        }
        // Core-type methods (M6): built-in methods on `str` / `list`. Handled before the clone-match
        // so `list.push` mutates the heap object in place (the match below clones the Obj). Mirrors
        // `interp::builtins::call_method` exactly — error strings included (parity-tested).
        if matches!(
            self.heap.get(h),
            Obj::Str(_) | Obj::List(_) | Obj::Map(_) | Obj::Set(_)
        ) {
            let result = self.core_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Routed off the
        // handle like the other core-type methods.
        if matches!(self.heap.get(h), Obj::Bytes(_)) {
            let result = self.bytes_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // `bytearray` methods (mutable buffer): `len`/`push`/`pop`/`extend`/`decode`. Routed separately
        // (not `core_method`) but with the same in-place-via-`get_mut` discipline as `list`.
        if matches!(self.heap.get(h), Obj::ByteArray(_)) {
            let result = self.bytearray_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        self.ensure_module_faulted(h); // D1: `module.fn(...)` on a not-yet-faulted worker module
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module { name, slots, index } => {
                let member = index
                    .get(method)
                    .map(|&i| slots[i as usize])
                    .ok_or_else(|| {
                        self.err(format!("module '{name}' has no member '{method}'"), span)
                    })?;
                self.stack.push(member);
                self.stack.extend(args);
                self.do_call(argc, span)
            }
            Obj::Struct {
                name, tid, fields, ..
            } => {
                // Fix A — resolve `(proto, module_idx)` WITHOUT cloning the whole StructDef (its
                // `fields` Vec + `methods` HashMap). On a megamorphic / sticky-generic site this slow
                // path runs per call, so the per-miss StructDef clone dwarfed the dispatch itself. We
                // bump the cheap `Arc<Program>` refcount (read-only, never alias-mutated) so the
                // immutable `structs` borrow is released before the later `&mut self` calls.
                let prog = Arc::clone(&self.program);
                let def = prog
                    .structs
                    .get(name.as_ref())
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let resolved = def.methods.get(method).copied();
                let def_module_idx = def.module_idx;
                if let Some(proto) = resolved {
                    let home = self.module_objs[def_module_idx];
                    if self.program.protos[proto].arity != argc + 1 {
                        // `self` + explicit args.
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    // Experimental generators — a generator method allocates rather than running (else
                    // its `Op::Yield` would poison the host run). Covers both the IC-flatten and the
                    // re-entrant `run_proto` paths below; never IC-cached (it returns, not push-frame).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // M19 N-way poly — fill the next free way so the next call at this site hits the fast
                    // path above (only for the dispatch-loop path: a real `ic`, a registered layout
                    // `tid`). When all ways are occupied AND this `tid` is new, latch `sticky` so the
                    // site stops probing the (full) ways and goes straight here, mirroring the binop
                    // quickening's one-way `Q_GENERIC` deopt — a megamorphic site never thrashes.
                    if ic != NO_IC && tid != TID_NONE {
                        let site = &mut self.method_ic[ic as usize];
                        if !site.sticky {
                            let cell = MethodIcCell {
                                tid,
                                proto,
                                module_idx: def_module_idx as u32,
                            };
                            if let Some(free) = site.ways.iter_mut().find(|w| w.tid == TID_NONE) {
                                *free = cell;
                            } else {
                                // All ways occupied and `tid` is distinct from every one of them (else
                                // the fast path would have hit) — the site is megamorphic; go sticky.
                                site.sticky = true;
                            }
                        }
                        // Flatten: install the frame in place and let the running `run_until` execute it
                        // (mirrors the IC fast path + the `Op::Call` flatten). The re-entrant callers
                        // pass NO_IC and so keep the synchronous `run_proto` path below.
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the method parked on a blocking `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // No method named `method`: fall back to a function-typed *field* — `recv.f(args)`
                // where `f` holds a function value (the checker verified `f: fn(...) -> ...`).
                // Invoked as a value (no `self` bound — it's not a method). Positional layout:
                // resolve the field name->index from the StructDef, then index the flat `fields`.
                let fidx = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .and_then(|d| d.fields.iter().position(|f| f == method));
                if let Some(fval) = fidx.and_then(|i| fields.get(i).copied()) {
                    let v = self.invoke_value(fval, args, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the function-field call parked on `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // A user iterator struct (`next`, no explicit `iter`) IS Iterable — `.iter()` returns
                // self (idempotent), letting it flow into an `[S: Iterable[T]]` body. Mirrors interp.
                if method == "iter"
                    && args.is_empty()
                    && self
                        .program
                        .structs
                        .get(name.as_ref())
                        .is_some_and(|d| d.methods.contains_key("next"))
                {
                    self.push(recv);
                    return Ok(());
                }
                // ROOT REDESIGN — render the BARE display name (not the identity key) in the error.
                let display = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name.as_ref()));
                Err(self.err(format!("struct '{display}' has no method '{method}'"), span))
            }
            // Enum method dispatch (name-resolved, like structs). Enums are type-erased — no `tid`,
            // so the method IC is skipped (follow-up lever); we resolve `enum_methods[key][method]`
            // off the variant's `enum_name` and dispatch with the same `self`-binding path structs use.
            Obj::Enum { variant_id, .. } => {
                let prog = Arc::clone(&self.program);
                let enum_key = self.enum_names(variant_id).0.to_string();
                let resolved = prog
                    .enum_methods
                    .get(&enum_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    // An enum method's home module is the enum's declaring module (recorded in
                    // `enum_home`), so its body resolves top-level names against the right globals.
                    let home = self.module_objs[self.enum_home_module(&enum_key)];
                    if self.program.protos[proto].arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // Flatten only on the real dispatch-loop path (`ic != NO_IC`); re-entrant callers
                    // pass `NO_IC` and use the synchronous `run_proto` path (mirrors the struct arm).
                    if ic != NO_IC {
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&enum_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            // Newtype method dispatch (name-resolved, like enums). Resolves `newtype_methods[key]
            // [method]` off the wrapper's `type_key`. The underlying's methods are NOT inherited.
            Obj::NewType { type_key, .. } => {
                let prog = Arc::clone(&self.program);
                let nt_key = type_key.to_string();
                let resolved = prog
                    .newtype_methods
                    .get(&nt_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    let home = self.module_objs[self.newtype_home_module(&nt_key)];
                    if self.program.protos[proto].arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    if ic != NO_IC {
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&nt_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            _ => Err(self.err(
                format!("type {} has no method '{method}'", self.type_name(recv)),
                span,
            )),
        }
    }

    /// `Type.method(args)` — STATIC (associated) method dispatch (the "no self ⇒ static" rule).
    /// Stack: `[arg0, …]` — exactly `argc` values, NO receiver. Resolves `method` in the named
    /// struct's (`program.structs[key].methods`) or enum's (`program.enum_methods[key]`) method
    /// table by `type_key`; the body's home module is the type's declaring module. Pushes a frame
    /// holding just the args (arity == argc, no `self` slot) and runs it via `push_frame_in_place`
    /// (the dispatch-loop path) — structurally identical to enum-method dispatch minus the receiver.
    /// A static generator allocates rather than running, mirroring the instance arms.
    pub(super) fn do_static_call(
        &mut self,
        type_key: &str,
        method: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let prog = Arc::clone(&self.program);
        // Resolve the proto + home module from the struct table first, then the enum table. The
        // compiler only emits `CallStatic` for a static method that exists on a known struct/enum,
        // so a miss here is an internal invariant break — surface it as a clear runtime error.
        let (proto, home_idx) = if let Some(def) = prog.structs.get(type_key) {
            match def.methods.get(method).copied() {
                Some(p) => (p, def.module_idx),
                None => {
                    return Err(self.err(
                        format!(
                            "type '{}' has no static method '{method}'",
                            def.display_name
                        ),
                        span,
                    ));
                }
            }
        } else if let Some(ms) = prog.enum_methods.get(type_key) {
            match ms.get(method).copied() {
                Some(p) => (p, self.enum_home_module(type_key)),
                None => {
                    let display = crate::compiler::bare_display(type_key);
                    return Err(self.err(
                        format!("type {display} has no static method '{method}'"),
                        span,
                    ));
                }
            }
        } else {
            let display = crate::compiler::bare_display(type_key);
            return Err(self.err(
                format!("type {display} has no static method '{method}'"),
                span,
            ));
        };
        // A static method has NO receiver, so its arity equals `argc` exactly (no `+ 1`).
        if self.program.protos[proto].arity != argc {
            return Err(self.err(
                format!(
                    "function '{}' expects {} argument(s), got {}",
                    self.program.protos[proto].name, self.program.protos[proto].arity, argc
                ),
                span,
            ));
        }
        let home = self.module_objs[home_idx];
        if self.program.protos[proto].is_generator {
            let at = self.stack.len() - argc;
            let gen_args: Vec<Value> = self.stack.split_off(at);
            let g = self.alloc_generator(proto, home, None, gen_args);
            self.push(g);
            return Ok(());
        }
        // The `argc` args are already contiguous on the operand stack (pushed by the compiler in
        // order, no receiver). Install the frame in place over them — the running `run_until`
        // executes the body and its `Return` pushes the result.
        let base = self.stack.len() - argc;
        self.push_frame_in_place(proto, home, None, base, span)
    }

    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list.
    ///
    /// SNAPSHOT semantics: iteration walks a copy of the receiver's elements taken at call time, so
    /// a callback that MUTATES the receiver (e.g. `xs.pop()`/`xs.push(..)`) does NOT perturb the
    /// iteration sequence — it always visits exactly the elements present when the HOF was invoked.
    /// This (a) matches the interpreter (the parity oracle clones `elems` before dispatch — see
    /// `src/interp/mod.rs` `eval_method_call`, the `map`/`filter`/`fold`/`sort_by` arm), (b) matches
    /// comprehensions/for-loops (`Op::ListClone`) and `list_sort_by`/`sort_by_key` (which snapshot),
    /// (c) matches Python `map`/`filter`, and (d) is OOB-safe: indexing the original live list while
    /// a callback shrinks it would panic (regression: `map_shrinking_callback_no_panic`).
    ///
    /// GC discipline: each element is fed to a closure via `invoke_value`, which runs nested VM
    /// frames that can trigger GC at instruction boundaries. To keep the GC from collecting in-flight
    /// heap values, the source list, the snapshot list, the partially-built result list (map/filter),
    /// and the fold accumulator are all kept rooted on the operand stack across the iteration. Returns
    /// the result (caller pushes it). Arity & error messages match the interp exactly (parity-tested).
    pub(super) fn list_hof(
        &mut self,
        src_h: GcRef,
        method: &str,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack: a method receiver is popped before dispatch, so
        // an inline temporary (`make().map(..)`) is otherwise unrooted and the callback's GC could
        // collect it before we snapshot.
        self.push(Value::Obj(src_h));
        // Take a SNAPSHOT now (matching the interpreter): iterate the receiver's elements as of call
        // time so a callback that shrinks/grows the receiver mid-iteration neither perturbs the
        // sequence nor indexes past the live (now-shorter) Vec. The snapshot is heap-allocated and
        // rooted on the operand stack so its elements survive the callback's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_hof on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::Obj(res_h));
                for i in 0..n {
                    // Read from the rooted SNAPSHOT, not the live receiver: a callback that shrinks
                    // the receiver must not affect this index (stays valid, no OOB).
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    // May GC; source, snapshot, and result lists are rooted, so elements survive.
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    if is_filter {
                        match out {
                            Value::Bool(true) => {
                                if let Obj::List(items) = self.heap.get_mut(res_h) {
                                    items.push(elem);
                                }
                            }
                            Value::Bool(false) => {}
                            other => {
                                self.pop(); // unroot result
                                self.pop(); // unroot snapshot
                                self.pop(); // unroot source
                                return Err(self.err(
                                    format!(
                                        "filter predicate must return bool, got {}",
                                        self.type_name(other)
                                    ),
                                    span,
                                ));
                            }
                        }
                    } else if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(out);
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::Obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'fold' expects 2 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // Read from the rooted SNAPSHOT (see map/filter): OOB-safe under a shrinking
                    // callback.
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(acc)
            }
            _ => unreachable!("list_hof called with non-HOF method {method}"),
        }
    }

    /// `xs.sort_by(cmp)` — stable in-place sort driven by a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). The comparator re-enters the VM
    /// and may GC, so we never hold the elements in an unrooted Rust `Vec`: the source list stays
    /// rooted on the operand stack, and the merge sort permutes plain `usize` **indices**, re-reading
    /// elements from the rooted heap object on each comparison. The final permutation is materialised
    /// only after all comparator calls finish (no GC in between). Returns `nil`.
    pub(super) fn list_sort_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let cmp = args.swap_remove(0);
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(...)`) is otherwise unrooted and the comparator's GC could
        // collect it before the write-back.
        self.push(Value::Obj(src_h));
        // Sort a SNAPSHOT taken now (matching the interpreter): a comparator that mutates the source
        // list mid-sort must not perturb the ordering, and its mutations are discarded by the final
        // write-back. The snapshot list is itself heap-allocated and rooted on the operand stack so
        // its elements survive the comparator's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices(snap_h, (0..n).collect(), cmp, span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via the Chezzi comparator `cmp`.
    pub(super) fn msort_indices(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        cmp: Value,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices(src_h, idx, cmp, span)?;
        let right = self.msort_indices(src_h, right, cmp, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= 0` keeps the left element first on ties → stable.
            if self.compare_with(cmp, a, b, span)? <= 0 {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    pub(super) fn compare_with(
        &mut self,
        cmp: Value,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        match self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))? {
            Value::Int(n) => Ok(n),
            other => Err(self.err(
                format!(
                    "sort_by comparator must return int, got {}",
                    self.type_name(other)
                ),
                span,
            )),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    pub(super) fn list_sort_by_key(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by_key' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let f = args.swap_remove(0);
        self.push(Value::Obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by_key on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        // Compute keys once per element into a rooted list. Each `invoke_value` may GC; already-pushed
        // keys survive because `keys_h` is rooted (a `Vec::push` into it does not itself GC).
        let keys_h = self.heap.alloc(Obj::List(Vec::with_capacity(n)));
        self.push(Value::Obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h) {
                Obj::List(v) => v[i],
                _ => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    if let Obj::List(v) = self.heap.get_mut(keys_h) {
                        v.push(k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        let order = match self.msort_indices_by_key(keys_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot keys
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No extractor/compare calls remain, so no GC: reorder the snapshot and write back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted keys list `keys_h`), ordering
    /// by each key's natural order via [`order_key`].
    pub(super) fn msort_indices_by_key(
        &mut self,
        keys_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_by_key(keys_h, idx, span)?;
        let right = self.msort_indices_by_key(keys_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(keys_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(keys_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.order_key(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Natural order over two `sort_by_key` keys: a Comparable struct key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in [`Vm::compare`]. The checker has
    /// verified the key type is orderable.
    pub(super) fn order_key(
        &mut self,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        if let (Value::Obj(ha), Value::Obj(hb)) = (a, b)
            && matches!(self.heap.get(ha), Obj::Struct { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. })
        {
            return self.struct_compare(a, b, span);
        }
        // Float keys order by `total_cmp` for the WHOLE comparison (not just the NaN case), exactly
        // mirroring `sort()`'s `value_order` Float arm — so `sort_by_key` and `sort()` agree on every
        // float pair, including `-0.0`/`+0.0` (which `partial_cmp` ranks Equal but `total_cmp` orders
        // `-0.0 < +0.0`) and NaN (deterministic, to one end). Int keys deliberately stay on the int
        // path below (`Int.cmp`): routing them through `as_f64` would lose precision past 2^53.
        if let (Value::Float(x), Value::Float(y)) = (a, b) {
            return Ok(x.total_cmp(&y));
        }
        match self.compare(a, b) {
            Some(ord) => Ok(ord),
            // Numeric `None` means a NaN float — handled above for the Float/Float case; this arm
            // only catches a mixed int/float key pair (not reachable for a single key type K), kept
            // deterministic via `total_cmp` for safety.
            None if is_numeric(a) && is_numeric(b) => Ok(as_f64(a).total_cmp(&as_f64(b))),
            // Genuinely-incomparable types: unreachable from well-typed source; kept for safety.
            None => Err(self.err(
                format!(
                    "sort_by_key keys are not comparable: {} vs {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
        }
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    pub(super) fn expect_list_obj(
        &self,
        method: &str,
        arg: Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        match arg {
            Value::Obj(ah) => match self.heap.get(ah) {
                Obj::List(items) => Ok(items.clone()),
                _ => Err(self.err(
                    format!(
                        "{method}() expects a list argument, got {}",
                        self.type_name(arg)
                    ),
                    span,
                )),
            },
            other => Err(self.err(
                format!(
                    "{method}() expects a list argument, got {}",
                    self.type_name(other)
                ),
                span,
            )),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins). Used by
    /// `map.update`. On INSERT it snapshots a struct/enum/newtype key (Go value-key model) so the
    /// target map does NOT alias the source map's stored key object — otherwise mutating one map's
    /// key (e.g. via `keys()`) would silently corrupt the other. `snapshot_key` is pure alloc (no
    /// GC), so `h` stays valid across it.
    pub(super) fn map_upsert_in_heap(&mut self, h: GcRef, hk: u64, key: Value, val: Value) {
        let Obj::Map(m) = self.heap.get(h) else {
            unreachable!()
        };
        let pos = m
            .candidates(hk)
            .iter()
            .copied()
            .find(|&p| self.values_equal(m.entries[p].1, key));
        match pos {
            Some(i) => {
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                m.entries[i].2 = val;
            }
            None => {
                let key = self.snapshot_key(key);
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                m.push(hk, key, val);
            }
        }
    }

    /// An `int` method-argument, with a uniform type error matching the interp.
    pub(super) fn int_arg(&self, method: &str, v: &Value, span: Span) -> Result<i64, RuntimeError> {
        match v {
            Value::Int(n) => Ok(*n),
            other => Err(self.err(
                format!(
                    "{method}() expects an int argument, got {}",
                    self.type_name(*other)
                ),
                span,
            )),
        }
    }

    pub(super) fn core_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // A str argument's owned text, with a uniform type error matching the interp.
        let str_arg = |vm: &Vm, i: usize| -> Result<String, RuntimeError> {
            match args[i] {
                Value::Obj(ah) => match vm.heap.get(ah) {
                    Obj::Str(a) => Ok(a.to_string()),
                    _ => Err(vm.err(
                        format!(
                            "{method}() expects a str argument, got {}",
                            vm.type_name(args[i])
                        ),
                        span,
                    )),
                },
                other => Err(vm.err(
                    format!(
                        "{method}() expects a str argument, got {}",
                        vm.type_name(other)
                    ),
                    span,
                )),
            }
        };
        match self.heap.get(h) {
            Obj::Str(s) => {
                let s = s.to_string();
                match method {
                    "len" => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::Int(s.chars().count() as i64))
                    }
                    "upper" => {
                        self.arity_err("upper", args, 0, span)?;
                        Ok(self.alloc_str(s.to_uppercase()))
                    }
                    "lower" => {
                        self.arity_err("lower", args, 0, span)?;
                        Ok(self.alloc_str(s.to_lowercase()))
                    }
                    "trim" => {
                        self.arity_err("trim", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // `str` conforms to `Error`: `message()` returns the string itself.
                    "message" => {
                        self.arity_err("message", args, 0, span)?;
                        Ok(self.alloc_str(s.to_string()))
                    }
                    "split" => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        let parts: Vec<Value> = s
                            .split(sep.as_str())
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
                    }
                    "chars" => {
                        self.arity_err("chars", args, 0, span)?;
                        let cs: Vec<Value> = s.chars().map(|c| self.alloc_char(c)).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(cs))))
                    }
                    "starts_with" => {
                        self.arity_err("starts_with", args, 1, span)?;
                        Ok(Value::Bool(s.starts_with(str_arg(self, 0)?.as_str())))
                    }
                    "contains" => {
                        self.arity_err("contains", args, 1, span)?;
                        Ok(Value::Bool(s.contains(str_arg(self, 0)?.as_str())))
                    }
                    // `encode() -> bytes`: UTF-8 encode (str is UTF-8 internally; copy the bytes out
                    // into a new immutable `bytes`). Always succeeds — no fault path. UTF-8 only.
                    "encode" => {
                        self.arity_err("encode", args, 0, span)?;
                        let bytes = s.as_bytes().to_vec().into_boxed_slice();
                        Ok(Value::Obj(self.heap.alloc(Obj::Bytes(bytes))))
                    }
                    "join" => {
                        self.arity_err("join", args, 1, span)?;
                        let Value::Obj(lh) = args[0] else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let Obj::List(items) = self.heap.get(lh) else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let mut out = String::new();
                        for (i, item) in items.clone().iter().enumerate() {
                            let Value::Obj(ih) = item else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            let Obj::Str(part) = self.heap.get(*ih) else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            if i > 0 {
                                out.push_str(&s);
                            }
                            out.push_str(part);
                        }
                        Ok(self.alloc_str(out))
                    }
                    // gap #1 (minimal subset): receiver methods forwarding to the `std.string` free
                    // fns. Pure native Rust, byte-identical to the std.string codepoint-loop oracle
                    // (see std/string.chz) and to the interp arms.
                    "ends_with" => {
                        self.arity_err("ends_with", args, 1, span)?;
                        Ok(Value::Bool(s.ends_with(str_arg(self, 0)?.as_str())))
                    }
                    "replace" => {
                        self.arity_err("replace", args, 2, span)?;
                        let old = str_arg(self, 0)?;
                        let new = str_arg(self, 1)?;
                        // std.string returns `s` unchanged for an empty `old`.
                        if old.is_empty() {
                            Ok(self.alloc_str(s))
                        } else {
                            Ok(self.alloc_str(s.replace(old.as_str(), new.as_str())))
                        }
                    }
                    "repeat" => {
                        self.arity_err("repeat", args, 1, span)?;
                        let n = self.int_arg("repeat", &args[0], span)?;
                        // std.string: n <= 0 yields "".
                        if n <= 0 {
                            Ok(self.alloc_str(String::new()))
                        } else {
                            // Guard the allocation: `str::repeat` hard-panics on capacity overflow.
                            // Raise a recoverable fault instead (repo convention for overflow).
                            match s
                                .len()
                                .checked_mul(n as usize)
                                .filter(|&t| t <= isize::MAX as usize)
                            {
                                Some(total) => {
                                    // The byte-size guard passes huge-but-representable totals that
                                    // `str::repeat` would still abort on. Probe allocation
                                    // feasibility with `try_reserve_exact` (uninitialized capacity,
                                    // freed immediately) so an infeasible request is a recoverable
                                    // fault, then fall through to the optimized `str::repeat` — which
                                    // also short-circuits to "" for an empty receiver (`total == 0`)
                                    // instead of looping `n` times.
                                    let mut probe = String::new();
                                    if probe.try_reserve_exact(total).is_err() {
                                        return Err(self.err(
                                            "string repeat capacity overflow".to_string(),
                                            span,
                                        ));
                                    }
                                    drop(probe);
                                    Ok(self.alloc_str(s.repeat(n as usize)))
                                }
                                None => {
                                    Err(self
                                        .err("string repeat capacity overflow".to_string(), span))
                                }
                            }
                        }
                    }
                    "reverse" => {
                        self.arity_err("reverse", args, 0, span)?;
                        Ok(self.alloc_str(s.chars().rev().collect::<String>()))
                    }
                    "pad_left" => {
                        self.arity_err("pad_left", args, 2, span)?;
                        let width = self.int_arg("pad_left", &args[0], span)?;
                        let fill = str_arg(self, 1)?;
                        // An empty `fill` can never reach `width` — the old prepend loop spun
                        // forever. Fault EAGERLY (before the width check) so the diagnostic does
                        // not depend on whether padding was actually needed.
                        if fill.is_empty() {
                            return Err(
                                self.err("pad_left: fill must not be empty".to_string(), span)
                            );
                        }
                        // std.string: pad to `width` CODEPOINTS; never shrinks (a `width` at or below
                        // the current length — including `i64::MIN` — returns `s` unchanged). The
                        // early-out comes BEFORE the subtraction, so `need` can never overflow i64
                        // (a `width - len` on a negative `width` would panic in debug and wrap in
                        // release) nor wrap into a colossal `take` at the `as usize` cast.
                        let len = s.chars().count() as i64;
                        if width <= len {
                            return Ok(self.alloc_str(s));
                        }
                        let need = (width - len) as usize;
                        // Guard the allocation the same way `repeat` does — a huge `width` must be
                        // a recoverable fault, not an OOM/abort. Size the pad in BYTES EXACTLY:
                        // whole cycles of `fill` plus the byte length of its first `rem` chars (an
                        // over-estimate would spuriously fault a pad that actually fits).
                        let fill_cp = fill.chars().count(); // non-zero (empty `fill` rejected above)
                        let rem: usize =
                            fill.chars().take(need % fill_cp).map(char::len_utf8).sum();
                        match (need / fill_cp)
                            .checked_mul(fill.len())
                            .and_then(|pad| pad.checked_add(rem))
                            .and_then(|pad| pad.checked_add(s.len()))
                            .filter(|&t| t <= isize::MAX as usize)
                        {
                            Some(total) => {
                                let mut out = String::new();
                                if out.try_reserve_exact(total).is_err() {
                                    return Err(
                                        self.err("string pad capacity overflow".to_string(), span)
                                    );
                                }
                                // The fill is a repeating cycle TRUNCATED to fit, so the result is
                                // EXACTLY `width` codepoints (`"a".pad_left(4, "xy")` -> `"xyxa"`).
                                out.extend(fill.chars().cycle().take(need));
                                out.push_str(&s);
                                Ok(self.alloc_str(out))
                            }
                            None => Err(self.err("string pad capacity overflow".to_string(), span)),
                        }
                    }
                    "index_of" => {
                        self.arity_err("index_of", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.string: empty -> 0; otherwise the CODEPOINT index (not byte offset).
                        if sub.is_empty() {
                            Ok(Value::Int(0))
                        } else {
                            match s.find(sub.as_str()) {
                                Some(byte) => Ok(Value::Int(s[..byte].chars().count() as i64)),
                                None => Ok(Value::Int(-1)),
                            }
                        }
                    }
                    "count" => {
                        self.arity_err("count", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.string: empty -> 0; otherwise non-overlapping count.
                        if sub.is_empty() {
                            Ok(Value::Int(0))
                        } else {
                            Ok(Value::Int(s.matches(sub.as_str()).count() as i64))
                        }
                    }
                    "strip_prefix" => {
                        self.arity_err("strip_prefix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_prefix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "strip_suffix" => {
                        self.arity_err("strip_suffix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_suffix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "split_lines" => {
                        self.arity_err("split_lines", args, 0, span)?;
                        let parts: Vec<Value> = s
                            .split('\n')
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
                    }
                    // `strip` is a trim alias.
                    "strip" => {
                        self.arity_err("strip", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // gap #7: safe numeric parse — None on bad input (trims like int()/float()).
                    "to_int" => {
                        self.arity_err("to_int", args, 0, span)?;
                        match s.trim().parse::<i64>() {
                            Ok(n) => Ok(self.alloc_enum("Option", "Some", vec![Value::Int(n)])),
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
                    }
                    "to_float" => {
                        self.arity_err("to_float", args, 0, span)?;
                        match s.trim().parse::<f64>() {
                            Ok(f) => Ok(self.alloc_enum("Option", "Some", vec![Value::Float(f)])),
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
                    }
                    // Result-returning parse siblings of to_int/to_float — carry a human-readable
                    // Err(msg) instead of None (trims first, like int()/float()/to_int/to_float).
                    "parse_int" => {
                        self.arity_err("parse_int", args, 0, span)?;
                        match s.trim().parse::<i64>() {
                            Ok(n) => Ok(self.alloc_enum("Result", "Ok", vec![Value::Int(n)])),
                            Err(_) => {
                                let msg =
                                    self.alloc_str(format!("cannot parse '{s}' as an integer"));
                                Ok(self.alloc_enum("Result", "Err", vec![msg]))
                            }
                        }
                    }
                    "parse_float" => {
                        self.arity_err("parse_float", args, 0, span)?;
                        match s.trim().parse::<f64>() {
                            Ok(f) => Ok(self.alloc_enum("Result", "Ok", vec![Value::Float(f)])),
                            Err(_) => {
                                let msg = self.alloc_str(format!("cannot parse '{s}' as a float"));
                                Ok(self.alloc_enum("Result", "Err", vec![msg]))
                            }
                        }
                    }
                    _ => Err(self.err(format!("type str has no method '{method}'"), span)),
                }
            }
            Obj::List(items) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(items.len() as i64))
                }
                "push" => {
                    self.arity_err("push", args, 1, span)?;
                    let v = args[0];
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    items.push(v);
                    Ok(Value::Nil)
                }
                "pop" => {
                    self.arity_err("pop", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    let popped = items.pop();
                    // M19 lever #2 — route through `alloc_enum` so the dense `variant_id` is stamped
                    // (replacing the two ad-hoc per-instance `Box<str>` builds).
                    Ok(match popped {
                        Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                        None => self.alloc_enum("Option", "None", vec![]),
                    })
                }
                "reverse" => {
                    self.arity_err("reverse", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    items.reverse();
                    Ok(Value::Nil)
                }
                "sort" => {
                    self.arity_err("sort", args, 0, span)?;
                    // In place, ascending. Checker guarantees a homogeneous orderable element type.
                    // A list of Comparable structs orders via each struct's `compare` (engine
                    // re-entry, so a merge sort that holds `&mut self`); primitives use the faster
                    // `value_order`. Str elements live on the heap, so `value_order` needs
                    // `&self.heap` — clone the elements out, sort (no alloc/closure → no GC for the
                    // primitive path), then write back.
                    let is_struct = matches!(items.first(), Some(Value::Obj(hh)) if matches!(self.heap.get(*hh), Obj::Struct { .. }));
                    if is_struct {
                        // Struct compare re-enters the VM (may GC) → rooted, index-based sort.
                        return self.list_sort_structs(h, span);
                    }
                    let mut elems = items.clone();
                    elems.sort_by(|a, b| self.value_order(*a, *b));
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    *items = elems;
                    Ok(Value::Nil)
                }
                "contains" => {
                    self.arity_err("contains", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    Ok(Value::Bool(
                        elems.iter().any(|v| self.values_equal(*v, target)),
                    ))
                }
                "index_of" => {
                    self.arity_err("index_of", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    let idx = elems.iter().position(|v| self.values_equal(*v, target));
                    Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
                }
                "concat" => {
                    self.arity_err("concat", args, 1, span)?;
                    let mut out = items.clone();
                    out.extend(self.expect_list_obj("concat", args[0], span)?);
                    // `out` is fully built and moved into the new Obj before any GC can run.
                    Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
                }
                "extend" => {
                    self.arity_err("extend", args, 1, span)?;
                    // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
                    let appended = self.expect_list_obj("extend", args[0], span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    items.extend(appended);
                    Ok(Value::Nil)
                }
                "sum" => {
                    self.arity_err("sum", args, 0, span)?;
                    let any_float = items.iter().any(|v| matches!(v, Value::Float(_)));
                    if any_float {
                        let mut acc = 0.0_f64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => acc += *n as f64,
                                Value::Float(f) => acc += *f,
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Float(acc))
                    } else {
                        let mut acc = 0_i64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => {
                                    acc = acc.checked_add(*n).ok_or_else(|| {
                                        self.err("integer overflow in Add".to_string(), span)
                                    })?;
                                }
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Int(acc))
                    }
                }
                _ => Err(self.err(format!("type list has no method '{method}'"), span)),
            },
            Obj::Map(m) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(m.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let found = m
                        .candidates(hk)
                        .iter()
                        .any(|&p| self.values_equal(m.entries[p].1, key));
                    Ok(Value::Bool(found))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let found = m
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key))
                        .map(|p| m.entries[p].2);
                    match found {
                        Some(v) => Ok(self.alloc_enum("Option", "Some", vec![v])),
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "keys" => {
                    self.arity_err("keys", args, 0, span)?;
                    let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(keys))))
                }
                "values" => {
                    self.arity_err("values", args, 0, span)?;
                    let vals: Vec<Value> = m.entries.iter().map(|(_, _, v)| *v).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(vals))))
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let pos = m
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key));
                    match pos {
                        Some(i) => {
                            let Obj::Map(m) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
                            let (_, _, v) = m.remove_at(i);
                            Ok(self.alloc_enum("Option", "Some", vec![v]))
                        }
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "merge" | "update" => {
                    self.arity_err(method, args, 1, span)?;
                    // Snapshot the incoming entries (with their cached hashes — engine-wide
                    // consistent, so reuse is sound) first; handles `m.merge(m)`/`m.update(m)`.
                    let incoming = match args[0] {
                        Value::Obj(oh) => match self.heap.get(oh) {
                            Obj::Map(o) => o.entries.clone(),
                            _ => {
                                return Err(self.err(
                                    format!(
                                        "{method}() expects a map argument, got {}",
                                        self.type_name(args[0])
                                    ),
                                    span,
                                ));
                            }
                        },
                        other => {
                            return Err(self.err(
                                format!(
                                    "{method}() expects a map argument, got {}",
                                    self.type_name(other)
                                ),
                                span,
                            ));
                        }
                    };
                    if method == "merge" {
                        // Build the result fresh, snapshotting EVERY struct/enum/newtype key (Go
                        // value-key model) so the new map aliases neither the receiver's nor the
                        // argument's stored keys — `m.clone()` would carry the receiver's key handles
                        // by reference. The receiver's own keys are already unique, so they need no
                        // dedup; the argument's entries upsert (last-wins). `snapshot_key` is pure
                        // alloc (no GC), so the local `out`'s contents stay valid across it.
                        let mine = match self.heap.get(h) {
                            Obj::Map(m) => m.entries.clone(),
                            _ => unreachable!(),
                        };
                        let mut out = MapData::default();
                        for (hk, key, val) in mine {
                            let key = self.snapshot_key(key);
                            out.push(hk, key, val);
                        }
                        for (hk, key, val) in incoming {
                            let pos = out
                                .candidates(hk)
                                .iter()
                                .copied()
                                .find(|&p| self.values_equal(out.entries[p].1, key));
                            match pos {
                                Some(i) => out.entries[i].2 = val,
                                None => {
                                    let key = self.snapshot_key(key);
                                    out.push(hk, key, val);
                                }
                            }
                        }
                        // `out` is fully built and moved into the new Obj before any GC can run.
                        Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
                    } else {
                        for (hk, key, val) in incoming {
                            self.map_upsert_in_heap(h, hk, key, val);
                        }
                        Ok(Value::Nil)
                    }
                }
                _ => Err(self.err(format!("type map has no method '{method}'"), span)),
            },
            Obj::Set(s) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(s.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    Ok(Value::Bool(
                        s.candidates(hx)
                            .iter()
                            .any(|&p| self.values_equal(s.entries[p].1, x)),
                    ))
                }
                "add" => {
                    self.arity_err("add", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let present = s
                        .candidates(hx)
                        .iter()
                        .any(|&p| self.values_equal(s.entries[p].1, x));
                    if !present {
                        // Snapshot a struct/enum/newtype element on insert (Go value-key model) so a
                        // later mutation of the caller's live value can't corrupt the set. Pure alloc
                        // (no GC), so no rooting needed before the immediately-following push.
                        let x = self.snapshot_key(x);
                        let Obj::Set(s) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        s.push(hx, x);
                    }
                    Ok(Value::Nil)
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let pos = s
                        .candidates(hx)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(s.entries[p].1, x));
                    match pos {
                        Some(i) => {
                            let Obj::Set(s) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
                            s.remove_at(i);
                            Ok(Value::Bool(true))
                        }
                        None => Ok(Value::Bool(false)),
                    }
                }
                "union" | "intersection" | "difference" => {
                    self.arity_err(method, args, 1, span)?;
                    // Both operands already carry per-element cached hashes, so set algebra needs no
                    // re-hashing (no user code re-enters) — purely build a fresh hash set, deduping
                    // and membership-testing via the cached hashes confirmed by `values_equal`.
                    let mine = match self.heap.get(h) {
                        Obj::Set(s) => s.entries.clone(),
                        _ => unreachable!(),
                    };
                    let other = self.set_arg(args[0], method, span)?;
                    let mut out = SetData::default();
                    let add = |vm: &Vm, set: &mut SetData, he: u64, e: Value| {
                        if !set
                            .candidates(he)
                            .iter()
                            .any(|&p| vm.values_equal(set.entries[p].1, e))
                        {
                            set.push(he, e);
                        }
                    };
                    match method {
                        "union" => {
                            for (he, e) in mine.iter().chain(other.entries.iter()) {
                                add(self, &mut out, *he, *e);
                            }
                        }
                        // intersection keeps mine's elements present in other; difference drops them.
                        m => {
                            let keep_when_present = m == "intersection";
                            for (he, e) in &mine {
                                let in_other = other
                                    .candidates(*he)
                                    .iter()
                                    .any(|&p| self.values_equal(other.entries[p].1, *e));
                                if in_other == keep_when_present {
                                    add(self, &mut out, *he, *e);
                                }
                            }
                        }
                    }
                    Ok(Value::Obj(self.heap.alloc(Obj::Set(out))))
                }
                _ => Err(self.err(format!("type set has no method '{method}'"), span)),
            },
            _ => unreachable!("core_method dispatched a non-str/list/map/set receiver"),
        }
    }

    /// Built-in methods on a `bytearray` receiver (`h` is the heap handle): `len`, `push(int 0..=255)`,
    /// `pop() -> Option[int]`, `extend(bytes|bytearray|List[int])`. Mirrors the interp's
    /// `eval_bytearray_method` and the checker's `bytearray_method_sig` — keep all three in lockstep.
    /// Mutators write IN PLACE through the heap slot (`get_mut`), exactly like the `list` methods, so a
    /// second binding to the same `bytearray` observes the change.
    pub(super) fn bytearray_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::Int(b.len() as i64))
            }
            "push" => {
                self.arity_err("push", args, 1, span)?;
                let byte = match args[0] {
                    Value::Int(n) if (0..=255).contains(&n) => n as u8,
                    Value::Int(n) => {
                        return Err(self.err(
                            format!("byte value {n} out of range (must be 0..=255)"),
                            span,
                        ));
                    }
                    other => {
                        return Err(self.err(
                            format!("push() expects an int, got {}", self.type_name(other)),
                            span,
                        ));
                    }
                };
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.push(byte);
                Ok(Value::Nil)
            }
            "pop" => {
                self.arity_err("pop", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                let popped = b.pop();
                Ok(match popped {
                    Some(x) => self.alloc_enum("Option", "Some", vec![Value::Int(x as i64)]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            "extend" => {
                self.arity_err("extend", args, 1, span)?;
                // Snapshot the other side first (so `ba.extend(ba)` terminates) — also validates
                // ints 0..=255 / element types up front, mirroring the constructor.
                let appended = self.collect_bytes_arg("extend", args[0], span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.extend_from_slice(&appended);
                Ok(Value::Nil)
            }
            // `decode() -> str`: UTF-8 decode the current buffer. Invalid UTF-8 is a RECOVERABLE
            // fault (catchable by `recover:`), never a panic — mirrors the bytes path + the interp.
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            _ => Err(self.err(format!("type bytearray has no method '{method}'"), span)),
        }
    }

    /// `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors the interp's
    /// bytes-method arm and the checker's `bytes_method_sig` — keep all three in lockstep.
    pub(super) fn bytes_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::Int(b.len() as i64))
            }
            _ => Err(self.err(format!("type bytes has no method '{method}'"), span)),
        }
    }

    /// UTF-8 decode a byte slice into a new heap `str`. Invalid UTF-8 maps to a RECOVERABLE
    /// RuntimeError (catchable by `recover:`), not a panic — the error message is byte-identical to
    /// the interp's so the two engines stay parity-equal.
    pub(super) fn decode_utf8(&mut self, bytes: &[u8], span: Span) -> Result<Value, RuntimeError> {
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(self.alloc_str(s.to_string())),
            Err(_) => Err(self.err("invalid UTF-8 in decode()".to_string(), span)),
        }
    }

    /// Read a set argument (for set algebra), erroring if it isn't a set. Returns a clone of its
    /// [`SetData`] (entries + index) so membership tests reuse the cached hashes.
    pub(super) fn set_arg(
        &self,
        v: Value,
        method: &str,
        span: Span,
    ) -> Result<SetData, RuntimeError> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Set(s) => Ok(s.clone()),
                _ => Err(self.err(
                    format!(
                        "{method}() expects a set argument, got {}",
                        self.type_name(v)
                    ),
                    span,
                )),
            },
            _ => Err(self.err(
                format!(
                    "{method}() expects a set argument, got {}",
                    self.type_name(v)
                ),
                span,
            )),
        }
    }

    /// Allocate a heap string and return its handle as a `Value`.
    pub(super) fn alloc_str(&mut self, s: String) -> Value {
        Value::Obj(self.heap.alloc(Obj::Str(s.into())))
    }

    /// M19 Phase 3 — the 1-char `str` value for `c`, in a single allocation. `c.to_string()` +
    /// `into_boxed_str` is two allocs (a `String`, then a shrink-to-fit realloc); encoding straight
    /// into a stack buffer and boxing the `&str` is one. Used by string indexing/iteration/`chr`.
    pub(super) fn alloc_char(&mut self, c: char) -> Value {
        let mut buf = [0u8; 4];
        Value::Obj(
            self.heap
                .alloc(Obj::Str((&*c.encode_utf8(&mut buf)).into())),
        )
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    ///
    /// Deferred calls (`defer`) run LIFO first, while the frame is still live so the GC keeps their
    /// values — and the return value — rooted. A fault in a deferred call supersedes the frame's
    /// result (Go: a panic in a defer wins): it returns `Err` and the frame is still torn down.
    pub(super) fn do_return(&mut self, _propagated: bool) -> Result<(), RuntimeError> {
        // M-C implicit nurseries: if this frame opened one, JOIN it here (run its spawned tasks to
        // completion) BEFORE the frame unwinds — `return`/`?`/fall-through is the join barrier. This
        // runs while the frame is still current and the return value (if any) still sits on the
        // operand stack; `join_nursery` swaps the whole `FiberCtx`, never the operand value, so the
        // value survives. Any *inner* `parallel:` this return/`?` escaped sits ABOVE the implicit
        // nursery and is cancelled-and-reported first (existing escape semantics). A task that faults
        // during the join propagates as this function's error (the frame is intact, so the normal
        // unwind machinery runs its defers). NB: an uncaught *body* fault never reaches here — it
        // unwinds via the handler path, which cancels (not joins) the implicit nursery.
        let frame_top = self.frames.last().unwrap();
        let nursery_floor = frame_top.nursery_len;
        if frame_top.has_implicit_nursery {
            self.drain_escaped_nursery(nursery_floor + 1); // cancel inner escaped `parallel:` levels
            if self.nurseries.len() > nursery_floor {
                self.join_nursery()?; // join the implicit nursery (runs its tasks)
            }
        }
        // Drain with the return value still on top of the stack (rooted) and the frame still on
        // `self.frames` (so `collect` roots the pending records). Defers run AFTER the implicit-nursery
        // join above (tasks complete, then cleanup).
        let defer_err = self.drain_top_frame_deferred();
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        // Reclaim any `parallel:` nursery this frame opened but whose `JoinNursery` was skipped by a
        // `?`/return escape (no-op on the normal fall-through path — `JoinNursery` already popped it,
        // so `nurseries.len() == frame.nursery_len`; also a no-op when the implicit nursery above was
        // just joined). Mirrors the `recover:` catch path and the interp's unconditional
        // `exec_parallel` pop; a nested frame keeps the parent's nursery (it captured the parent depth
        // at entry). TASK B: route through `drain_escaped_nursery` so the unstarted tasks are
        // cancelled-and-reported (not silently dropped). NB: within-frame `break`/`continue` out of a
        // `parallel:` no longer rely on this — the compiler emits a `ReclaimNursery` before their
        // loop-exit `Jump` (see `compile_parallel`/`emit_loop_body_drain`), reclaiming block-scoped.
        self.drain_escaped_nursery(frame.nursery_len);
        // Drop any `recover:` handlers installed in the frame we just left (e.g. a `?` early-return
        // out of a recover block) — they must not survive to catch a later, unrelated fault.
        while self
            .handlers
            .last()
            .is_some_and(|h| h.frame_len > self.frames.len())
        {
            self.handlers.pop();
        }
        if let Some(e) = defer_err {
            return Err(e);
        }
        self.push(ret);
        Ok(())
    }

    /// `defer f(args)` / `defer recv.m(args)` — pop the callee/receiver + `argc` args off the stack
    /// and record a deferred call on the current frame (drained LIFO at frame exit). The values were
    /// evaluated now (Go semantics); the call runs at exit.
    pub(super) fn do_defer(&mut self, method: Option<String>, argc: usize, span: Span) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let d = match method {
            Some(name) => Deferred::Method {
                recv: head,
                name,
                args,
                span,
            },
            None => Deferred::Call {
                callee: head,
                args,
                span,
            },
        };
        self.frames.last_mut().unwrap().deferred.push(d);
    }

    /// Run one deferred call to completion (the result is discarded). `Call` rides `invoke_value`;
    /// `Method` re-uses the normal method dispatch by pushing the receiver + args and popping the
    /// discarded result. The pop→push window has no instruction boundary, so the moved-out values
    /// can't be collected before they're re-rooted.
    pub(super) fn run_one_deferred(&mut self, d: Deferred) -> Result<(), RuntimeError> {
        // Guarded: a deferred call runs during frame teardown (the LIFO drain loop is Rust-stack
        // state), so a blocking `recv` inside it cannot park — it faults `deadlock` (B1).
        self.guarded(|vm| match d {
            Deferred::Call { callee, args, span } => {
                vm.invoke_value(callee, args, span)?;
                Ok(())
            }
            Deferred::Method {
                recv,
                name,
                args,
                span,
            } => {
                let argc = args.len();
                vm.push(recv);
                for a in args {
                    vm.push(a);
                }
                vm.do_method_call(&name, argc, NO_IC, span)?;
                vm.pop(); // discard the deferred call's result
                Ok(())
            }
        })
    }

    // ----- concurrency C4: sequential, run-to-completion executor (mirrors the interpreter) -----
}
