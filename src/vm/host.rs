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
    fn arg_opt_int(&mut self, i: usize) -> Result<Option<i64>, crate::native::HostError> {
        use crate::native::NativeArg as A;
        match self.args.get(i) {
            Some(A::None) => Ok(None),
            Some(A::Some(inner)) => match inner.as_ref() {
                A::Int(n) => Ok(Some(*n)),
                A::Float(_)
                | A::Bool(_)
                | A::Str(_)
                | A::Bytes(_)
                | A::Map(_)
                | A::List(_)
                | A::None
                | A::Some(_) => Err(crate::native::HostError::arg_type(i, "int?", "other")),
            },
            Some(_) => Err(crate::native::HostError::arg_type(i, "int?", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
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

/// M3.0a — one native-call argument, copied out of the heap: the answer of every typed `Host`
/// read of it. THE owner of those reads: [`VmHost`] copies an arg when a native reads it,
/// [`DoorHost`] serves the copies made before its GC door opened, and the dirty-pool offload takes
/// the primitive ones ([`ArgCopy::offload_form`]).
#[derive(Clone)]
pub(super) struct ArgCopy {
    shape: ArgShape,
    /// The arg's type name ([`Vm::type_name`]), for a wrong-type error.
    ty: &'static str,
}

#[derive(Clone)]
enum ArgShape {
    /// An int, inline or boxed.
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Bytes(Vec<u8>),
    Ptr(usize),
    /// A list: its strs in order, or `None` when an element is not a str.
    List(Option<Vec<String>>),
    /// A map: its `(key, value)` strs in insertion order, or `None` when one is not a str.
    Map(Option<Vec<(String, String)>>),
    /// A struct: its fields' C values in declaration order ([`ArgCopy::c_form`] of each), or
    /// `None` when a field has no C shape.
    Struct(Option<crate::native::NativeRet>),
    /// The absent value of a `T?` (`None`).
    NoneValue,
    /// A present `T?` (`?x`): its payload's shape.
    Some(Box<ArgShape>),
    /// Any other value: nil, a tuple, a closure, a handle.
    Other,
}

impl ArgCopy {
    fn wrong(&self, i: usize, want: &str) -> crate::native::HostError {
        crate::native::HostError::arg_type(i, want, self.ty)
    }
    fn is_int(&self) -> bool {
        matches!(self.shape, ArgShape::Int(_))
    }
    fn int(self, i: usize) -> Result<i64, crate::native::HostError> {
        if let ArgShape::Int(n) = self.shape {
            return Ok(n);
        }
        Err(crate::native::HostError::arg_type(i, "int", self.ty))
    }
    fn float(self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.shape {
            ArgShape::Float(f) => Ok(f),
            ArgShape::Int(n) => Ok(n as f64),
            ArgShape::Bool(_)
            | ArgShape::Str(_)
            | ArgShape::Bytes(_)
            | ArgShape::Ptr(_)
            | ArgShape::List(_)
            | ArgShape::Map(_)
            | ArgShape::Struct(_)
            | ArgShape::NoneValue
            | ArgShape::Some(_)
            | ArgShape::Other => Err(self.wrong(i, "float")),
        }
    }
    fn bool(self, i: usize) -> Result<bool, crate::native::HostError> {
        if let ArgShape::Bool(b) = self.shape {
            return Ok(b);
        }
        Err(crate::native::HostError::arg_type(i, "bool", self.ty))
    }
    fn ptr(self, i: usize) -> Result<usize, crate::native::HostError> {
        if let ArgShape::Ptr(a) = self.shape {
            return Ok(a);
        }
        Err(crate::native::HostError::arg_type(i, "ptr", self.ty))
    }
    fn str(self, i: usize) -> Result<String, crate::native::HostError> {
        if let ArgShape::Str(s) = self.shape {
            return Ok(s);
        }
        Err(crate::native::HostError::arg_type(i, "str", self.ty))
    }
    /// R1 — a `bytes` ONLY: the checker types every seam param `bytes`, and neither a `bytearray`
    /// (not assignable to a `bytes` sink — 7b29552) nor a `list[int]` (a `bytes()`-ctor
    /// convenience, not a seam contract) can reach here from typed code.
    fn bytes(self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        if let ArgShape::Bytes(b) = self.shape {
            return Ok(b);
        }
        Err(crate::native::HostError::arg_type(i, "bytes", self.ty))
    }
    fn c_value(self, i: usize) -> Result<crate::native::NativeRet, crate::native::HostError> {
        let ty = self.ty;
        self.c_form()
            .ok_or_else(|| crate::native::HostError::arg_type(i, "C value", ty))
    }
    /// The arg as a C-marshallable value for the FFI seam (`Host::arg_c_value`): a scalar
    /// (`int`/`float`/`bool`/`ptr`/`str`) as itself, a struct as a `NativeRet::List` of its
    /// fields. `None` for anything else. The one owner of that mapping.
    fn c_form(self) -> Option<crate::native::NativeRet> {
        use crate::native::NativeRet as N;
        match self.shape {
            ArgShape::Int(n) => Some(N::Int(n)),
            ArgShape::Float(f) => Some(N::Float(f)),
            ArgShape::Bool(b) => Some(N::Bool(b)),
            ArgShape::Ptr(a) => Some(N::Ptr(a)),
            ArgShape::Str(s) => Some(N::Str(s)),
            ArgShape::Struct(c) => c,
            ArgShape::Bytes(_)
            | ArgShape::List(_)
            | ArgShape::Map(_)
            | ArgShape::NoneValue
            | ArgShape::Some(_)
            | ArgShape::Other => None,
        }
    }
    /// In list order (it IS the argv). Every element must be a str.
    fn str_list(self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        match self.shape {
            ArgShape::List(Some(xs)) => Ok(xs),
            ArgShape::List(None) => {
                Err(crate::native::HostError::arg_type(i, "List[str]", "other"))
            }
            ArgShape::Int(_)
            | ArgShape::Float(_)
            | ArgShape::Bool(_)
            | ArgShape::Str(_)
            | ArgShape::Bytes(_)
            | ArgShape::Ptr(_)
            | ArgShape::Map(_)
            | ArgShape::Struct(_)
            | ArgShape::NoneValue
            | ArgShape::Some(_)
            | ArgShape::Other => Err(self.wrong(i, "List[str]")),
        }
    }
    /// In insertion order, so header order is deterministic and matches the off-heap host. Every
    /// key and value must be a str.
    fn str_map(self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.shape {
            ArgShape::Map(Some(pairs)) => Ok(pairs),
            ArgShape::Map(None) => Err(crate::native::HostError::arg_type(
                i,
                "Map[str, str]",
                "other",
            )),
            ArgShape::Int(_)
            | ArgShape::Float(_)
            | ArgShape::Bool(_)
            | ArgShape::Str(_)
            | ArgShape::Bytes(_)
            | ArgShape::Ptr(_)
            | ArgShape::List(_)
            | ArgShape::Struct(_)
            | ArgShape::NoneValue
            | ArgShape::Some(_)
            | ArgShape::Other => Err(self.wrong(i, "Map[str, str]")),
        }
    }
    /// An `int?`: `None` for the absent value, else the int of `?n`.
    pub(super) fn opt_int(self, i: usize) -> Result<Option<i64>, crate::native::HostError> {
        match self.shape {
            ArgShape::NoneValue => Ok(None),
            ArgShape::Some(inner) => ArgCopy {
                shape: *inner,
                ty: self.ty,
            }
            .int(i)
            .map(Some),
            ArgShape::Int(_)
            | ArgShape::Float(_)
            | ArgShape::Bool(_)
            | ArgShape::Str(_)
            | ArgShape::Bytes(_)
            | ArgShape::Ptr(_)
            | ArgShape::List(_)
            | ArgShape::Map(_)
            | ArgShape::Struct(_)
            | ArgShape::Other => Err(self.wrong(i, "int?")),
        }
    }
    /// D5 — the `Send` primitive form a dirty-pool thread serves ([`OffloadHost`]), or `None`
    /// when the arg has none (the call then runs inline: a safe fallback, never a fault).
    pub(super) fn offload_form(self) -> Option<crate::native::NativeArg> {
        use crate::native::NativeArg as A;
        let ty = self.ty;
        match self.shape {
            ArgShape::NoneValue => Some(A::None),
            ArgShape::Some(inner) => ArgCopy { shape: *inner, ty }
                .offload_form()
                .map(|a| A::Some(Box::new(a))),
            ArgShape::Int(n) => Some(A::Int(n)),
            ArgShape::Float(f) => Some(A::Float(f)),
            ArgShape::Bool(b) => Some(A::Bool(b)),
            ArgShape::Str(s) => Some(A::Str(s)),
            ArgShape::Bytes(b) => Some(A::Bytes(b)),
            ArgShape::List(xs) => xs.map(A::List),
            ArgShape::Map(pairs) => pairs.map(A::Map),
            ArgShape::Ptr(_) | ArgShape::Struct(_) | ArgShape::Other => None,
        }
    }
}

impl Vm {
    /// `v` copied out of the heap ([`ArgCopy`]).
    pub(super) fn copy_arg(&self, v: Value) -> ArgCopy {
        let str_of = |e: Value| match e.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let shape = if let Some(n) = self.int_val(v) {
            ArgShape::Int(n)
        } else if v.is_float() {
            ArgShape::Float(self.float_of(v))
        } else if let Some(b) = v.as_bool() {
            ArgShape::Bool(b)
        } else if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Str(s) => ArgShape::Str(s.to_string()),
                Obj::Bytes(b) => ArgShape::Bytes(b.to_vec()),
                Obj::Ptr(a) => ArgShape::Ptr(*a),
                Obj::List(items) => ArgShape::List(items.iter().map(str_of).collect()),
                Obj::Map(m) => ArgShape::Map(
                    m.entries
                        .iter()
                        .map(|(_, k, mv)| Some((str_of(k)?, str_of(mv)?)))
                        .collect(),
                ),
                Obj::Struct { fields, .. } => ArgShape::Struct(
                    fields
                        .iter()
                        .map(|f| self.copy_arg(f).c_form())
                        .collect::<Option<Vec<_>>>()
                        .map(crate::native::NativeRet::List),
                ),
                // A carrier `T?` (the only `Option` a native param declares).
                Obj::Enum {
                    variant_id,
                    payload,
                } if *variant_id == crate::base::op::VID_NONE_VARIANT && payload.is_empty() => {
                    ArgShape::NoneValue
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } if *variant_id == crate::base::op::VID_SOME && payload.len() == 1 => {
                    ArgShape::Some(Box::new(self.copy_arg(payload.get(0)).shape))
                }
                Obj::ByteArray(_)
                | Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::Tuple(_)
                | Obj::Set(_)
                | Obj::Enum { .. }
                | Obj::Cell(_)
                | Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module(_)
                | Obj::Native { .. }
                | Obj::Builtin(_)
                | Obj::Cffi(_)
                | Obj::Channel(_)
                | Obj::Shared(_)
                | Obj::RwShared(_)
                | Obj::Atomic(_)
                | Obj::AtomicInt(_)
                | Obj::Executor(_)
                | Obj::Socket(_)
                | Obj::Listener(_)
                | Obj::Writer(_)
                | Obj::Reader(_)
                | Obj::Generator(_) => ArgShape::Other,
            }
        } else {
            ArgShape::Other
        };
        ArgCopy {
            shape,
            ty: self.type_name(v),
        }
    }

    /// Every arg of a call, copied out before its GC door opens ([`DoorHost`]).
    pub(super) fn copy_args(&self, args: &[Value]) -> Vec<ArgCopy> {
        args.iter().map(|&v| self.copy_arg(v)).collect()
    }

    /// Run the Chezzi closure `callee` on the C scalars `args` that C handed a callback
    /// trampoline, and lower its result back to a [`crate::native::NativeRet`]. Same-thread,
    /// synchronous, through the proven `guarded`+`invoke_value` re-entry path (the one
    /// map/filter/sort use). No span is available at the FFI boundary; a zero span is used.
    fn run_c_callback(
        &mut self,
        callee: Value,
        args: &[crate::native::NativeRet],
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        // Lower the C scalar args to engine `Value`s (allocations happen here, at the boundary).
        let vals: Vec<Value> = args.iter().map(|a| self.lower_native(a.clone())).collect();
        let result = self
            .guarded(|vm| vm.invoke_value(callee, vals, Span::default()))
            .map_err(|e| crate::native::HostError { message: e.message })?;
        Ok(self.value_to_native_ret(result))
    }
}

/// `std.os.getenv` from the run's shared env.
fn env_get(host: &crate::native::HostConfig, key: &str) -> Option<String> {
    host.env
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

/// `std.os.environ`. Sorted by key: the backing store is a `HashMap` (per-instance random
/// iteration order), but every other Chezzi map is deterministic — an unsorted lowering would
/// diverge run-to-run.
fn env_all(host: &crate::native::HostConfig) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = host
        .env
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// `std.os.setenv`. Writes the SHARED HostConfig env — the SAME map `os_env`/`os_environ` read,
/// shared by Arc across M:N workers — NOT `std::env::set_var` (a third, process-global-racy
/// source). So a `setenv` from any task is observed by both readers AND by the parent/siblings.
fn env_set(host: &crate::native::HostConfig, key: String, value: String) {
    host.env
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, value);
}

/// `std.os.getcwd`. W7-8 — RAW bytes, not `display().to_string()`: a non-UTF-8 cwd used to come
/// back U+FFFD-substituted, naming nothing.
fn cwd_bytes() -> Result<Vec<u8>, crate::native::HostError> {
    std::env::current_dir()
        .map(|p| crate::native::fs::path_bytes(&p))
        .map_err(|e| crate::native::HostError {
            message: e.to_string(),
        })
}

/// M3.0a — the [`crate::native::Host`] of a native that runs in the wait door ([`Vm::wait_door`]):
/// a native that holds its host thread, and an extern C call. It serves the args copied out of
/// the heap before the door opened ([`Vm::copy_args`]) and the Rust-owned host state, through the
/// door's [`super::gc_door::Detached`] handle, which reaches no heap. A C callback and an exit
/// re-attach for their span ([`super::gc_door::Detached::reenter`]).
pub(super) struct DoorHost<'d, 'v> {
    pub(super) door: &'d mut super::gc_door::Detached<'v>,
    pub(super) args: Vec<ArgCopy>,
    /// The call's arg values, read only attached: the callee of a C callback (an extern call
    /// pins them in its door). Empty for a native.
    pub(super) vals: Vec<Value>,
}

impl DoorHost<'_, '_> {
    fn arg(&self, i: usize) -> Result<ArgCopy, crate::native::HostError> {
        self.args
            .get(i)
            .cloned()
            .ok_or_else(|| crate::native::HostError::missing_arg(i))
    }
}

impl crate::native::Host for DoorHost<'_, '_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        self.arg(i)?.int(i)
    }
    fn arg_is_int(&self, i: usize) -> bool {
        self.args.get(i).is_some_and(ArgCopy::is_int)
    }
    fn arg_opt_int(&mut self, i: usize) -> Result<Option<i64>, crate::native::HostError> {
        self.arg(i)?.opt_int(i)
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        self.arg(i)?.float(i)
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        self.arg(i)?.bool(i)
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        self.arg(i)?.ptr(i)
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        self.arg(i)?.str(i)
    }
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        self.arg(i)?.bytes(i)
    }
    fn arg_c_value(
        &mut self,
        i: usize,
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        self.arg(i)?.c_value(i)
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        self.arg(i)?.str_map(i)
    }
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        self.arg(i)?.str_list(i)
    }
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[crate::native::NativeRet],
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        // The Chezzi closure passed as extern arg `arg_index` (a function pointer to C). This fires
        // inside the extern `ffi_call` on the calling thread. M1.8 — the extern call waits in the
        // GC door: the callback runs attached (Go `cgocallback`), and the call's args stay pinned
        // after it.
        let callee = *self
            .vals
            .get(arg_index)
            .ok_or_else(|| crate::native::HostError {
                message: format!("callback argument {arg_index} is missing"),
            })?;
        let pins: Vec<GcRef> = self.vals.iter().filter_map(|v| v.child_gcref()).collect();
        self.door
            .reenter(&pins, |vm| vm.run_c_callback(callee, args))
    }
    fn write_stdout(&mut self, s: &str) {
        self.door.emit_out(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.door.emit_err(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.door.host().stdin.read_line()
    }
    fn read_all(&mut self) -> Result<String, crate::native::HostError> {
        self.door.host().stdin.read_all()
    }
    fn read_char(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.door.host().stdin.read_char()
    }
    fn os_args(&self) -> Vec<String> {
        self.door.host_ref().args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        env_get(self.door.host_ref(), key)
    }
    fn os_environ(&self) -> Vec<(String, String)> {
        env_all(self.door.host_ref())
    }
    fn os_setenv(&mut self, key: String, value: String) {
        env_set(self.door.host_ref(), key, value);
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
        cwd_bytes()
    }
    fn request_exit(&mut self, code: i64) {
        // No native that runs in the door exits today; one that does re-attaches for the exit,
        // whose teardown reads the `Vm`.
        self.door.reenter(&[], |vm| {
            crate::native::Host::request_exit(
                &mut VmHost {
                    vm,
                    args: Vec::new(),
                },
                code,
            )
        });
    }
}

/// The VM's [`crate::native::Host`] adapter for a native run attached: lets a native fn read the
/// evaluated `Value` arguments ([`Vm::copy_arg`] copies one per read) and write to the captured
/// output buffers. Holds `&mut Vm` plus the arg vector; it allocates nothing itself — the returned
/// [`crate::native::NativeRet`] is lowered to heap objects by [`Vm::lower_native`] after the call
/// returns.
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

    /// `args[i]` copied out of the heap.
    fn arg(&self, i: usize) -> Result<ArgCopy, crate::native::HostError> {
        match self.args.get(i) {
            Some(&v) => Ok(self.vm.copy_arg(v)),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
}

impl crate::native::Host for VmHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        self.arg(i)?.int(i)
    }
    fn arg_is_int(&self, i: usize) -> bool {
        self.args.get(i).is_some_and(|v| self.vm.is_integral(*v))
    }
    fn arg_opt_int(&mut self, i: usize) -> Result<Option<i64>, crate::native::HostError> {
        self.arg(i)?.opt_int(i)
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        self.arg(i)?.float(i)
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        self.arg(i)?.bool(i)
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        self.arg(i)?.ptr(i)
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        self.arg(i)?.str(i)
    }
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        self.arg(i)?.bytes(i)
    }
    fn arg_c_value(
        &mut self,
        i: usize,
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        self.arg(i)?.c_value(i)
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        self.arg(i)?.str_map(i)
    }
    fn arg_settle_channel(&mut self, ch: usize, v: usize) -> Result<(), crate::native::HostError> {
        let h = self.arg_channel(ch)?;
        let Some(val) = self.args.get(v).copied() else {
            return Err(crate::native::HostError::missing_arg(v));
        };
        let core = self.vm.channel_core(h);
        // M3.2 — the outcome is sealed by handle: every `get()` reader shares the job's value.
        let sealed = core.q.lock().unwrap_or_else(|e| e.into_inner()).seal(val);
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
        self.arg(i)?.str_list(i)
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
        env_get(&self.vm.host, key)
    }
    fn os_environ(&self) -> Vec<(String, String)> {
        env_all(&self.vm.host)
    }
    fn os_setenv(&mut self, key: String, value: String) {
        env_set(&self.vm.host, key, value);
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
        cwd_bytes()
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
        // W7-57 — the order is: code, then flag, then teardown (`Vm::halt_run`).
        // 1. the code first, so anything woken by step 3 already reads it;
        // 2. the flag, so `true` publishes the code, and a call that returns during step 3 reads it;
        // 3. halt every live nursery — a party that is SPINNING, `recv`-parked or asleep reaches no
        //    polling wait, so without this teardown it outlives the exit (a spinner forever).
        //
        // The flag does NOT make the cancel rung win for a scoped fiber, and an earlier revision of this
        // comment claimed it did. An `Acquire` load orders only the reads AFTER it, while both CPU
        // checkpoints read cancel BEFORE exit — so `cancel == false` + `exit == true` is legal, and the
        // measured result was a sibling `defer` that ran 2/8 times and once died mid-body. That
        // guarantee lives in [`Vm::exit_halt`], which decides from the flag's PRESENCE and so needs no
        // ordering; this sequence is only about publishing the code and the teardown before the hint.
        // TICKET-270 — a FIBER that exits counts itself as an exit origin BEFORE the code is
        // published: a joiner that reads the code and flushes finished output then waits in
        // `take_finished_streams` until this fiber's slot holds what it printed. The code itself stays
        // here, at the call: `MnSched::finish_with` trips this fiber's scope cancel, and
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

/// The one PEP-515 (underscore-stripping) base-10 integer parse, shared by every
/// `to_int`-family caller, so overflow is diagnosed identically everywhere (W12-18a; DEC-098).
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
