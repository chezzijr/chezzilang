//! VM: type names, display and stringify.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::stmt::BoxCore;
use super::stmt::BoxSnaps;
use super::*;
use crate::base::hooks::Hook;

impl Vm {
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
                Obj::Cell(v) => self.type_name(*v),
                Obj::Iter { .. } => "iterator",
            },
        }
    }

    /// `Display` form. Thin wrapper over the depth-guarded worker — kept infallible so every
    /// error-message / `display_wire` caller is unchanged; a cyclic structure renders as `<...>` here
    /// (the print path surfaces the error).
    pub(super) fn display(&self, v: Value) -> String {
        self.display_guarded(v, 0)
            .unwrap_or_else(|_| "<...>".to_string())
    }

    /// Depth-guarded structural display. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding cyclic data from overflowing the host stack.
    pub(super) fn display_guarded(&self, v: Value, depth: usize) -> Result<String, RuntimeError> {
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
                Obj::ByteArray(b) => Ok(crate::slice::bytearray_repr(b)),
                Obj::List(items) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    Ok(format!("[{}]", parts.join(", ")))
                }
                Obj::Tuple(items) => {
                    let n = items.len();
                    let mut parts = Vec::with_capacity(n);
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    // A 1-tuple renders `(1,)` — the trailing comma is what distinguishes it from a
                    // parenthesised scalar, as in Python's repr.
                    if n == 1 {
                        Ok(format!("({},)", parts.join(", ")))
                    } else {
                        Ok(format!("({})", parts.join(", ")))
                    }
                }
                Obj::Map(m) => {
                    let mut parts = Vec::with_capacity(m.entries.len());
                    for (_, k, v) in &m.entries {
                        parts.push(format!(
                            "{}: {}",
                            self.display_guarded(*k, depth + 1)?,
                            self.display_guarded(*v, depth + 1)?
                        ));
                    }
                    Ok(format!("{{{}}}", parts.join(", ")))
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        Ok("Set()".to_string())
                    } else {
                        let mut parts = Vec::with_capacity(s.entries.len());
                        for (_, v) in &s.entries {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{{{}}}", parts.join(", ")))
                    }
                }
                Obj::Struct { tid, fields, .. } => {
                    // Positional layout: recover declaration-order field names from the StructDef
                    // (cold display path). Resolve the type key from `tid`; snapshot values to drop
                    // the heap borrow.
                    let name = self.struct_name_of_tid(*tid);
                    let vals: Vec<Value> = fields.as_slice().to_vec();
                    // ROOT REDESIGN — render the BARE display name (not the qualified identity key);
                    // `name` is the key the StructDef is stored under. Fall back to stripping the key.
                    let (display, names): (String, Vec<String>) = self
                        .program
                        .structs
                        .get(name)
                        .map(|d| (d.display_name.clone(), d.fields.clone()))
                        .unwrap_or_else(|| (crate::base::words::bare_display(name), Vec::new()));
                    let mut parts = Vec::with_capacity(vals.len());
                    for (i, v) in vals.iter().enumerate() {
                        let k = names.get(i).cloned().unwrap_or_else(|| i.to_string());
                        parts.push(format!("{k}={}", self.display_guarded(*v, depth + 1)?));
                    }
                    Ok(format!("{display}({})", parts.join(", ")))
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // A carrier prints its prefix, then its payload in the mode of the position
                    // the carrier itself occupies (top level: bare; nested: repr).
                    if let Some(prefix) = self.carrier_prefix(*variant_id, payload) {
                        let inner = match payload.first() {
                            Some(v) => {
                                self.display_guarded(*v, if depth == 0 { 0 } else { depth + 1 })?
                            }
                            None => Self::NONE_TEXT.to_string(),
                        };
                        return Ok(format!("{prefix}{inner}"));
                    }
                    // M19 lever #2 — recover the variant name from the id (cold display path).
                    let variant = self.enum_names(*variant_id).1.to_string();
                    let payload: Vec<Value> = payload.clone();
                    if payload.is_empty() {
                        Ok(variant)
                    } else {
                        let mut parts = Vec::with_capacity(payload.len());
                        for v in &payload {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{variant}({})", parts.join(", ")))
                    }
                }
                // Raw display fallback (no method dispatch here): `Name(inner)`. The `str(self)`
                // override is honored by `stringify` (the path print/`str()` actually use).
                Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[*proto].name)),
                Obj::Closure { .. } => Ok("<closure>".to_string()),
                Obj::Module(m) => Ok(format!("<module {}>", m.name)),
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
                Obj::Shared(core) => Ok(self.display_box_alone(BoxCore::Shared(core))),
                Obj::RwShared(core) => Ok(self.display_box_alone(BoxCore::RwShared(core))),
                Obj::Atomic(core) => Ok(self.display_box_alone(BoxCore::Atomic(core))),
                Obj::AtomicInt(core) => Ok(format!(
                    "AtomicInt({})",
                    core.v.load(std::sync::atomic::Ordering::SeqCst)
                )),
                // Work not yet finished, counted across BOTH halves: the lazy `inner` queue (which
                // only the since-removed cooperative engine ever filled) and the eager outstanding
                // count. Summing both is what keeps this honest — reading only the queue would report
                // `pending=0` while jobs are running. Exactly one term is ever non-zero today.
                Obj::Executor(core) => Ok(format!(
                    "Executor(pending={})",
                    core.inner.lock().unwrap().len() + core.outstanding()
                )),
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
                Obj::Writer(core) => Ok(format!(
                    "Writer({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                // R2b: render open/closed without exposing the fd (mirrors the Writer handle).
                Obj::Reader(core) => Ok(format!(
                    "Reader({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Generator(_) => Ok("<generator>".to_string()),
                // A cell is a transparent by-reference box (never a user-visible operand — reads
                // `CellLoad` first); defensively display its inner value.
                Obj::Cell(v) => self.display_guarded(*v, depth),
                Obj::Iter { .. } => Ok("<iterator>".to_string()),
            },
        }
    }

    /// `Display` form of a [`WireValue`] — the read-only (`&self`) counterpart of [`display`] for
    /// values that live in a core (only a box renders its contents). A `Handle(GcRef)` resolves
    /// back through the heap via `display`; a nested box renders through [`Vm::display_box`], the
    /// same function its heap counterpart uses. B3.1: total over the sendable set.
    #[cfg(test)]
    pub(super) fn display_wire(&self, w: &WireValue) -> String {
        self.display_wire_in(w, &BoxSnaps::default(), &mut Vec::new())
    }

    /// A box with no snapshots taken beforehand and nothing being rendered around it.
    fn display_box_alone(&self, core: BoxCore) -> String {
        self.display_box(core, &BoxSnaps::default(), &mut Vec::new())
    }

    /// What a box holds, copied out; the lock is dropped before this returns. `None` when the
    /// state cannot be read without waiting.
    fn box_peek(&self, core: BoxCore) -> Option<WireValue> {
        Some(match core {
            // The state lock is held across user code, so a `&self` render only TRIES it.
            BoxCore::Shared(c) => {
                let st = c.state.0.try_lock().ok()?;
                self.box_copy_out(st.as_ref()?, Span::default()).ok()?
            }
            BoxCore::RwShared(c) => {
                let st = c.state.try_read()?;
                self.box_copy_out(st.as_ref()?, Span::default()).ok()?
            }
            BoxCore::Atomic(c) => c.v.lock().unwrap().clone(),
        })
    }

    /// TICKET-246 — the ONE renderer of a `Shared` / `RwShared` / `Atomic`, for a heap object and
    /// for a wire handle alike. Its rule:
    ///
    /// 1. a box already on the `seen` stack renders `Shared(...)`, like any container cycle;
    /// 2. it never takes a blocking lock on a box state: a state it cannot read renders
    ///    `Shared(<locked>)`;
    /// 3. it renders a snapshot (from `snaps`, else peeked) after the lock dropped, never under it;
    /// 4. contents render structurally, with no user `str` hook: a hook is user code.
    pub(super) fn display_box(
        &self,
        core: BoxCore,
        snaps: &BoxSnaps,
        seen: &mut Vec<usize>,
    ) -> String {
        let (label, addr) = (core.label(), core.addr());
        if seen.contains(&addr) {
            return format!("{label}(...)");
        }
        let peeked;
        let w = match snaps.get(&addr) {
            Some(w) => w,
            None => match self.box_peek(core) {
                Some(w) => {
                    peeked = w;
                    &peeked
                }
                None => return format!("{label}(<locked>)"),
            },
        };
        seen.push(addr);
        let inner = self.display_wire_in(w, snaps, seen);
        seen.pop();
        format!("{label}({inner})")
    }

    /// One box's snapshot for the print path, which may wait.
    fn box_snapshot_one(&mut self, core: BoxCore, span: Span) -> Result<WireValue, RuntimeError> {
        match core {
            BoxCore::Shared(c) => self.shared_copy_out(c, span),
            BoxCore::RwShared(c) => {
                self.rwshared_read(c, span, |vm, ctx| vm.box_copy_out(ctx, span))
            }
            BoxCore::Atomic(_) => self
                .box_peek(core)
                .ok_or_else(|| self.err("a box state could not be read".to_string(), span)),
        }
    }

    /// Snapshots of `root` and of every box nested in a snapshot, each box once. The print path
    /// takes them BEFORE it renders, so [`Vm::display_box`] never waits and never shows
    /// `<locked>` to user output.
    pub(super) fn box_snapshots(
        &mut self,
        root: BoxCore,
        span: Span,
    ) -> Result<BoxSnaps, RuntimeError> {
        let mut snaps = BoxSnaps::default();
        let mut seen = super::core::SeenCores::with(root.addr());
        let mut work = vec![root.to_wire()];
        let mut handles = Vec::new();
        while let Some(w) = work.pop() {
            // `pending` also carries channels and executors; they hold no box state to render.
            let Some(core) = BoxCore::of_wire(&w) else {
                continue;
            };
            let snap = self.box_snapshot_one(core, span)?;
            super::core::collect_gcrefs_structural(&snap, &mut handles, &mut seen, &mut work);
            snaps.insert(core.addr(), snap);
        }
        Ok(snaps)
    }

    /// [`display_wire`](Vm::display_wire) with the box snapshots taken beforehand and the stack of
    /// boxes being rendered.
    fn display_wire_in(&self, w: &WireValue, snaps: &BoxSnaps, seen: &mut Vec<usize>) -> String {
        match w {
            WireValue::Int(n) => n.to_string(),
            WireValue::Float(x) => format_float(*x),
            WireValue::Bool(b) => b.to_string(),
            WireValue::Nil => Self::NONE_TEXT.to_string(),
            // Every `display_wire` caller renders a NESTED position — inside `Shared(…)`/`Atomic(…)`,
            // a container, or a struct field — so a wire string is always quoted (`Shared(['a'])`).
            WireValue::Str(s) => crate::slice::str_repr(s),
            WireValue::Bytes(b) => crate::slice::bytes_repr(b),
            WireValue::ByteArray(b) => crate::slice::bytearray_repr(b),
            WireValue::Handle(h) => self.display(Value::obj(*h)),
            WireValue::List { items, .. } => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire_in(v, snaps, seen))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            WireValue::Tuple { items, .. } => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire_in(v, snaps, seen))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
            WireValue::Map { entries, .. } => {
                let inner = entries
                    .iter()
                    .map(|(_, k, v)| {
                        format!(
                            "{}: {}",
                            self.display_wire_in(k, snaps, seen),
                            self.display_wire_in(v, snaps, seen)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
            WireValue::Set { entries, .. } => {
                if entries.is_empty() {
                    "Set()".to_string()
                } else {
                    let inner = entries
                        .iter()
                        .map(|(_, v)| self.display_wire_in(v, snaps, seen))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{{{inner}}}")
                }
            }
            WireValue::Struct { name, fields, .. } => {
                let inner = fields
                    .iter()
                    .map(|(k, v)| format!("{k}={}", self.display_wire_in(v, snaps, seen)))
                    .collect::<Vec<_>>()
                    .join(", ");
                // ROOT REDESIGN — `name` is the qualified identity key; render the bare display name.
                let display = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::base::words::bare_display(name.as_ref()));
                format!("{display}({inner})")
            }
            WireValue::Enum {
                variant_id,
                payload,
                ..
            } => {
                // A carrier: its prefix, then its payload (a wire position is always nested).
                if let Some(prefix) = Self::carrier_prefix_wire(*variant_id, payload) {
                    let inner = match payload.first() {
                        Some(v) => self.display_wire_in(v, snaps, seen),
                        None => Self::NONE_TEXT.to_string(),
                    };
                    return format!("{prefix}{inner}");
                }
                // M19 lever #2 — the wire form carries the id; resolve the variant name on this cold
                // display path via the shared program's `variants_by_id`.
                let variant = self.enum_names(*variant_id).1;
                if payload.is_empty() {
                    variant.to_string()
                } else {
                    let inner = payload
                        .iter()
                        .map(|v| self.display_wire_in(v, snaps, seen))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{variant}({inner})")
                }
            }
            WireValue::Channel(core) => {
                // TICKET-042a — see the `Obj::Channel` arm above.
                format!("Channel(len={})", core.q.lock().unwrap().len())
            }
            WireValue::Shared(core) => self.display_box(BoxCore::Shared(core), snaps, seen),
            WireValue::RwShared(core) => self.display_box(BoxCore::RwShared(core), snaps, seen),
            WireValue::Atomic(core) => self.display_box(BoxCore::Atomic(core), snaps, seen),
            WireValue::AtomicInt(core) => {
                format!(
                    "AtomicInt({})",
                    core.v.load(std::sync::atomic::Ordering::SeqCst)
                )
            }
            WireValue::Executor(core) => {
                format!(
                    "Executor(pending={})",
                    core.inner.lock().unwrap().len() + core.outstanding()
                )
            }
            // D6: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Socket(core) => {
                format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            WireValue::Listener(core) => {
                format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // R2: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Writer(core) => {
                format!(
                    "Writer({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // R2b: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Reader(core) => {
                format!(
                    "Reader({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // An opaque `ptr` renders like its heap counterpart (`Obj::Ptr` → "<ptr null>"/"<ptr>");
            // never the raw address (non-deterministic across engines).
            WireValue::Ptr(a) => {
                if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }
            }
            // A wired first-class builtin fn renders like its heap counterpart (`<builtin fn name>`).
            WireValue::Builtin(b) => format!("<builtin fn {}>", b.name()),
            // A wired native/FFI fn renders like its heap counterpart (`Obj::Native`/`Obj::Cffi`).
            WireValue::Native { name, .. } => format!("<native fn {name}>"),
            WireValue::Cffi(c) => format!("<extern fn {}>", c.name()),
            // B3.6: a wired closure renders like its heap counterpart (`Obj::Closure` → "<closure>").
            WireValue::Closure { .. } => "<closure>".to_string(),
            // B3.3: a wired bare fn renders like its heap counterpart (`Obj::Func` → "<fn NAME>"),
            // DISTINCT from a closure — this is why `WireValue::Func` is kept separate from `Closure`.
            WireValue::Func { proto, .. } => format!("<fn {}>", self.program.protos[*proto].name),
            // A wired cursor renders like its heap counterpart (`Obj::Iter` → "<iterator>").
            WireValue::Iter { .. } => "<iterator>".to_string(),
            // A cell is a transparent box — render its inner value (never user-visible in practice).
            WireValue::Cell { inner, .. } => self.display_wire_in(inner, snaps, seen),
            // A back-reference closes a Cell/Closure cycle; render a stable marker rather than recurse
            // (never user-visible in practice — a wire value is only rendered on the error path).
            WireValue::Backref(_) => "<cycle>".to_string(),
            // F3 path C: a wired generator renders like its heap counterpart (`Obj::Generator` →
            // "<generator>").
            WireValue::Generator { .. } => "<generator>".to_string(),
        }
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
    /// `["a, b"]` no longer print the same text and `[""]` no longer prints `[]` (`docs/gaps.md`
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
            let inner = payload.first().copied();
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
            Obj::ByteArray(b) => out.push_str(&crate::slice::bytearray_repr(&b)),
            Obj::List(items) => {
                // W14-35c: a list met again while its own render is open is a back edge.
                if self.repr_active.contains(&h) {
                    out.push_str("[...]");
                    return Ok(());
                }
                out.push('[');
                self.stringify_open_seq(out, h, &items, span, depth)?;
                out.push(']');
            }
            Obj::Tuple(items) => {
                out.push('(');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                // A 1-tuple renders `(1,)` — the trailing comma is what distinguishes it from a
                // parenthesised scalar, as in Python's repr.
                if items.len() == 1 {
                    out.push(',');
                }
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
                self.stringify_open_fields(out, h, def.as_ref(), fields.as_slice(), span, depth)?;
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
                    return match payload.first() {
                        Some(v) => self.stringify_into(out, *v, span, depth),
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
                    self.stringify_seq_into(out, &payload, span, depth + 1)?;
                    out.push(')');
                }
            }
            Obj::Func { proto, .. } => {
                let _ = write!(out, "<fn {}>", self.program.protos[proto].name);
            }
            Obj::Closure { .. } => out.push_str("<closure>"),
            Obj::Module(m) => {
                let _ = write!(out, "<module {}>", m.name);
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
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Ptr(_) => {
                out.push_str(&self.display_guarded(Value::obj(h), depth)?);
            }
            // TICKET-246 — the print path snapshots the box and every box nested in it first
            // (it may wait there), then renders with no lock held.
            Obj::Shared(_) | Obj::RwShared(_) | Obj::Atomic(_) => {
                let root = match self.heap.get(h) {
                    Obj::Shared(c) => WireValue::Shared(Arc::clone(c)),
                    Obj::RwShared(c) => WireValue::RwShared(Arc::clone(c)),
                    Obj::Atomic(c) => WireValue::Atomic(Arc::clone(c)),
                    Obj::Str(_)
                    | Obj::Bytes(_)
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
                    | Obj::AtomicInt(_)
                    | Obj::Executor(_)
                    | Obj::Socket(_)
                    | Obj::Listener(_)
                    | Obj::Writer(_)
                    | Obj::Reader(_)
                    | Obj::Generator(_) => unreachable!("matched a box above"),
                };
                let core = BoxCore::of_wire(&root).expect("a box handle");
                let snaps = self.box_snapshots(core, span)?;
                out.push_str(&self.display_box(core, &snaps, &mut Vec::new()));
            }
            // Experimental generators stringify opaquely (no protocol hook).
            Obj::Generator(_) => out.push_str("<generator>"),
            // A cell is a transparent by-reference box (never a user-visible operand — reads
            // `CellLoad` first); defensively stringify its inner value.
            Obj::Cell(v) => self.stringify_into(out, v, span, depth)?,
            Obj::Iter { .. } => out.push_str("<iterator>"),
        }
        Ok(())
    }

    // W14-35c — the four open-render helpers below hold `repr_active` open around one container's
    // element walk and close it on the `Ok` AND the `Err` path. They are `#[inline(never)]` so the
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
        entries: &[(u64, Value, Value)],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.repr_active.push(h);
        let mut r = Ok(());
        for (i, (_, k, mv)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            r = self.stringify_nested_into(out, *k, span, depth + 1);
            if r.is_err() {
                break;
            }
            out.push_str(": ");
            r = self.stringify_nested_into(out, *mv, span, depth + 1);
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
        entries: &[(u64, Value)],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        self.repr_active.push(h);
        let mut r = Ok(());
        for (i, (_, e)) in entries.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            r = self.stringify_nested_into(out, *e, span, depth + 1);
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

    pub(super) fn stringify_seq_into(
        &mut self,
        out: &mut String,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        for (i, e) in elems.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.stringify_nested_into(out, *e, span, depth)?;
        }
        Ok(())
    }
}
