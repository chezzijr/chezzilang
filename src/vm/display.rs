//! VM: type names, display and stringify.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::stmt::BoxCore;
use super::*;
use crate::base::hooks::Hook;

/// The heap values of a print walk's element copy, the ones it roots (M3.0a C6): an inline int,
/// bool or nil needs no root, so a list of them pushes nothing.
fn walk_roots(vals: impl Iterator<Item = Value>) -> Vec<Value> {
    vals.filter(|v| v.child_gcref().is_some()).collect()
}

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
    /// [`io_state`] for the print path: a busy handle is waited for through the GC door.
    fn io_state_door<T>(&mut self, inner: &std::sync::Mutex<Option<T>>) -> bool {
        io_state(inner).unwrap_or_else(|| {
            self.gc_blocking(&[], |_| {
                inner.lock().unwrap_or_else(|e| e.into_inner()).is_some()
            })
        })
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
                Obj::Shared(_) => "Shared",
                Obj::RwShared(_) => "RwShared",
                Obj::Atomic(_) => "Atomic",
                Obj::AtomicInt(_) => "AtomicInt",
                Obj::Executor(_) => "Executor",
                Obj::Socket(_) => "Socket",
                Obj::Listener(_) => "Listener",
                Obj::Writer(_) => "Writer",
                Obj::Reader(_) => "Reader",
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
                // so it renders as its `repr` — same rule as `stringify_nested_into`, applied here
                // by depth alone because this renderer is `&self` and has no display-hook path that
                // preserves depth. A top-level `display` of a `str` stays its bare characters.
                Obj::Str(s) if depth > 0 => Ok(crate::slice::str_repr(s)),
                Obj::Str(s) => Ok(s.to_string()),
                // Boxed scalars render identically to the inline `Int`/`Float`.
                Obj::BigInt(n) => Ok(n.to_string()),
                Obj::FloatBox(f) => Ok(format_float(*f)),
                // Python `bytes` repr `b'...'` — shared with the interp via `slice::bytes_repr`.
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
                // override is honored by `stringify` (the path print/`str()` actually use).
                Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[*proto].name)),
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
                // D6: render open/closed without exposing the fd; matches no interp counterpart (net
                // is VM-only) but mirrors the core handles' structural `Display`.
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

    /// Protocol-aware render for `print` / `str()` / interpolation: a struct with a self-only
    /// `str(self) -> str` method (the `Stringable` protocol) dispatches to it; everything else uses
    /// the default structural repr, recursing through `stringify` so nested structs honour the
    /// protocol too. Distinct from the `&self` `display` above, which stays the pure structural form
    /// for error/debug text.
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

    /// Render `v` by appending into `out` — the allocation-free core shared by `stringify` (which
    /// wraps it in a fresh `String`) and `BuildStr` (which reuses one buffer across all interpolation
    /// parts). Byte-identical output to the old return-a-`String` form; only the intermediate
    /// per-part / per-element `String`s are gone.
    /// `Op::ToStrFmt` — render the top-of-stack value per the parsed format spec. Scalars map
    /// straight to a [`crate::fmtspec::FmtArg`]; non-scalars are rendered via the normal
    /// `stringify_into` first (rooted on the operand stack, so a `str` method's nested frames see a
    /// live object), then formatted as a plain string. A LITERAL width/precision is already capped at
    /// compile time. A nested `{expr}` one (`{s:<{w}}`) sits on the stack above the value — width,
    /// then precision — and is resolved and capped by [`crate::fmtspec::field_from_int`] BEFORE
    /// anything is rendered or allocated, so a huge runtime width is a clean error, never an OOM.
    /// Lives in its own `#[inline(never)]` helper to keep `step`'s frame small (commit 1450077).
    #[inline(never)]
    pub(super) fn op_to_str_fmt(
        &mut self,
        spec: &crate::fmtspec::FormatSpec,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let nested = spec.dyn_width as usize + spec.dyn_precision as usize;
        let resolved;
        let spec = if nested > 0 {
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
            for _ in 0..nested {
                self.pop();
            }
            resolved = fs;
            &resolved
        } else {
            spec
        };
        let v = self.stack[self.stack.len() - 1]; // leave rooted; rendering may run user code
        let mut out = String::new();
        if let Some(n) = self.int_val(v) {
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Int(n), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else if v.is_float() {
            let x = self.float_of(v);
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Float(x), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Str(_))
        {
            let s = match self.heap.get(h) {
                Obj::Str(s) => s.clone(),
                Obj::Bytes(_)
                | Obj::ByteArray(_)
                | Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::List(_)
                | Obj::Tuple(_)
                | Obj::Map(_)
                | Obj::Set(_)
                | Obj::Struct { .. }
                | Obj::Enum { .. }
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
                | Obj::Generator(_) => unreachable!(),
            };
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Str(&s), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else {
            // Bool/Nil/containers/structs: render with the normal stringify, then treat as a
            // string for fill/align/width (type chars/precision error via `apply`).
            let mut rendered = String::new();
            self.stringify_into(&mut rendered, v, span, 0)?;
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Other(&rendered), &mut out)
                .map_err(|m| self.err(m, span))?;
        }
        self.pop();
        let h = self.heap.alloc(Obj::Str(out.into()));
        self.push(Value::obj(h));
        Ok(())
    }

    pub(super) fn stringify_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Guard against cyclic data overflowing the host stack — turns SIGABRT into a recoverable
        // `RuntimeError`. Tested against `walk_base + depth`: a `str` hook that stringifies from
        // inside its BODY continues on the same shared budget rather than restarting at 0 (see
        // `Vm::walk_base`) — without that, hook-nesting × per-hook depth is unbounded and the
        // process dies uncatchably. The hook's RESULT is re-stringified at the *same* `depth` after
        // `guarded_walk` restored, so a non-recursive protocol hook still doesn't burn the budget.
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        if let Some(n) = self.int_val(v) {
            let _ = write!(out, "{n}");
        } else if v.is_float() {
            out.push_str(&format_float(self.float_of(v)));
        } else if let Some(b) = v.as_bool() {
            out.push_str(if b { "true" } else { "false" });
        } else if v.is_nil() {
            out.push_str(Self::NONE_TEXT);
        } else if let Some(h) = v.as_obj() {
            // ROOT the object on the operand stack: a `str` method runs nested frames that GC at
            // instruction boundaries, and the container keeps its transitive contents reachable.
            self.push(v);
            let r = self.stringify_obj_into(out, h, span, depth);
            self.pop();
            return r;
        }
        Ok(())
    }

    /// Render a value that is NESTED inside something else — a list/tuple/map/set element, a struct
    /// field, an enum payload. Identical to [`Self::stringify_into`] except that a `str` renders as
    /// its Python `repr` (quoted + escaped) instead of its bare characters, so `["a", "b"]` and
    /// `["a, b"]` no longer print the same text and `[""]` no longer prints `[]` (`docs/contributing/gaps.md`
    /// §W7-25). CPython's `str` vs `repr` split, same rule.
    ///
    /// NOT used for a `str(self)` display hook's RESULT: that string is the object's own rendering,
    /// not a nested value, so it must never be quoted (those sites re-enter `stringify_into` at the
    /// same depth deliberately).
    pub(super) fn stringify_nested_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            out.push_str(&crate::slice::str_repr(s));
            return Ok(());
        }
        // A nested carrier keeps the nested (repr) mode for its payload: `[!'boom']`.
        if let Some(h) = v.as_obj()
            && let Obj::Enum {
                variant_id,
                payload,
            } = self.heap.get(h)
            && let Some(prefix) = self.carrier_prefix(*variant_id, payload)
        {
            let inner = payload.try_get(0);
            out.push_str(prefix);
            return match inner {
                Some(p) => self.stringify_nested_into(out, p, span, depth + 1),
                None => {
                    out.push_str(Self::NONE_TEXT);
                    Ok(())
                }
            };
        }
        self.stringify_into(out, v, span, depth)
    }

    pub(super) fn stringify_obj_into(
        &mut self,
        out: &mut String,
        h: GcRef,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Clone the object's shape out so no heap borrow is held across the nested `&mut self` calls.
        match self.heap.get(h).clone() {
            Obj::Str(s) => out.push_str(&s),
            // Boxed scalars stringify identically to the inline `Int`/`Float`.
            Obj::BigInt(n) => out.push_str(&n.to_string()),
            Obj::FloatBox(f) => out.push_str(&format_float(f)),
            // `bytes` interpolates/prints as its Python `b'...'` repr (shared helper).
            Obj::Bytes(b) => out.push_str(&crate::slice::bytes_repr(&b)),
            // `bytearray` interpolates/prints as `bytearray(b'...')` (shared helper).
            Obj::ByteArray(b) => out.push_str(&crate::slice::bytearray_repr(&b.to_vec())),
            Obj::List(items) => {
                // W14-35c: a list met again while its own render is open is a back edge.
                if self.repr_active.contains(&h) {
                    out.push_str("[...]");
                    return Ok(());
                }
                out.push('[');
                self.stringify_open_seq(out, h, &items.to_vec(), span, depth)?;
                out.push(']');
            }
            Obj::Tuple(items) => {
                out.push('(');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                out.push_str(crate::base::words::tuple_tail(items.len()));
                out.push(')');
            }
            Obj::Map(m) => {
                if self.repr_active.contains(&h) {
                    out.push_str("{...}");
                    return Ok(());
                }
                out.push('{');
                self.stringify_open_map(out, h, &m.entries, span, depth)?;
                out.push('}');
            }
            Obj::Set(s) => {
                if s.entries.is_empty() {
                    out.push_str("Set()");
                } else if self.repr_active.contains(&h) {
                    out.push_str("Set(...)");
                    return Ok(());
                } else {
                    out.push('{');
                    self.stringify_open_set(out, h, &s.entries, span, depth)?;
                    out.push('}');
                }
            }
            Obj::Struct { tid, fields } => {
                // Resolve the type key from `tid` (owned — the `str` hook below takes `&mut self`,
                // so a `&self` borrow of the name can't live across it). Cold stringify path.
                let name = self.struct_name_of_tid(tid).to_string();
                // The `str` hook overrides the default repr. The checker decided whether the
                // type has it; a `str` that is not the hook is an ordinary method and never runs
                // here. The hook re-enters the VM and may start its own structural walk, so hand
                // it the depth this walk has already consumed — one shared budget (see
                // [`Vm::walk_base`]). The `stringify_into(out, res, ...)` below runs AFTER
                // `guarded_walk` restored, at the outer `depth`: it is the hook's RESULT.
                let def = self.program.structs.get(name.as_str()).cloned();
                let base = self.walk_base + depth;
                if let Some(res) = self.guarded_walk(base, |vm| {
                    vm.invoke_hook(Hook::Str, Value::obj(h), &[], span)
                })? {
                    return self.stringify_into(out, res, span, depth);
                }
                // Positional layout: recover declaration-order field names from the StructDef (the
                // same `def` already cloned for the `str` hook) — no per-instance name strings.
                // ROOT REDESIGN — render the BARE display name, not the qualified identity key.
                let display = def
                    .as_ref()
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::base::words::bare_display(&name));
                // W14-35c: the DEFAULT repr only (a `str` hook returned above and is never guarded,
                // like CPython's user `__repr__`). CPython's dataclass repr writes `...` for the
                // whole re-entered object.
                if self.repr_active.contains(&h) {
                    out.push_str("...");
                    return Ok(());
                }
                let _ = write!(out, "{display}(");
                self.stringify_open_fields(out, h, def.as_ref(), &fields.to_vec(), span, depth)?;
                out.push(')');
            }
            Obj::Enum {
                variant_id,
                payload,
            } => {
                // The `str` hook overrides the default `Variant(payload)` repr (see the struct arm).
                let base = self.walk_base + depth;
                if let Some(res) = self.guarded_walk(base, |vm| {
                    vm.invoke_hook(Hook::Str, Value::obj(h), &[], span)
                })? {
                    return self.stringify_into(out, res, span, depth);
                }
                // A carrier prints its prefix, then its payload in the mode of the position the
                // carrier itself occupies. This is the bare (str) mode; a nested carrier is
                // rendered by `stringify_nested_into` before it reaches here.
                if let Some(prefix) = self.carrier_prefix(variant_id, &payload) {
                    out.push_str(prefix);
                    return match payload.try_get(0) {
                        Some(v) => self.stringify_into(out, v, span, depth),
                        None => {
                            out.push_str(Self::NONE_TEXT);
                            Ok(())
                        }
                    };
                }
                // M19 lever #2 — recover the variant name from the id (cold stringify path).
                out.push_str(self.enum_names(variant_id).1);
                if !payload.is_empty() {
                    out.push('(');
                    self.stringify_seq_into(out, &payload.to_vec(), span, depth + 1)?;
                    out.push(')');
                }
            }
            Obj::Func { proto, .. } => {
                let _ = write!(out, "<fn {}>", self.program.protos[proto].name);
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
            // Channel / Shared / Executor have no protocol hook — reuse the structural `Display`
            // (`stringify`'s catch-all falls back to `Display` too).
            Obj::Channel(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Ptr(_) => {
                out.push_str(&self.display_guarded(Value::obj(h), depth)?);
            }
            // M1.8 — the print path waits for a busy handle through the GC door: a read or write
            // can hold the lock while it waits detached (a FIFO read), and a stop must not wait
            // for this thread meanwhile.
            Obj::Writer(core) => {
                let open = self.io_state_door(&core.inner);
                out.push_str(&io_handle_text("Writer", open));
            }
            Obj::Reader(core) => {
                let open = self.io_state_door(&core.inner);
                out.push_str(&io_handle_text("Reader", open));
            }
            // TICKET-246 / M3.3 — a box renders through its one renderer, with no wait.
            Obj::Shared(c) => out.push_str(&self.display_box_alone(BoxCore::Shared(&c), depth)?),
            Obj::RwShared(c) => {
                out.push_str(&self.display_box_alone(BoxCore::RwShared(&c), depth)?)
            }
            Obj::Atomic(c) => out.push_str(&self.display_box_alone(BoxCore::Atomic(&c), depth)?),
            // Experimental generators stringify opaquely (no protocol hook).
            Obj::Generator(_) => out.push_str("<generator>"),
            // A cell is a transparent by-reference box (never a user-visible operand — reads
            // `CellLoad` first); defensively stringify its inner value.
            Obj::Cell(v) => self.stringify_into(out, v.get(), span, depth)?,
            Obj::Iter { .. } => out.push_str("<iterator>"),
        }
        Ok(())
    }

    // W14-35c — the four open-render helpers below hold `repr_active` open around one container's
    // element walk and close it on the `Ok` AND the `Err` path. M3.0a C6 — each walks a Rust copy
    // of the container's elements, so it roots them for the walk (`Vm::with_roots`): the walk can
    // wait in the GC door (a busy `Writer`) or run a `str` hook, and a collection meanwhile must not
    // free an element that another task unlinked from the container. They are `#[inline(never)]` so the
    // `Result` temporaries they need stay out of `stringify_obj_into`'s frame: a `str` hook that
    // re-enters `str(self)` recurses through that frame, and a bigger frame would trip the native
    // stack before the call-depth cap (`self_referential_stringable_hits_depth_limit`).

    #[inline(never)]
    fn stringify_open_seq(
        &mut self,
        out: &mut String,
        h: GcRef,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.repr_active.push(h);
        let r = self.stringify_seq_into(out, elems, span, depth + 1);
        self.repr_active.pop();
        r
    }

    #[inline(never)]
    fn stringify_open_map(
        &mut self,
        out: &mut String,
        h: GcRef,
        entries: &RVec<MapEntry>,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        let roots = walk_roots(entries.iter().flat_map(|(_, k, mv)| [k, mv]));
        self.with_roots(&roots, |vm| {
            vm.stringify_map_entries(out, h, entries, span, depth)
        })
    }

    fn stringify_map_entries(
        &mut self,
        out: &mut String,
        h: GcRef,
        entries: &RVec<MapEntry>,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.repr_active.push(h);
        let mut r = Ok(());
        for (i, (_, k, mv)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            r = self.stringify_nested_into(out, k, span, depth + 1);
            if r.is_err() {
                break;
            }
            out.push_str(": ");
            r = self.stringify_nested_into(out, mv, span, depth + 1);
            if r.is_err() {
                break;
            }
        }
        self.repr_active.pop();
        r
    }

    #[inline(never)]
    fn stringify_open_set(
        &mut self,
        out: &mut String,
        h: GcRef,
        entries: &RVec<SetEntry>,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        let roots = walk_roots(entries.iter().map(|(_, e)| e));
        self.with_roots(&roots, |vm| {
            vm.stringify_set_entries(out, h, entries, span, depth)
        })
    }

    fn stringify_set_entries(
        &mut self,
        out: &mut String,
        h: GcRef,
        entries: &RVec<SetEntry>,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.repr_active.push(h);
        let mut r = Ok(());
        for (i, (_, e)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            r = self.stringify_nested_into(out, e, span, depth + 1);
            if r.is_err() {
                break;
            }
        }
        self.repr_active.pop();
        r
    }

    #[inline(never)]
    fn stringify_open_fields(
        &mut self,
        out: &mut String,
        h: GcRef,
        def: Option<&crate::base::op::StructDef>,
        fields: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        let roots = walk_roots(fields.iter().copied());
        self.with_roots(&roots, |vm| {
            vm.stringify_fields(out, h, def, fields, span, depth)
        })
    }

    fn stringify_fields(
        &mut self,
        out: &mut String,
        h: GcRef,
        def: Option<&crate::base::op::StructDef>,
        fields: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        self.repr_active.push(h);
        let mut r = Ok(());
        for (i, fv) in fields.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match def.and_then(|d| d.fields.get(i)) {
                Some(k) => {
                    let _ = write!(out, "{k}=");
                }
                None => {
                    let _ = write!(out, "{i}=");
                }
            }
            r = self.stringify_nested_into(out, *fv, span, depth + 1);
            if r.is_err() {
                break;
            }
        }
        self.repr_active.pop();
        r
    }

    /// Render `elems`, a Rust copy of a sequence's elements, rooted for the walk (M3.0a C6, see
    /// the open-render helpers above).
    #[inline(never)]
    pub(super) fn stringify_seq_into(
        &mut self,
        out: &mut String,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        let roots = walk_roots(elems.iter().copied());
        self.with_roots(&roots, |vm| {
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                vm.stringify_nested_into(out, *e, span, depth)?;
            }
            Ok(())
        })
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
