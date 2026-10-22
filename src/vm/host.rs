//! VM: the native `Host` implementations, float formatting and integer parsing.
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

/// D5 — the off-heap [`crate::native::Host`] for a blocking native run on the dirty pool (no `Vm`,
/// no heap). It serves the pre-extracted primitive args ([`crate::native::NativeArg`]) and *panics*
/// on any host-I/O method: the offload classifier ([`crate::native::Kind::Blocking`]) only covers fns
/// that read primitive args + return a primitive `NativeRet`, so reaching stdout/stderr/stdin/os here
/// means a fn was misclassified as off-heap-safe — a bug to surface loudly, not paper over.
pub(super) struct OffloadHost {
    pub(super) args: Vec<crate::native::NativeArg>,
}

impl crate::native::Host for OffloadHost {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Int(n)) => Ok(*n),
            Some(_) => Err(crate::native::HostError::arg_type(i, "int", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        matches!(self.args.get(i), Some(crate::native::NativeArg::Int(_)))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Float(f)) => Ok(*f),
            Some(crate::native::NativeArg::Int(n)) => Ok(*n as f64),
            Some(_) => Err(crate::native::HostError::arg_type(i, "float", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Str(s)) => Ok(s.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "str", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.args.get(i) {
            // Pre-extracted on the worker (heap live) into owned pairs; served back off-thread.
            Some(crate::native::NativeArg::Map(pairs)) => Ok(pairs.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(
                i,
                "Map[str, str]",
                "other",
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        match self.args.get(i) {
            // Pre-extracted on the worker (heap live) into owned strings; served back off-thread.
            Some(crate::native::NativeArg::List(items)) => Ok(items.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "List[str]", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        match self.args.get(i) {
            // R1 — pre-extracted on the worker (heap live) into an owned byte vec; served off-thread.
            Some(crate::native::NativeArg::Bytes(b)) => Ok(b.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "bytes", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, _s: &str) {
        unreachable!("offloaded blocking native must not write stdout (off-heap host)")
    }
    fn write_stderr(&mut self, _s: &str) {
        unreachable!("offloaded blocking native must not write stderr (off-heap host)")
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn read_all(&mut self) -> Result<String, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn read_char(&mut self) -> Result<Option<String>, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn os_args(&self) -> Vec<String> {
        unreachable!("offloaded blocking native must not read os args (off-heap host)")
    }
    fn os_env(&self, _key: &str) -> Option<String> {
        unreachable!("offloaded blocking native must not read env (off-heap host)")
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read cwd (off-heap host)")
    }
}

/// The VM's [`crate::native::Host`] adapter: lets a native fn read the evaluated `Value` arguments
/// (reaching into the heap for `str` args) and write to the captured output buffers. Holds `&mut
/// Vm` plus the arg vector; it allocates nothing itself — the returned [`crate::native::NativeRet`]
/// is lowered to heap objects by [`Vm::lower_native`] after the call returns. (Stdin / args / env /
/// cooperative-exit are wired in a later milestone; the unwired methods return inert defaults.)
pub(super) struct VmHost<'a> {
    pub(super) vm: &'a mut Vm,
    pub(super) args: Vec<Value>,
}

impl VmHost<'_> {
    /// `args[i]` as a channel handle (TICKET-219, `std.concurrency._settle` / `is_settled`).
    fn arg_channel(&self, i: usize) -> Result<GcRef, crate::native::HostError> {
        let Some(v) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        match v.as_obj() {
            Some(h) if matches!(self.vm.heap.get(h), Obj::Channel(_)) => Ok(h),
            _ => Err(crate::native::HostError::arg_type(
                i,
                "Channel",
                self.vm.type_name(v),
            )),
        }
    }
}

impl crate::native::Host for VmHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => match self.vm.int_val(v) {
                Some(n) => Ok(n),
                None => Err(crate::native::HostError::arg_type(
                    i,
                    "int",
                    self.vm.type_name(v),
                )),
            },
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        self.args.get(i).is_some_and(|v| self.vm.is_integral(*v))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) if v.is_float() => Ok(self.vm.float_of(v)),
            Some(v) if self.vm.is_integral(v) => Ok(self.vm.int_of(v) as f64),
            Some(v) => Err(crate::native::HostError::arg_type(
                i,
                "float",
                self.vm.type_name(v),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => match v.as_bool() {
                Some(b) => Ok(b),
                None => Err(crate::native::HostError::arg_type(
                    i,
                    "bool",
                    self.vm.type_name(v),
                )),
            },
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Ptr(a) = self.vm.heap.get(h)
                {
                    Ok(*a)
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "ptr",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Str(s) = self.vm.heap.get(h)
                {
                    Ok(s.to_string())
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "str",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    /// R1 — a `bytes` arg, copied out of the heap (no aliasing). `bytes` ONLY: the checker types every
    /// seam param `bytes`, and neither a `bytearray` (not assignable to a `bytes` sink — 7b29552) nor a
    /// `list[int]` (a `bytes()`-ctor convenience, not a seam contract) can reach here from typed code.
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Bytes(b) = self.vm.heap.get(h)
                {
                    Ok(b.to_vec())
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "bytes",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_c_value(
        &mut self,
        i: usize,
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        let Some(v) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        self.vm
            .c_value_of(v)
            .ok_or_else(|| crate::native::HostError::arg_type(i, "C value", self.vm.type_name(v)))
    }
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[crate::native::NativeRet],
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        // The Chezzi closure passed as extern arg `arg_index` (a function pointer to C). Re-enter the
        // engine to run it on the C scalars, then lower its result back to a NativeRet. Same-thread,
        // synchronous: this fires inside the extern `ffi_call` on the calling (worker) thread, so it
        // is plain Rust-stack recursion through the proven `guarded`+`invoke_value` re-entry path
        // (the one map/filter/sort use). No span is available at the FFI boundary; use a zero span.
        let callee = *self
            .args
            .get(arg_index)
            .ok_or_else(|| crate::native::HostError {
                message: format!("callback argument {arg_index} is missing"),
            })?;
        let span = Span::default();
        // Lower the C scalar args to engine `Value`s (allocations happen here, at the boundary).
        let vals: Vec<Value> = args
            .iter()
            .map(|a| self.vm.lower_native(a.clone()))
            .collect();
        let result = self
            .vm
            .guarded(|vm| vm.invoke_value(callee, vals, span))
            .map_err(|e| crate::native::HostError { message: e.message })?;
        Ok(self.vm.value_to_native_ret(result))
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        let Some(v) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        let entries = match v.as_obj().map(|h| self.vm.heap.get(h)) {
            Some(Obj::Map(m)) => m.entries.clone(),
            _ => {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "Map[str, str]",
                    self.vm.type_name(v),
                ));
            }
        };
        // Iterate `entries` (insertion order) so header order is deterministic and matches the interp
        // + off-heap hosts. Every key/value must be a str.
        let mut pairs = Vec::with_capacity(entries.len());
        for (_, k, val) in &entries {
            let (Some(kh), Some(vh)) = (k.as_obj(), val.as_obj()) else {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "Map[str, str]",
                    "other",
                ));
            };
            let (Obj::Str(ks), Obj::Str(vs)) = (self.vm.heap.get(kh), self.vm.heap.get(vh)) else {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "Map[str, str]",
                    "other",
                ));
            };
            pairs.push((ks.to_string(), vs.to_string()));
        }
        Ok(pairs)
    }
    fn arg_is_task_copy(&self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => Ok(v.as_obj().is_some_and(|h| self.vm.heap.is_copied(h))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_settle_channel(&mut self, ch: usize, v: usize) -> Result<(), crate::native::HostError> {
        let h = self.arg_channel(ch)?;
        let Some(val) = self.args.get(v).copied() else {
            return Err(crate::native::HostError::missing_arg(v));
        };
        let core = self.vm.channel_core(h);
        let w = self
            .vm
            .to_wire_crossable(val, Span::default())
            .map_err(|e| crate::native::HostError { message: e.message })?;
        let sum = crate::vm::core::wire_summary(&w);
        let sealed = core
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seal(sum, w);
        if sealed {
            self.vm.wake_receivers_all(h, &core);
        }
        Ok(())
    }
    fn arg_cancel_job(&mut self, ex: usize, ch: usize) -> Result<(), crate::native::HostError> {
        let Some(ev) = self.args.get(ex).copied() else {
            return Err(crate::native::HostError::missing_arg(ex));
        };
        let h = match ev.as_obj() {
            Some(h) if matches!(self.vm.heap.get(h), Obj::Executor(_)) => h,
            _ => {
                return Err(crate::native::HostError::arg_type(
                    ex,
                    "Executor",
                    self.vm.type_name(ev),
                ));
            }
        };
        let core = self.vm.channel_core(self.arg_channel(ch)?);
        self.vm.cancel_task_job(h, &core);
        Ok(())
    }
    fn arg_channel_settled(&self, i: usize) -> Result<bool, crate::native::HostError> {
        let h = self.arg_channel(i)?;
        let core = self.vm.channel_core(h);
        let sealed = core.q.lock().unwrap_or_else(|e| e.into_inner()).is_sealed();
        Ok(sealed)
    }
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        let Some(av) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        let items = match av.as_obj().map(|h| self.vm.heap.get(h)) {
            Some(Obj::List(items)) => items.clone(),
            _ => {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "List[str]",
                    self.vm.type_name(av),
                ));
            }
        };
        // Iterate in list order (it IS the argv). Every element must be a str.
        let mut out = Vec::with_capacity(items.len());
        for v in &items {
            let Some(eh) = v.as_obj() else {
                return Err(crate::native::HostError::arg_type(i, "List[str]", "other"));
            };
            let Obj::Str(s) = self.vm.heap.get(eh) else {
                return Err(crate::native::HostError::arg_type(i, "List[str]", "other"));
            };
            out.push(s.to_string());
        }
        Ok(out)
    }
    fn write_stdout(&mut self, s: &str) {
        self.vm.emit_out(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.vm.emit_err(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        // No flush seam here BY DESIGN: the streamed handles are unbuffered (one `write_all` + `flush`
        // per `print`, on the writer thread), so a `print("name? ", end="")` prompt is already on its
        // way out — and a fiber that WAITED for the writer would block on stdout's consumer, pinning a
        // core worker (the D5 invariant) for as long as a stalled reader cares to stall. See
        // [`stream`].
        self.vm.host.stdin.read_line()
    }
    fn read_all(&mut self) -> Result<String, crate::native::HostError> {
        self.vm.host.stdin.read_all()
    }
    fn read_char(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.vm.host.stdin.read_char()
    }
    fn os_args(&self) -> Vec<String> {
        self.vm.host.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
    fn os_environ(&self) -> Vec<(String, String)> {
        // Sort by key: the backing store is a `HashMap` (per-instance random iteration order), but
        // every other Chezzi map is deterministic and the serial/M:N engines each build their own
        // HashMap — an unsorted lowering would diverge run-to-run and between engines.
        let mut pairs: Vec<(String, String)> = self
            .vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
    fn os_setenv(&mut self, key: String, value: String) {
        // Writes the SHARED HostConfig env — the SAME map `os_env`/`os_environ` read, shared by Arc
        // across M:N workers — NOT `std::env::set_var` (a third, process-global-racy source). So a
        // `setenv` from any task is observed by both readers AND by the parent/siblings.
        self.vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
        // W7-8 — RAW bytes, not `display().to_string()`: a non-UTF-8 cwd used to come back U+FFFD-
        // substituted, naming nothing.
        std::env::current_dir()
            .map(|p| crate::native::fs::path_bytes(&p))
            .map_err(|e| crate::native::HostError {
                message: e.to_string(),
            })
    }
    fn request_exit(&mut self, code: i64) {
        // The LOW 8 BITS of the code, exactly like POSIX `exit(3)` / bash / Python / Go: `-1` → 255,
        // `300` → 44, `-256` → 0. NOT a clamp — clamping a negative code UP to 0 reported a failure
        // exit as SUCCESS to the shell/CI, and `os.exit(-1)` is the canonical failure idiom.
        let code = (code & 0xff) as i32;
        self.vm.pending_exit = Some(code);
        // W7-47 — also publish run-wide, AFTER the mask so it is applied exactly once. `pending_exit`
        // is per-`Vm`: for an eager `Executor` job it is a value only the join observes, and a `main`
        // parked in `accept()`/`recv()` never reaches the join. The run-scoped cell is what lets every
        // blocking loop see the exit (`Vm::run_exit_err`) — Go's `os.Exit` is immediate.
        //
        // W7-57 — the order is: code, then teardown, then flag.
        // 1. the code first, so anything woken by step 2 already reads it;
        // 2. halt every live nursery — a party that is SPINNING, `recv`-parked or asleep reaches no
        //    polling wait, so without this teardown it outlives the exit (a spinner forever);
        // 3. the flag LAST, so `true` publishes both of the above to whoever reads it.
        //
        // Step 3 does NOT make the cancel rung win for a scoped fiber, and an earlier revision of this
        // comment claimed it did. An `Acquire` load orders only the reads AFTER it, while both CPU
        // checkpoints read cancel BEFORE exit — so `cancel == false` + `exit == true` is legal, and the
        // measured result was a sibling `defer` that ran 2/8 times and once died mid-body. That
        // guarantee lives in [`Vm::exit_halt`], which decides from the flag's PRESENCE and so needs no
        // ordering; this sequence is only about publishing the code and the teardown before the hint.
        // TICKET-270 — a FIBER that exits counts itself as an exit origin BEFORE the code is
        // published: a joiner that reads the code and flushes finished output then waits in
        // `take_finished_streams` until this fiber's slot holds what it printed. The code itself stays
        // here, at the call: `classify_mn_outcome` trips this fiber's scope cancel before `finish`, and
        // a sibling woken by it must already read the exit, or it runs its `defer` (DEC-213). `main` is
        // not a fiber and holds its own output.
        if self.vm.fiber_scope.is_some() && !self.vm.exit_origin {
            self.vm.exit_origin = true;
            self.vm.quiesce.begin_exit_origin();
        }
        self.vm.quiesce.request_exit(code);
        self.vm.halt_run();
    }
}

// `is_numeric` / `as_f64` are now heap-aware methods on `Vm` (arith.rs) — a boxed `BigInt`/`FloatBox`
// is invisible to a free fn that can't reach the heap.

/// Format a float the way Chezzi prints it — CPython `repr()`/`str()` parity: fixed with an
/// always-present `.0` on integer-valued floats, scientific (`1e+16`, `1.5e+300`, `1e-05`) when the
/// decimal exponent is `< -4` or `>= 16`. Single-sourced in `fmtspec` alongside the `{:e}` spec path.
pub(super) fn format_float(x: f64) -> String {
    crate::fmtspec::repr_float(x)
}

/// [`strip_num_underscores_radix`] at base 10, for the base-10-only callers.
pub(super) fn strip_num_underscores(s: &str) -> Option<std::borrow::Cow<'_, str>> {
    strip_num_underscores_radix(s, 10)
}

/// Why a numeral was rejected by [`parse_i64_pep515`] — a malformed numeral and a well-formed one
/// outside i64 range get different messages at the caller (W12-18a), the same split
/// `math.parse_int_base` already makes for the same input.
pub(crate) enum IntParseErr {
    Malformed,
    Overflow,
}

/// The one PEP-515 (underscore-stripping) base-10 integer parse, shared by every `int()`/
/// `parse_int`-family caller, so overflow is diagnosed identically everywhere (W12-18a; DEC-098).
pub(crate) fn parse_i64_pep515(s: &str) -> Result<i64, IntParseErr> {
    let Some(t) = strip_num_underscores(s) else {
        return Err(IntParseErr::Malformed);
    };
    t.parse::<i64>().map_err(|e| match e.kind() {
        std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
            IntParseErr::Overflow
        }
        std::num::IntErrorKind::Empty
        | std::num::IntErrorKind::InvalidDigit
        | std::num::IntErrorKind::Zero
        | _ => IntParseErr::Malformed,
    })
}
