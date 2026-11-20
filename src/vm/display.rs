//! VM: type names, display, and the `print` / `str` render: a walk whose `str` hooks run as
//! frames that can park.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::stmt::BoxCore;
use super::*;
use crate::base::hooks::Hook;

/// `Writer(open)` / `Reader(closed)`: an I/O handle's text, without its fd.
fn io_handle_text(label: &str, open: bool) -> String {
    format!("{label}({})", if open { "open" } else { "closed" })
}

/// M1.8 — whether an I/O handle's `inner` slot is open, with no wait: `None` while another thread
/// holds the lock. A read or a write holds it while it waits in the GC door.
fn io_state<T>(inner: &std::sync::Mutex<Option<T>>) -> Option<bool> {
    match inner.try_lock() {
        Ok(g) => Some(g.is_some()),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner().is_some()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// [`io_state`] for a `&self` render (error text, a box's contents), which cannot take the GC
/// door: a short spin, then `open`. That is the true state: a holder that stays longer waits on a
/// syscall, and only an op on an open slot makes one. An op on a closed slot returns at once, and
/// a close drops the backing (a flush that can block) after it released the lock
/// (`fileio::close_slot`, M3.0a C7).
fn io_state_now<T>(inner: &std::sync::Mutex<Option<T>>) -> bool {
    for _ in 0..64 {
        if let Some(open) = io_state(inner) {
            return open;
        }
        std::thread::yield_now();
    }
    true
}

/// What a `&self` render ([`Vm::display_guarded`]) is inside: the boxes (core addresses) and, inside
/// a box, the containers on its path. A back edge to either renders as a marker, not a deeper walk.
/// Containers are tracked only inside a box (M3.5, in place of the deleted wire form's
/// back-references): outside one, the depth guard stays the rule, and the print path keeps its own
/// `repr_active`.
#[derive(Default)]
pub(super) struct OpenRender {
    boxes: Vec<usize>,
    objs: Vec<GcRef>,
}

impl OpenRender {
    /// `h` is a container this render already opened inside a box.
    fn is_back_edge(&self, h: GcRef) -> bool {
        !self.boxes.is_empty() && self.objs.contains(&h)
    }

    /// Open container `h` for its walk; returns whether it was recorded (only inside a box).
    fn enter(&mut self, h: GcRef) -> bool {
        let inside = !self.boxes.is_empty();
        if inside {
            self.objs.push(h);
        }
        inside
    }

    /// Close what [`enter`](Self::enter) opened.
    fn leave(&mut self, opened: bool) {
        if opened {
            self.objs.pop();
        }
    }
}

impl Vm {
    /// [`io_state`] for the print path: a busy handle is waited for as its fd's wait (`waits`), so
    /// the holder's FIFO peer runs at one worker too.
    fn io_state_door<T>(
        &mut self,
        inner: &std::sync::Mutex<Option<T>>,
        waits: bool,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        match io_state(inner) {
            Some(open) => Ok(open),
            None => self.wait_once(WaitSpec::of_fd(waits), "a busy handle", span, &[], |_| {
                inner.lock().unwrap_or_else(|e| e.into_inner()).is_some()
            }),
        }
    }

    pub(super) fn type_name(&self, v: Value) -> &'static str {
        // A boxed float is Float-tagged → `view` yields `Obj(h)` → the `Obj::FloatBox => "float"` arm
        // below names it (a boxed `BigInt` likewise → "int").
        match v.view() {
            ValueView::Int(_) => "int",
            ValueView::Bool(_) => "bool",
            ValueView::Nil => Self::NONE_TEXT,
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::Str(_) => "str",
                Obj::Bytes(_) => "bytes",
                Obj::ByteArray(_) => "bytearray",
                // Boxed scalars report the same type name as the inline `Int`/`Float`.
                Obj::BigInt(_) => "int",
                Obj::FloatBox(_) => "float",
                Obj::List(_) => "List",
                Obj::Tuple(_) => "tuple",
                Obj::Map(_) => "Map",
                Obj::Set(_) => "Set",
                Obj::Struct { .. } => "struct",
                Obj::Enum { variant_id, .. } => Self::carrier_word(*variant_id).unwrap_or("enum"),
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module(_) => "module",
                Obj::Native { .. } => "function",
                Obj::Builtin(_) => "function",
                Obj::Cffi(_) => "function",
                Obj::Ptr(_) => "ptr",
                Obj::Channel(_) => "Channel",
                o @ (Obj::Shared(_)
                | Obj::RwShared(_)
                | Obj::Atomic(_)
                | Obj::AtomicInt(_)
                | Obj::Executor(_)
                | Obj::Socket(_)
                | Obj::Listener(_)
                | Obj::Writer(_)
                | Obj::Reader(_)) => o.handle_kind().map_or("handle", HandleKind::name),
                Obj::Generator(_) => "generator",
                // A cell is a transparent by-reference box (never a user-visible operand — reads
                // `CellLoad` first); defensively report its inner value's type.
                Obj::Cell(v) => self.type_name(v.get()),
                Obj::Iter { .. } => "iterator",
            },
        }
    }

    /// `Display` form. Thin wrapper over the depth-guarded worker — kept infallible so every
    /// error-message caller is unchanged; a structure too deep renders as `<...>` here (the print
    /// path surfaces the error).
    pub(super) fn display(&self, v: Value) -> String {
        self.display_guarded(v, 0)
            .unwrap_or_else(|_| "<...>".to_string())
    }

    /// Depth-guarded structural display. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding deep data from overflowing the host stack.
    pub(super) fn display_guarded(&self, v: Value, depth: usize) -> Result<String, RuntimeError> {
        self.display_walk(v, depth, &mut OpenRender::default())
    }

    /// [`display_guarded`](Vm::display_guarded) with what the walk has `open`: a box, or a container
    /// inside a box, met again inside its own render is a back edge, and renders as the print path's
    /// marker (`[...]`, `{...}`, `Set(...)`, `...`, `Shared(...)`; W14-35c, TICKET-246).
    fn display_walk(
        &self,
        v: Value,
        depth: usize,
        open: &mut OpenRender,
    ) -> Result<String, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::RUNTIME));
        }
        match v.view() {
            ValueView::Int(n) => Ok(n.to_string()),
            ValueView::Bool(b) => Ok(b.to_string()),
            ValueView::Nil => Ok(Self::NONE_TEXT.to_string()),
            // A boxed float/big-int is heap-tagged → the `Obj::FloatBox`/`Obj::BigInt` arms below.
            ValueView::Obj(h) => match self.heap.get(h) {
                // NESTED (`depth > 0`) means this string sits inside a container / field / payload,
                // so it renders as its `repr` — same rule as [`Vm::str_walk`]'s `nested`, applied here
                // by depth alone because this renderer is `&self` and has no display-hook path that
                // preserves depth. A top-level `display` of a `str` stays its bare characters.
                Obj::Str(s) if depth > 0 => Ok(crate::slice::str_repr(s)),
                Obj::Str(s) => Ok(s.to_string()),
                // Boxed scalars render identically to the inline `Int`/`Float`.
                Obj::BigInt(n) => Ok(n.to_string()),
                Obj::FloatBox(f) => Ok(format_float(*f)),
                // Python `bytes` repr `b'...'` — shared via `slice::bytes_repr`.
                Obj::Bytes(b) => Ok(crate::slice::bytes_repr(b)),
                // Python `bytearray` repr `bytearray(b'...')` — shared via `slice::bytearray_repr`.
                Obj::ByteArray(b) => Ok(crate::slice::bytearray_repr(&b.to_vec())),
                Obj::List(_) if open.is_back_edge(h) => Ok("[...]".to_string()),
                Obj::List(items) => {
                    // The collect stops at the first fault; `leave` runs on both paths.
                    let opened = open.enter(h);
                    let parts = items
                        .into_iter()
                        .map(|v| self.display_walk(v, depth + 1, open))
                        .collect::<Result<Vec<_>, _>>();
                    open.leave(opened);
                    Ok(format!("[{}]", parts?.join(", ")))
                }
                Obj::Tuple(items) => {
                    let n = items.len();
                    let mut parts = Vec::with_capacity(n);
                    for v in items {
                        parts.push(self.display_walk(*v, depth + 1, open)?);
                    }
                    Ok(crate::base::words::tuple_text(parts))
                }
                Obj::Map(_) if open.is_back_edge(h) => Ok("{...}".to_string()),
                Obj::Map(m) => {
                    let opened = open.enter(h);
                    let parts = (&m.entries)
                        .into_iter()
                        .map(|(_, k, v)| {
                            let k = self.display_walk(k, depth + 1, open)?;
                            Ok(format!("{k}: {}", self.display_walk(v, depth + 1, open)?))
                        })
                        .collect::<Result<Vec<_>, RuntimeError>>();
                    open.leave(opened);
                    Ok(format!("{{{}}}", parts?.join(", ")))
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        Ok("Set()".to_string())
                    } else if open.is_back_edge(h) {
                        Ok("Set(...)".to_string())
                    } else {
                        let opened = open.enter(h);
                        let parts = (&s.entries)
                            .into_iter()
                            .map(|(_, v)| self.display_walk(v, depth + 1, open))
                            .collect::<Result<Vec<_>, _>>();
                        open.leave(opened);
                        Ok(format!("{{{}}}", parts?.join(", ")))
                    }
                }
                Obj::Struct { .. } if open.is_back_edge(h) => Ok("...".to_string()),
                Obj::Struct { tid, fields, .. } => {
                    // Positional layout: recover declaration-order field names from the StructDef
                    // (cold display path). Resolve the type key from `tid`; snapshot values to drop
                    // the heap borrow.
                    let name = self.struct_name_of_tid(*tid);
                    let vals: Vec<Value> = fields.to_vec();
                    // ROOT REDESIGN — render the BARE display name (not the qualified identity key);
                    // `name` is the key the StructDef is stored under. Fall back to stripping the key.
                    let (display, names): (String, Vec<String>) = self
                        .program
                        .structs
                        .get(name)
                        .map(|d| (d.display_name.clone(), d.fields.clone()))
                        .unwrap_or_else(|| (crate::base::words::bare_display(name), Vec::new()));
                    let opened = open.enter(h);
                    let parts = vals
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let k = names.get(i).cloned().unwrap_or_else(|| i.to_string());
                            Ok(format!("{k}={}", self.display_walk(*v, depth + 1, open)?))
                        })
                        .collect::<Result<Vec<_>, RuntimeError>>();
                    open.leave(opened);
                    Ok(format!("{display}({})", parts?.join(", ")))
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // A carrier prints its prefix, then its payload in the mode of the position
                    // the carrier itself occupies (top level: bare; nested: repr).
                    if let Some(prefix) = self.carrier_prefix(*variant_id, payload) {
                        let inner = match payload.try_get(0) {
                            Some(v) => {
                                self.display_walk(v, if depth == 0 { 0 } else { depth + 1 }, open)?
                            }
                            None => Self::NONE_TEXT.to_string(),
                        };
                        return Ok(format!("{prefix}{inner}"));
                    }
                    // M19 lever #2 — recover the variant name from the id (cold display path).
                    let variant = self.enum_names(*variant_id).1.to_string();
                    let payload: Vec<Value> = payload.to_vec();
                    if payload.is_empty() {
                        Ok(variant)
                    } else {
                        let mut parts = Vec::with_capacity(payload.len());
                        for v in &payload {
                            parts.push(self.display_walk(*v, depth + 1, open)?);
                        }
                        Ok(format!("{variant}({})", parts.join(", ")))
                    }
                }
                // Raw display fallback (no method dispatch here): `Name(inner)`. The `str(self)`
                // override is honored by `stringify` (the path print and interpolation use).
                Obj::Func { proto, .. } => Ok(format!(
                    "<fn {}>",
                    crate::base::provider_name::display_fn_name(&self.program.protos[*proto].name)
                )),
                Obj::Closure { .. } => Ok("<closure>".to_string()),
                Obj::Module(m) => Ok(format!("<module {}>", m.names().name)),
                Obj::Native { name, .. } => Ok(format!("<native fn {name}>")),
                Obj::Builtin(b) => Ok(format!("<builtin fn {}>", b.name())),
                Obj::Cffi(c) => Ok(format!("<extern fn {}>", c.name())),
                // A raw address is non-deterministic (differs per run), so never render it — a
                // printed pointer's value would not be reproducible. Only null vs live (a
                // deterministic distinction) is observable.
                Obj::Ptr(a) => Ok(if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }),
                // TICKET-185 — `len()` counts the buffer only: a parked sender's offer stays
                // invisible here too, matching `Channel.len()`.
                Obj::Channel(core) => Ok(format!("Channel(len={})", core.q.lock().unwrap().len())),
                // A box renders through `display_box` only (TICKET-246).
                Obj::Shared(core) => self.display_box(BoxCore::Shared(core), depth, open),
                Obj::RwShared(core) => self.display_box(BoxCore::RwShared(core), depth, open),
                Obj::Atomic(core) => self.display_box(BoxCore::Atomic(core), depth, open),
                Obj::AtomicInt(core) => Ok(format!(
                    "AtomicInt({})",
                    core.v.load(std::sync::atomic::Ordering::SeqCst)
                )),
                // Work not yet finished: the jobs submitted and not yet ended.
                Obj::Executor(core) => Ok(format!("Executor(pending={})", core.outstanding())),
                // D6: render open/closed without exposing the fd; mirrors the core handles'
                // structural
                // `Display`.
                Obj::Socket(core) => Ok(format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Listener(core) => Ok(format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                // R2: render open/closed without exposing the fd (mirrors the socket handles).
                Obj::Writer(core) => Ok(io_handle_text("Writer", io_state_now(&core.inner))),
                // R2b: render open/closed without exposing the fd (mirrors the Writer handle).
                Obj::Reader(core) => Ok(io_handle_text("Reader", io_state_now(&core.inner))),
                Obj::Generator(_) => Ok("<generator>".to_string()),
                // A cell is a transparent by-reference box (never a user-visible operand — reads
                // `CellLoad` first); defensively display its inner value.
                Obj::Cell(v) => self.display_walk(v.get(), depth, open),
                Obj::Iter { .. } => Ok("<iterator>".to_string()),
            },
        }
    }

    /// TICKET-246 — the ONE renderer of a `Shared` / `RwShared` / `Atomic`, for every path that
    /// renders one. Its rule:
    ///
    /// 1. a box already open in this render renders `Shared(...)`, like any container cycle;
    /// 2. it takes no lock and no guard: it reads the box's one slot (M3.3), as Go's `fmt` reads a
    ///    struct's fields, and renders that value with no safepoint in between;
    /// 3. contents render structurally, with no user `str` hook: a hook is user code.
    pub(super) fn display_box(
        &self,
        core: BoxCore,
        depth: usize,
        open: &mut OpenRender,
    ) -> Result<String, RuntimeError> {
        let (label, addr) = (core.label(), core.addr());
        if open.boxes.contains(&addr) {
            return Ok(format!("{label}(...)"));
        }
        open.boxes.push(addr);
        let inner = self.display_walk(core.peek(), depth + 1, open);
        open.boxes.pop();
        Ok(format!("{label}({})", inner?))
    }

    /// A box with nothing open around it (the print path's own walk keeps no [`OpenRender`]).
    fn display_box_alone(&self, core: BoxCore, depth: usize) -> Result<String, RuntimeError> {
        self.display_box(core, depth, &mut OpenRender::default())
    }

    /// Protocol-aware render for `print` / interpolation / `str()` with no frame to resume: a
    /// struct or enum with a `str` hook (the `Stringable` protocol) dispatches to it, nested
    /// (`guarded_walk`); everything else takes the default structural repr. Distinct from the
    /// `&self` `display` above, which stays the pure structural form for error/debug text.
    pub(super) fn stringify(
        &mut self,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<String, RuntimeError> {
        let mut s = String::new();
        self.stringify_into(&mut s, v, span, depth)?;
        Ok(s)
    }

    /// [`Vm::stringify`] appending onto `out`: a top-level (bare) render, a `str` its characters.
    pub(super) fn stringify_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.str_walk(out, v, false, depth, None, &[], span)
            .map(|_| ())
    }

    /// `Op::ToStr`: the value on the stack top becomes its str. Out of line for `step`'s frame
    /// (commit 1450077).
    #[inline(never)]
    pub(super) fn op_to_str(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.stack[self.stack.len() - 1]; // leave rooted until the str replaces it
        let mut out = String::new();
        if self.str_walk(&mut out, v, false, 0, Some(StrThen::ToStr), &[], span)? {
            self.str_finish(StrThen::ToStr, out, span)?;
        }
        Ok(())
    }

    /// `Op::BuildStr`: parts `i..` of the `n` on the stack top render onto `s` (one buffer, no
    /// per-part `String`); then the str replaces them. A part whose render parks keeps `[n, i]`
    /// below its walk ([`StrThen::Build`]). Each part stays rooted in place while it renders.
    /// Out of line for `step`'s frame (commit 1450077).
    #[inline(never)]
    pub(super) fn build_str(
        &mut self,
        n: usize,
        i: usize,
        mut s: String,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - n;
        for i in i..n {
            let p = self.stack[at + i];
            let state = [Value::int(n as i64), Value::int(i as i64)];
            if !self.str_walk(&mut s, p, false, 0, Some(StrThen::Build), &state, span)? {
                return Ok(());
            }
        }
        self.stack.truncate(at);
        let h = self.heap.alloc(Obj::Str(s.into()));
        self.push(Value::obj(h));
        Ok(())
    }

    /// `Op::Assert`, which only runs on its failing path: a comparison's operands render
    /// ([`Vm::assert_text`]), then it faults ([`Vm::assert_fault`]). Out of line for `step`'s
    /// frame (commit 1450077).
    #[cold]
    #[inline(never)]
    pub(super) fn op_assert(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.assert_op().1 {
            Some(_) => self.assert_text(0, String::new(), span),
            None => Err(self.assert_fault(None, span)),
        }
    }

    /// The top frame's failing `Op::Assert`: its `has_msg` and `cmp`.
    fn assert_op(&self) -> (bool, Option<crate::base::op::AssertCmp>) {
        let f = self.frames.last().expect("an assert runs in a frame");
        let Op::Assert { has_msg, cmp } = &self.program.protos[f.proto].code[f.ip - 1] else {
            unreachable!("an assert text is rendered only by Op::Assert")
        };
        (*has_msg, *cmp)
    }

    /// A failed comparison `assert`'s text from operand `side` on, onto `s`: `l`, ` <op> `, `r`,
    /// each nested; then its fault. The two operand values sit on the stack under `msg`
    /// (duplicated by the compiler before the comparison consumed the originals), rooted until the
    /// last render ends. A render that parks goes on in [`StrThen::Assert`]. One that faults drops
    /// the operand text only, and so does a fault in its `str` hook frame ([`Vm::fault_in`]): a
    /// faulting operand never makes a failed assertion an error.
    pub(super) fn assert_text(
        &mut self,
        side: usize,
        mut s: String,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let (has_msg, cmp) = self.assert_op();
        let symbol = cmp.expect("a comparison assert").symbol();
        let at = self.stack.len() - 2 - usize::from(has_msg);
        for side in side..2 {
            if side == 1 {
                s.push(' ');
                s.push_str(symbol);
                s.push(' ');
            }
            let v = self.stack[at + side];
            let then = StrThen::Assert { right: side == 1 };
            match self.str_walk(&mut s, v, true, 0, Some(then), &[], span) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(_) => return Err(self.assert_fault(None, span)),
            }
        }
        Err(self.assert_fault(Some(s), span))
    }

    /// THE fault of a failed `assert`, with its comparison's text if that rendered: its operands
    /// and `msg` leave the stack. The one site that flags an assert failure: the `test fn` runner
    /// buckets it as FAIL, every other fault (`is_assert: false`) as ERROR.
    pub(super) fn assert_fault(&mut self, cmp_text: Option<String>, span: Span) -> RuntimeError {
        let (has_msg, cmp) = self.assert_op();
        let n = self.stack.len();
        let msg = if has_msg {
            self.val_str(self.stack[n - 1])
        } else {
            None
        };
        self.stack
            .truncate(n - usize::from(has_msg) - if cmp.is_some() { 2 } else { 0 });
        let message = match (msg, cmp_text) {
            (Some(m), Some(c)) => format!("assertion failed: {m} ({c})"),
            (Some(m), None) => format!("assertion failed: {m}"),
            (None, Some(c)) => format!("assertion failed: {c}"),
            (None, None) => "assertion failed".to_string(),
        };
        RuntimeError {
            message,
            span,
            is_assert: true,
            is_over_memory: false,
            is_timed_out: false,
            is_panic: false,
        }
    }

    /// The fault `e` left a failed `assert`'s operand `str` hook frame (its walk's header on the
    /// stack top): the walk's slots go, and the assert faults with no operand text
    /// ([`Vm::assert_text`]; base ran the hook nested and dropped a faulting render's text). A halt
    /// [`Vm::cleanup_halt`] keeps is `Err`, itself.
    pub(super) fn assert_hook_fault(
        &mut self,
        e: RuntimeError,
        span: Span,
    ) -> Result<RuntimeError, RuntimeError> {
        self.cleanup_halt(e)?;
        let n = self.stack.len();
        let levels = self.stack[n - 2].as_int() as usize;
        self.stack.truncate(n - STR_HEADER - levels * STR_LEVEL - 2);
        Ok(self.assert_fault(None, span))
    }

    /// The fault of a map lookup that missed `key`, the key rendered as a nested element renders
    /// (a `str` quoted, an `int` bare). `flat` (the caller runs on the dispatch loop): a `str` hook
    /// runs as a frame that can park, and the fault waits for its return ([`StrThen::KeyText`]).
    /// The key stays rooted in the render's slot.
    pub(super) fn key_not_found(
        &mut self,
        flat: bool,
        key: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let mut ks = String::new();
        let then = flat.then_some(StrThen::KeyText);
        if self.str_walk(&mut ks, key, true, 0, then, &[], span)? {
            return Err(self.key_fault(&ks, span));
        }
        Ok(())
    }

    /// `key not found: <key>`, the key's text rendered.
    fn key_fault(&self, key: &str, span: Span) -> RuntimeError {
        self.err(format!("key not found: {key}"), span)
    }

    /// `Op::ToStrFmt` — render the top-of-stack value per the parsed format spec. Scalars map
    /// straight to a [`crate::fmtspec::FmtArg`]; anything else renders first (a `str` hook's result
    /// included; rooted on the operand stack), then formats as a plain string. A LITERAL
    /// width/precision is already capped at compile time. A nested `{expr}` one (`{s:<{w}}`) sits
    /// on the stack above the value — width, then precision — and is resolved and capped by
    /// [`crate::fmtspec::field_from_int`] BEFORE anything is rendered or allocated, so a huge
    /// runtime width is a clean error, never an OOM. Lives in its own `#[inline(never)]` helper to
    /// keep `step`'s frame small (commit 1450077). A render that parks finishes in
    /// [`Vm::fmt_finish`] ([`StrThen::Fmt`]).
    #[inline(never)]
    pub(super) fn op_to_str_fmt(
        &mut self,
        spec: &crate::fmtspec::FormatSpec,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let nested = spec.dyn_width as usize + spec.dyn_precision as usize;
        let v = self.stack[self.stack.len() - 1 - nested];
        let scalar = self.int_val(v).is_some()
            || v.is_float()
            || v.as_obj()
                .is_some_and(|h| matches!(self.heap.get(h), Obj::Str(_)));
        if !scalar {
            self.fmt_fields(spec, span)?;
            let mut out = String::new();
            if self.str_walk(&mut out, v, false, 0, Some(StrThen::Fmt), &[], span)? {
                self.fmt_finish(spec, out, span)?;
            }
            return Ok(());
        }
        let resolved = self.fmt_fields(spec, span)?;
        for _ in 0..nested {
            self.pop();
        }
        let spec = resolved.as_ref().unwrap_or(spec);
        let mut out = String::new();
        if let Some(n) = self.int_val(v) {
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Int(n), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else if v.is_float() {
            let x = self.float_of(v);
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Float(x), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else {
            let s = self.val_str(v).unwrap_or_default();
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Str(&s), &mut out)
                .map_err(|m| self.err(m, span))?;
        }
        self.pop();
        let h = self.heap.alloc(Obj::Str(out.into()));
        self.push(Value::obj(h));
        Ok(())
    }

    /// `spec` with its nested width/precision fields (the `nested` values on the stack top)
    /// resolved and capped; `None` when it has none.
    fn fmt_fields(
        &self,
        spec: &crate::fmtspec::FormatSpec,
        span: Span,
    ) -> Result<Option<crate::fmtspec::FormatSpec>, RuntimeError> {
        let nested = spec.dyn_width as usize + spec.dyn_precision as usize;
        if nested == 0 {
            return Ok(None);
        }
        let mut fs = spec.clone();
        let mut slot = self.stack.len() - nested;
        if fs.dyn_width {
            fs.width = crate::fmtspec::field_from_int(self.int_val(self.stack[slot]), "width")
                .map_err(|m| self.err(m, span))?;
            slot += 1;
        }
        if fs.dyn_precision {
            let p = crate::fmtspec::field_from_int(self.int_val(self.stack[slot]), "precision")
                .map_err(|m| self.err(m, span))?;
            fs.precision = Some(p);
        }
        Ok(Some(fs))
    }

    /// `Op::ToStrFmt`'s end for a value that is not a scalar: `rendered` (bool, `None`, a
    /// container, a `str` hook's result) pads as a plain string (type chars / precision fault in
    /// `apply`), and the str replaces the value and its nested fields.
    pub(super) fn fmt_finish(
        &mut self,
        spec: &crate::fmtspec::FormatSpec,
        rendered: String,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let nested = spec.dyn_width as usize + spec.dyn_precision as usize;
        let resolved = self.fmt_fields(spec, span)?;
        for _ in 0..nested {
            self.pop();
        }
        let spec = resolved.as_ref().unwrap_or(spec);
        let mut out = String::new();
        crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Other(&rendered), &mut out)
            .map_err(|m| self.err(m, span))?;
        let h = self.heap.alloc(Obj::Str(out.into()));
        let top = self.stack.len() - 1;
        self.stack[top] = Value::obj(h);
        Ok(())
    }

    /// THE render of `print`, interpolation and `str()`: append `v` onto `out`, a `str` bare, or
    /// (`nested`: inside a container, a field, a payload, an error text) as its Python `repr`
    /// (quoted + escaped), so `["a", "b"]` and `["a, b"]` differ and `[""]` is not `[]`
    /// (`docs/contributing/gaps.md` §W7-25). A `str(self)` hook's result is the object's own
    /// rendering, never quoted: it renders bare at the hooked value's depth. `depth` is the structural depth `v` sits at; every render tests
    /// `walk_base + depth` against [`MAX_STRUCTURAL_DEPTH`] per value, so a `str` hook that
    /// renders continues on the same budget (see [`Vm::walk_base`]).
    ///
    /// A value with no `str` hook in it renders by a `&self` walk on the host stack with no user
    /// code and no heap allocation ([`Vm::str_tight`]). At the first hooked value the render
    /// starts again on levels in operand-stack slots ([`Vm::str_levels`]): with `then` (the
    /// caller runs on the dispatch loop) the hook runs as a frame that can park, and `Ok(false)`
    /// says so: its return resumes the walk ([`Vm::str_resume`]) and feeds `then`, with `state`
    /// (the caller's own slots, pushed only then) below the walk. With no `then` the hook runs
    /// nested ([`Vm::guarded_walk`]) and the answer is always `Ok(true)`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn str_walk(
        &mut self,
        out: &mut String,
        v: Value,
        nested: bool,
        depth: usize,
        then: Option<StrThen>,
        state: &[Value],
        span: Span,
    ) -> Result<bool, RuntimeError> {
        use std::fmt::Write as _;
        if !nested {
            if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
                return Err(self.depth_exceeded_err(span));
            }
            if let Some(n) = self.int_val(v) {
                let _ = write!(out, "{n}");
                return Ok(true);
            }
            if v.is_float() {
                out.push_str(&format_float(self.float_of(v)));
                return Ok(true);
            }
            if let Some(b) = v.as_bool() {
                out.push_str(if b { "true" } else { "false" });
                return Ok(true);
            }
            if v.is_nil() {
                out.push_str(Self::NONE_TEXT);
                return Ok(true);
            }
            if let Some(h) = v.as_obj()
                && let Obj::Str(s) = self.heap.get(h)
            {
                out.push_str(s);
                return Ok(true);
            }
        }
        self.str_obj(out, v, nested, depth, then, state, span)
    }

    /// [`Vm::str_walk`] past its inline scalar answers: the tight walk, then the walk on levels.
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn str_obj(
        &mut self,
        out: &mut String,
        v: Value,
        nested: bool,
        depth: usize,
        then: Option<StrThen>,
        state: &[Value],
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let mark = out.len();
        let mut open = std::mem::take(&mut self.repr_active);
        let ra = open.len();
        let item = Item {
            v,
            d: depth,
            nested,
            hooks: true,
        };
        let r = self.str_tight(out, &mut open, item, span);
        open.truncate(ra);
        self.repr_active = open;
        if r? {
            return Ok(true);
        }
        out.truncate(mark);
        let floor = self.stack.len();
        if then.is_some() {
            self.stack.extend_from_slice(state);
        }
        // The value's root slot, then the output buffer's (nil until a park fills it).
        self.stack.extend([v, Value::nil()]);
        let base = self.stack.len();
        match self.str_levels(out, base, ra, Some(item), then, span) {
            Ok(true) => {
                self.str_merge(base - 1, out);
                self.stack.truncate(floor);
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e) => {
                self.stack.truncate(floor);
                self.repr_active.truncate(ra);
                Err(e)
            }
        }
    }

    /// The walk of `it` up to its first hooked value or busy I/O handle (`Ok(false)`): no user
    /// code runs and nothing allocates on the heap, so nothing can park, collect or mutate, and
    /// the walk recurses on the host stack over each container's children in place
    /// (`MAX_STRUCTURAL_DEPTH` bounds it, as at every value). `open` is the render's open
    /// containers ([`Vm::repr_active`], moved out for the walk); the caller restores it and, at
    /// `Ok(false)`, renders again on levels, which meet the same values in the same order up to
    /// that one. The value rules are [`Vm::str_item`]'s.
    fn str_tight(
        &self,
        out: &mut String,
        open: &mut Vec<GcRef>,
        mut it: Item,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        loop {
            match self.str_item(out, open, it, span)? {
                Got::Done => return Ok(true),
                Got::Tail(v, d, nested) => {
                    it = Item {
                        v,
                        d,
                        nested,
                        hooks: true,
                    }
                }
                Got::Open(kind, h, d) => return self.str_tight_kids(out, open, kind, h, d, span),
                Got::Hook | Got::Busy => return Ok(false),
            }
        }
    }

    /// [`Vm::str_tight`] over the children of `h`, its opener written: each at depth `d`, nested,
    /// then its closer. Out of line, so the recursion's frame stays small.
    #[inline(never)]
    fn str_tight_kids(
        &self,
        out: &mut String,
        open: &mut Vec<GcRef>,
        kind: Open,
        h: GcRef,
        d: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if kind.tracks() {
            open.push(h);
        }
        let def = self.str_def(kind, h);
        let mut i = 0;
        let all = self.str_kids_each(h, |v| {
            str_sep(out, kind, i, def);
            i += 1;
            let it = Item {
                v,
                d,
                nested: true,
                hooks: true,
            };
            self.str_tight(out, open, it, span)
        })?;
        if !all {
            return Ok(false);
        }
        str_close(out, kind, i);
        if kind.tracks() {
            open.pop();
        }
        Ok(true)
    }

    /// Run `f` over the children of the container `h` in render order (a map's key, then its
    /// value, per entry), until it answers `false`.
    fn str_kids_each(
        &self,
        h: GcRef,
        mut f: impl FnMut(Value) -> Result<bool, RuntimeError>,
    ) -> Result<bool, RuntimeError> {
        match self.heap.get(h) {
            Obj::List(xs) => {
                for v in xs.iter() {
                    if !f(v)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Tuple(xs) => {
                for &v in xs {
                    if !f(v)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Map(m) => {
                for (_, k, v) in m.entries.iter() {
                    if !f(k)? || !f(v)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Set(s) => {
                for (_, e) in s.entries.iter() {
                    if !f(e)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Struct { fields, .. } => {
                for v in fields.iter() {
                    if !f(v)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Enum { payload, .. } => {
                for v in payload.iter() {
                    if !f(v)? {
                        return Ok(false);
                    }
                }
            }
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
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
            | Obj::Generator(_) => unreachable!("a render opens a container"),
        }
        Ok(true)
    }

    /// The struct def whose field names a `Struct` level writes; `None` for any other level.
    fn str_def(&self, kind: Open, h: GcRef) -> Option<&crate::base::op::StructDef> {
        if kind != Open::Struct {
            return None;
        }
        let Obj::Struct { tid, .. } = self.heap.get(h) else {
            unreachable!("a struct level holds a struct")
        };
        self.program.structs.get(self.struct_name_of_tid(*tid))
    }

    /// One value's render step, the rules every render shares: write `it`'s text, or its opener
    /// and ask for its children, or ask for its hook. `open` holds the containers whose default
    /// render is in progress: one met again is a back edge and renders as `[...]` / `{...}` /
    /// `Set(...)` / `...` (W14-35c, CPython's `Py_ReprEnter`; a stack, not a visited set, so a
    /// shared acyclic child renders in full).
    fn str_item(
        &self,
        out: &mut String,
        open: &[GcRef],
        it: Item,
        span: Span,
    ) -> Result<Got, RuntimeError> {
        use std::fmt::Write as _;
        let Item {
            v,
            d,
            nested,
            hooks,
        } = it;
        if nested && let Some(h) = v.as_obj() {
            if let Obj::Str(s) = self.heap.get(h) {
                out.push_str(&crate::slice::str_repr(s));
                return Ok(Got::Done);
            }
            // A nested carrier keeps the nested (repr) mode for its payload: `[!'boom']`.
            if let Obj::Enum {
                variant_id,
                payload,
            } = self.heap.get(h)
                && let Some(prefix) = self.carrier_prefix(*variant_id, payload)
            {
                out.push_str(prefix);
                return Ok(match payload.try_get(0) {
                    Some(p) => Got::Tail(p, d + 1, true),
                    None => {
                        out.push_str(Self::NONE_TEXT);
                        Got::Done
                    }
                });
            }
        }
        // Guard against cyclic data overflowing the host stack — turns SIGABRT into a recoverable
        // `RuntimeError`. Tested against `walk_base + depth`: a `str` hook that renders from
        // inside its BODY continues on the same shared budget rather than restarting at 0 (see
        // `Vm::walk_base`). The hook's RESULT renders at the hooked value's own depth, so a
        // non-recursive protocol hook doesn't burn the budget.
        if self.walk_base + d > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        if let Some(n) = self.int_val(v) {
            let _ = write!(out, "{n}");
            return Ok(Got::Done);
        }
        if v.is_float() {
            out.push_str(&format_float(self.float_of(v)));
            return Ok(Got::Done);
        }
        if let Some(b) = v.as_bool() {
            out.push_str(if b { "true" } else { "false" });
            return Ok(Got::Done);
        }
        let Some(h) = v.as_obj() else {
            out.push_str(Self::NONE_TEXT);
            return Ok(Got::Done);
        };
        match self.heap.get(h) {
            Obj::Str(s) => out.push_str(s),
            // Boxed scalars render identically to the inline `Int`/`Float`.
            Obj::BigInt(n) => {
                let _ = write!(out, "{n}");
            }
            Obj::FloatBox(f) => out.push_str(&format_float(*f)),
            // `bytes` renders as its Python `b'...'` repr, `bytearray` as `bytearray(b'...')`.
            Obj::Bytes(b) => out.push_str(&crate::slice::bytes_repr(b)),
            Obj::ByteArray(b) => out.push_str(&crate::slice::bytearray_repr(&b.to_vec())),
            Obj::List(_) if open.contains(&h) => out.push_str("[...]"),
            Obj::List(_) => {
                out.push('[');
                return Ok(Got::Open(Open::List, h, d + 1));
            }
            Obj::Tuple(_) => {
                out.push('(');
                return Ok(Got::Open(Open::Tuple, h, d + 1));
            }
            Obj::Map(_) if open.contains(&h) => out.push_str("{...}"),
            Obj::Map(_) => {
                out.push('{');
                return Ok(Got::Open(Open::Map, h, d + 1));
            }
            Obj::Set(s) if s.entries.is_empty() => out.push_str("Set()"),
            Obj::Set(_) if open.contains(&h) => out.push_str("Set(...)"),
            Obj::Set(_) => {
                out.push('{');
                return Ok(Got::Open(Open::Set, h, d + 1));
            }
            // The `str` hook overrides the default repr. The checker decided whether the type has
            // it; a `str` that is not the hook is an ordinary method and never runs here.
            Obj::Struct { .. } if hooks && self.hook_of(v, Hook::Str) => return Ok(Got::Hook),
            // W14-35c: the DEFAULT repr only (a hook is never guarded, like CPython's user
            // `__repr__`). CPython's dataclass repr writes `...` for the whole re-entered object.
            Obj::Struct { .. } if open.contains(&h) => out.push_str("..."),
            Obj::Struct { tid, .. } => {
                // ROOT REDESIGN — the BARE display name, not the qualified identity key.
                let name = self.struct_name_of_tid(*tid);
                match self.program.structs.get(name) {
                    Some(def) => out.push_str(&def.display_name),
                    None => out.push_str(&crate::base::words::bare_display(name)),
                }
                out.push('(');
                return Ok(Got::Open(Open::Struct, h, d + 1));
            }
            Obj::Enum { .. } if hooks && self.hook_of(v, Hook::Str) => return Ok(Got::Hook),
            Obj::Enum {
                variant_id,
                payload,
            } => {
                // A carrier prints its prefix, then its payload in the mode of the position the
                // carrier itself occupies. This is the bare (str) mode; a nested carrier took the
                // nested arm above.
                if let Some(prefix) = self.carrier_prefix(*variant_id, payload) {
                    out.push_str(prefix);
                    return Ok(match payload.try_get(0) {
                        Some(p) => Got::Tail(p, d, false),
                        None => {
                            out.push_str(Self::NONE_TEXT);
                            Got::Done
                        }
                    });
                }
                // M19 lever #2 — recover the variant name from the id (cold render path).
                out.push_str(self.enum_names(*variant_id).1);
                if !payload.is_empty() {
                    out.push('(');
                    return Ok(Got::Open(Open::Enum, h, d + 1));
                }
            }
            Obj::Func { proto, .. } => {
                let _ = write!(
                    out,
                    "<fn {}>",
                    crate::base::provider_name::display_fn_name(&self.program.protos[*proto].name)
                );
            }
            Obj::Closure { .. } => out.push_str("<closure>"),
            Obj::Module(m) => {
                let _ = write!(out, "<module {}>", m.names().name);
            }
            Obj::Native { name, .. } => {
                let _ = write!(out, "<native fn {name}>");
            }
            Obj::Builtin(b) => {
                let _ = write!(out, "<builtin fn {}>", b.name());
            }
            Obj::Cffi(c) => {
                let _ = write!(out, "<extern fn {}>", c.name());
            }
            // Channel / Executor / sockets have no protocol hook: the structural `Display`.
            Obj::Channel(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Ptr(_) => out.push_str(&self.display_guarded(v, d)?),
            // A read or write can hold the handle's lock while it waits detached (a FIFO read):
            // the walk on levels waits for it through the wait door ([`Vm::io_state_door`]).
            Obj::Writer(core) => match io_state(&core.inner) {
                Some(o) => out.push_str(&io_handle_text("Writer", o)),
                None => return Ok(Got::Busy),
            },
            Obj::Reader(core) => match io_state(&core.inner) {
                Some(o) => out.push_str(&io_handle_text("Reader", o)),
                None => return Ok(Got::Busy),
            },
            // TICKET-246 / M3.3 — a box renders through its one renderer, with no wait.
            Obj::Shared(c) => out.push_str(&self.display_box_alone(BoxCore::Shared(c), d)?),
            Obj::RwShared(c) => out.push_str(&self.display_box_alone(BoxCore::RwShared(c), d)?),
            Obj::Atomic(c) => out.push_str(&self.display_box_alone(BoxCore::Atomic(c), d)?),
            // Experimental generators render opaquely (no protocol hook).
            Obj::Generator(_) => out.push_str("<generator>"),
            // A cell is a transparent by-reference box (never a user-visible operand — reads
            // `CellLoad` first); defensively render its inner value.
            Obj::Cell(c) => return Ok(Got::Tail(c.get(), d, false)),
            Obj::Iter { .. } => out.push_str("<iterator>"),
        }
        Ok(Got::Done)
    }

    /// The render on levels from `base` (its levels in [`STR_LEVEL`]-slot groups, above the
    /// value's root and the output buffer): first `item`, then the top level's next child, until
    /// no level is left (`Ok(true)`). `ra` is `repr_active`'s length when the render began. A
    /// hooked value parks (`then`: `Ok(false)`) or runs its hook nested.
    fn str_levels(
        &mut self,
        out: &mut String,
        base: usize,
        ra: usize,
        mut item: Option<Item>,
        then: Option<StrThen>,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        loop {
            if let Some(it) = item.take() {
                match self.str_item(out, &self.repr_active, it, span)? {
                    Got::Done => {}
                    Got::Tail(v, d, nested) => {
                        item = Some(Item {
                            v,
                            d,
                            nested,
                            hooks: true,
                        });
                        continue;
                    }
                    Got::Open(kind, h, d) => self.str_push_level(kind, h, d),
                    Got::Busy => {
                        let text = self.io_text_door(it.v, span)?;
                        out.push_str(&text);
                    }
                    Got::Hook => {
                        // The hook's result renders bare in the value's place, at its depth; a
                        // type with no hook body after all renders its default repr.
                        let bare = |res: Value| Item {
                            v: res,
                            d: it.d,
                            nested: false,
                            hooks: true,
                        };
                        let res = match then {
                            Some(then) => match self.str_park(out, base, ra, it, then, span)? {
                                true => return Ok(false),
                                false => None,
                            },
                            None => {
                                // The hook re-enters the VM and may start its own structural
                                // walk, so hand it the depth this render has already consumed.
                                let at = self.walk_base + it.d;
                                self.guarded_walk(at, |vm| {
                                    vm.invoke_hook(Hook::Str, it.v, &[], span)
                                })?
                            }
                        };
                        item = Some(match res {
                            Some(res) => bare(res),
                            None => Item { hooks: false, ..it },
                        });
                        continue;
                    }
                }
            }
            if self.stack.len() == base {
                return Ok(true);
            }
            item = self.str_level_step(out);
        }
    }

    /// Push a level for the container `h` (its opener written; its children render at depth
    /// `d`): a snapshot list of its children, as the default render always walked, so a hook that
    /// mutates `h` does not change what it prints and the children it has still to render stay
    /// alive. `h` sits in its slot while the snapshot allocates.
    fn str_push_level(&mut self, kind: Open, h: GcRef, d: usize) {
        let at = self.stack.len();
        let tag = (d << 3) | kind as usize;
        self.stack.extend([
            Value::int(tag as i64),
            Value::obj(h),
            Value::nil(),
            Value::int(0),
        ]);
        let kids = self.walk_children(h);
        let snap = self.heap.alloc(Obj::List(kids.into()));
        self.stack[at + 2] = Value::obj(snap);
        if kind.tracks() {
            self.repr_active.push(h);
        }
    }

    /// The top level's next child to render, its separator written; or, past its last child, its
    /// closer written and the level popped (`None`).
    fn str_level_step(&mut self, out: &mut String) -> Option<Item> {
        let t = self.stack.len() - STR_LEVEL;
        let tag = self.stack[t].as_int() as usize;
        let (kind, d) = (Open::of(tag & 7), tag >> 3);
        let (h, snap) = (self.stack[t + 1].as_gcref(), self.stack[t + 2].as_gcref());
        let i = self.stack[t + 3].as_int() as usize;
        match self.walk_child(snap, i) {
            Some(v) => {
                str_sep(out, kind, i, self.str_def(kind, h));
                self.stack[t + 3] = Value::int(i as i64 + 1);
                Some(Item {
                    v,
                    d,
                    nested: true,
                    hooks: true,
                })
            }
            None => {
                str_close(out, kind, i);
                if kind.tracks() {
                    self.repr_active.pop();
                }
                self.stack.truncate(t);
                None
            }
        }
    }

    /// A busy `Writer` / `Reader`'s text, once its lock is free: its fd's wait, so the holder's
    /// FIFO peer runs at one worker too.
    fn io_text_door(&mut self, v: Value, span: Span) -> Result<String, RuntimeError> {
        let h = v.as_gcref();
        if let Obj::Writer(core) = self.heap.get(h) {
            let core = core.clone();
            let open = self.io_state_door(&core.inner, core.waits, span)?;
            return Ok(io_handle_text("Writer", open));
        }
        if let Obj::Reader(core) = self.heap.get(h) {
            let core = core.clone();
            let open = self.io_state_door(&core.inner, core.waits, span)?;
            return Ok(io_handle_text("Reader", open));
        }
        unreachable!("only an I/O handle is busy")
    }

    /// Push the header and the `str` hook frame of the hooked value `it`. The text so far moves
    /// into the render's buffer (slot `base - 1`), appended in place. The hook runs with
    /// `walk_base` raised by its depth (the shared budget), restored by its return
    /// ([`Vm::str_resume`]) or its unwind ([`Vm::drop_ret`]). `Ok(false)`: no hook body.
    fn str_park(
        &mut self,
        out: &mut String,
        base: usize,
        ra: usize,
        it: Item,
        then: StrThen,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        self.str_stash(base - 1, out);
        let saved = self.walk_base;
        let levels = (self.stack.len() - base) / STR_LEVEL;
        self.stack
            .extend([ra, saved, levels, it.d].map(|k| Value::int(k as i64)));
        self.walk_base = saved + it.d;
        let ret = RetAction::StringifyWalk(then);
        let pushed = self.push_hook_frame(Hook::Str, it.v, &[], ret, span);
        if !matches!(pushed, Ok(true)) {
            self.walk_base = saved;
            self.stack.truncate(base + levels * STR_LEVEL);
        }
        pushed
    }

    /// Append `out` to the render's buffer in `slot` (a `bytearray`, made at the first park) and
    /// clear it.
    fn str_stash(&mut self, slot: usize, out: &mut String) {
        if out.is_empty() {
            return;
        }
        if self.stack[slot].is_nil() {
            let h = self.heap.alloc(Obj::ByteArray(RVec::new()));
            self.stack[slot] = Value::obj(h);
        }
        let h = self.stack[slot].as_gcref();
        let heap = self.heap.write(h);
        if let Obj::ByteArray(b) = heap.get(h) {
            b.extend_from_slice(heap, out.as_bytes());
        }
        out.clear();
    }

    /// Put the render's buffer in `slot` (if a park made one) in front of `out`.
    fn str_merge(&mut self, slot: usize, out: &mut String) {
        let Some(h) = self.stack[slot].as_obj() else {
            return;
        };
        let Obj::ByteArray(b) = self.heap.get(h) else {
            unreachable!("a render's buffer is a bytearray")
        };
        let bytes = b.to_vec();
        let mut s = String::from_utf8(bytes).expect("a render's buffer holds the str it was given");
        s.push_str(out);
        *out = s;
    }

    /// A render's hook frame returned: its result on the stack top, over the header. Render it
    /// in the hooked value's place and run the walk on; at its end feed `then`. On a fault the
    /// walk's slots and `then`'s go.
    pub(super) fn str_resume(&mut self, then: StrThen, span: Span) -> Result<(), RuntimeError> {
        let res = self.pop();
        let n = self.stack.len();
        let [ra, saved, levels, d] = [4, 3, 2, 1].map(|k| self.stack[n - k].as_int() as usize);
        self.stack.truncate(n - STR_HEADER);
        self.walk_base = saved;
        let base = self.stack.len() - levels * STR_LEVEL;
        let mut out = String::new();
        let item = Item {
            v: res,
            d,
            nested: false,
            hooks: true,
        };
        match self.str_levels(&mut out, base, ra, Some(item), Some(then), span) {
            Ok(true) => {
                self.str_merge(base - 1, &mut out);
                self.stack.truncate(base - 2);
                self.str_finish(then, out, span)
            }
            Ok(false) => Ok(()),
            Err(e) => {
                self.stack.truncate(base - 2 - then.slots());
                self.repr_active.truncate(ra);
                match then {
                    StrThen::Assert { .. } => Err(self.assert_fault(None, span)),
                    StrThen::ToStr
                    | StrThen::Fmt
                    | StrThen::Push
                    | StrThen::Print { .. }
                    | StrThen::Build
                    | StrThen::KeyText => Err(e),
                }
            }
        }
    }

    /// A render's hook frame unwound (`base` its frame base): restore `walk_base` and close the
    /// containers the render had open. The slots go with the caller's stack.
    pub(super) fn str_walk_drop(&mut self, base: usize) {
        self.walk_base = self.stack[base - 3].as_int() as usize;
        self.repr_active
            .truncate(self.stack[base - 4].as_int() as usize);
    }

    /// Feed a finished render to its continuation; its `state` is on the stack top.
    pub(super) fn str_finish(
        &mut self,
        then: StrThen,
        out: String,
        span: Span,
    ) -> Result<(), RuntimeError> {
        match then {
            StrThen::ToStr => {
                let h = self.heap.alloc(Obj::Str(out.into()));
                let top = self.stack.len() - 1;
                self.stack[top] = Value::obj(h);
                Ok(())
            }
            StrThen::Push => {
                let h = self.heap.alloc(Obj::Str(out.into()));
                self.push(Value::obj(h));
                Ok(())
            }
            StrThen::Fmt => {
                let program = Arc::clone(&self.program);
                let caller = self.frames.last().expect("a hook returns to its caller");
                let Op::ToStrFmt(spec) = &program.protos[caller.proto].code[caller.ip - 1] else {
                    unreachable!("a Fmt render is started only by Op::ToStrFmt")
                };
                self.fmt_finish(spec, out, span)
            }
            StrThen::Print { custom } => self.print_resume(custom, out, span),
            StrThen::Build => {
                let i = self.pop().as_int() as usize;
                let n = self.pop().as_int() as usize;
                self.build_str(n, i + 1, out, span)
            }
            StrThen::Assert { right } => self.assert_text(usize::from(right) + 1, out, span),
            StrThen::KeyText => Err(self.key_fault(&out, span)),
        }
    }
}

/// What a render's answer feeds once its parked hook frame returns
/// ([`RetAction::StringifyWalk`]).
#[derive(Clone, Copy, Debug)]
pub(super) enum StrThen {
    /// `Op::ToStr`: the str replaces the value on the stack top
    ToStr,
    /// `Op::ToStrFmt`: the caller's spec pads the str; the value and its nested fields sit below
    Fmt,
    /// `x.str()` (the intrinsic): push the str
    Push,
    /// a `print` arg: the line runs on from the next arg; `[argc, i]` sit below the walk
    Print { custom: bool },
    /// an interpolated part (`Op::BuildStr`): the parts run on; `[n, i]` sit below the walk
    Build,
    /// a failed `assert`'s left (or `right`) operand: its text runs on, then the assert faults
    /// ([`Vm::assert_text`]); the operands sit below the walk
    Assert { right: bool },
    /// a missing map key: the lookup faults with its text ([`Vm::key_not_found`])
    KeyText,
}

impl StrThen {
    /// The `state` slots its caller keeps below the walk.
    fn slots(self) -> usize {
        match self {
            StrThen::ToStr
            | StrThen::Fmt
            | StrThen::Push
            | StrThen::Assert { .. }
            | StrThen::KeyText => 0,
            StrThen::Print { .. } | StrThen::Build => 2,
        }
    }
}

/// One container level of a render on levels: `[tag, h, snap, i]`. `tag` packs the [`Open`] kind
/// (low 3 bits) and the depth its children render at; `h` is the container, `snap` the snapshot
/// list of its children, `i` the next child.
const STR_LEVEL: usize = 4;

/// What a parked render keeps above its levels, under the hook frame: `[ra, walk_base, levels,
/// depth]`, `repr_active`'s length when the render began, the `walk_base` to restore, the level
/// count and the hooked value's depth.
const STR_HEADER: usize = 4;

/// A container a render opens: its children render between its opener and its closer.
#[derive(Clone, Copy, PartialEq)]
enum Open {
    List = 0,
    Tuple = 1,
    Map = 2,
    Set = 3,
    Struct = 4,
    Enum = 5,
}

impl Open {
    fn of(tag: usize) -> Open {
        match tag {
            0 => Open::List,
            1 => Open::Tuple,
            2 => Open::Map,
            3 => Open::Set,
            4 => Open::Struct,
            _ => Open::Enum,
        }
    }

    /// A back edge can reach it: it is in `repr_active` while its children render.
    fn tracks(self) -> bool {
        match self {
            Open::List | Open::Map | Open::Set | Open::Struct => true,
            Open::Tuple | Open::Enum => false,
        }
    }
}

/// One value a render has still to write: at depth `d`, `nested` (a `str` as its repr), and
/// whether a `str` hook may run (`false` once a hooked type turned out to have no hook body).
#[derive(Clone, Copy)]
struct Item {
    v: Value,
    d: usize,
    nested: bool,
    hooks: bool,
}

/// What one value's render step ([`Vm::str_item`]) asks for next.
enum Got {
    /// the value's text is written
    Done,
    /// render this value in its place: a carrier's payload, a cell's value (depth, nested)
    Tail(Value, usize, bool),
    /// the opener is written: render the children of the container at this depth, then its closer
    Open(Open, GcRef, usize),
    /// a struct or enum with a `str` hook: the hook's result renders in its place
    Hook,
    /// a `Writer` / `Reader` whose lock another task holds: wait for it
    Busy,
}

/// The text before child `i` of a `kind` container: `, ` between children, a map's `: ` between
/// its key and value, a struct field's `name=`.
fn str_sep(out: &mut String, kind: Open, i: usize, def: Option<&crate::base::op::StructDef>) {
    use std::fmt::Write as _;
    match kind {
        Open::Map if i % 2 == 1 => out.push_str(": "),
        Open::List | Open::Tuple | Open::Map | Open::Set | Open::Struct | Open::Enum => {
            if i > 0 {
                out.push_str(", ");
            }
        }
    }
    if kind == Open::Struct {
        match def.and_then(|d| d.fields.get(i)) {
            Some(k) => {
                let _ = write!(out, "{k}=");
            }
            None => {
                let _ = write!(out, "{i}=");
            }
        }
    }
}

/// The closer of a `kind` container of `n` children.
fn str_close(out: &mut String, kind: Open, n: usize) {
    match kind {
        Open::List => out.push(']'),
        Open::Tuple => {
            out.push_str(crate::base::words::tuple_tail(n));
            out.push(')');
        }
        Open::Map | Open::Set => out.push('}'),
        Open::Struct | Open::Enum => out.push(')'),
    }
}

#[cfg(test)]
mod io_state_tests {
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, Sender, channel};

    /// A backing whose drop blocks until the test lets it go: the render must not wait on a close.
    struct SlowDrop {
        entered: Sender<()>,
        release: Receiver<()>,
    }

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }

    /// M3.0a C7 — a render of a handle that another thread is closing reads its true state,
    /// `closed`, as base main did by waiting on the lock: the close takes the backing out under
    /// the lock, and its drop (a flush that can block on a FIFO) runs after the lock is released.
    #[test]
    fn a_render_during_a_close_reads_closed() {
        let (entered_tx, entered_rx) = channel();
        let (release_tx, release_rx) = channel();
        let inner = Mutex::new(Some(SlowDrop {
            entered: entered_tx,
            release: release_rx,
        }));
        std::thread::scope(|s| {
            s.spawn(|| super::super::fileio::close_slot(&inner));
            entered_rx.recv().expect("the close drops the backing");
            let open = super::io_state_now(&inner);
            release_tx.send(()).expect("release the drop");
            assert!(!open, "a handle in its close renders as open");
        });
    }
}
