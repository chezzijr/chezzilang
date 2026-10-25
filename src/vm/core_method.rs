//! VM: intrinsic protocol methods and the core methods of str, list, map, set, bytes and bytearray.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::call::py_blank;
use super::*;
use crate::ast::BinaryOp;
use crate::base::builtin_method::{
    BytearrayMethod, BytesMethod, ListMethod, MapMethod, SetMethod, StrMethod,
};

impl Vm {
    pub(super) fn intrinsic_proto_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        // W8-22 — `line()`/`col()`/`file()` through the `Error` existential, for every receiver:
        // the fault's origin span, stamped on a `str` handle at one of the three `recover:`
        // boundaries. `None` for a receiver that was never stamped (a user-constructed
        // `Err("...")`, or any non-`str` error, which never carries a stamped span). This hook is
        // miss-only and runs from the struct arm, the enum arm and the catch-all, so a user error
        // type that DEFINES its own `line()` still gets that method instead of this answer.
        // `file()` resolves the span's file to a display path, the text `lexer::render_span` prints
        // for an uncaught fault; `None` when the span's file has no coordinate (`file` 0).
        if args.is_empty() && matches!(method, "line" | "col" | "file") {
            let sp = recv.as_obj().and_then(|r| self.heap.err_span(r));
            let answer = match (method, sp) {
                ("line", Some(sp)) => Some(Value::int(sp.line as i64)),
                ("col", Some(sp)) => Some(Value::int(sp.col as i64)),
                ("file", Some(sp)) => self
                    .program
                    .file_path(sp.file)
                    .map(|p| crate::lexer::display_path(p).to_string_lossy().into_owned())
                    .map(|p| self.alloc_str(p)),
                _ => None,
            };
            return Ok(Some(match answer {
                Some(v) => self.alloc_enum("Option", "Some", &[v]),
                None => self.alloc_enum("Option", "None", &[]),
            }));
        }
        // `Add`/`Sub`/`Mul`/`Div`/`Mod` → the binary-arith primitive.
        let bin = match method {
            "add" => Some(Op::Add),
            "sub" => Some(Op::Sub),
            "mul" => Some(Op::Mul),
            "div" => Some(Op::Div),
            "mod" => Some(Op::Mod),
            _ => None,
        };
        if let Some(bin) = bin
            && args.len() == 1
        {
            self.push(recv);
            self.push(args[0]);
            self.arith(&bin, span)?;
            return Ok(Some(self.pop()));
        }
        // `BitAnd`/`BitOr`/`BitXor`/`Shl`/`Shr` → the bitwise primitive.
        let bit = match method {
            "bitand" => Some(Op::BitAnd),
            "bitor" => Some(Op::BitOr),
            "bitxor" => Some(Op::BitXor),
            "shl" => Some(Op::Shl),
            "shr" => Some(Op::Shr),
            _ => None,
        };
        if let Some(bit) = bit
            && args.len() == 1
        {
            self.push(recv);
            self.push(args[0]);
            self.bitwise(&bit, span)?;
            return Ok(Some(self.pop()));
        }
        match (method, args.len()) {
            ("neg", 0) => Ok(Some(self.neg_value(recv, span)?)),
            // `Stringable` for a kind with no `str` method of its own: the SAME rendering `print`,
            // `str()` and `"{x}"` use. Miss-only, so a declared `str` hook is dispatched first.
            ("str", 0) => {
                let s = self.stringify(recv, span, 0)?;
                Ok(Some(Value::obj(self.heap.alloc(Obj::Str(s.into())))))
            }
            // `Hashable` → the SAME hash a Map/Set key gets, so `x.hash()` can never disagree with
            // `m[x]` / `s.contains(x)` membership. `recv` stays rooted across the (possibly re-entrant,
            // possibly allocating) hash + the result box.
            ("hash", 0) => {
                self.push(recv);
                let hv = self.hash_value(recv, span)?;
                let n = self.make_int(hv as i64);
                self.pop();
                Ok(Some(n))
            }
            // `Comparable` — needed for a NaN operand on a scalar (the pre-dispatch scalar arm only
            // answers when `compare` returns `Some`).
            //
            // NaN is TOTAL here, and by the SAME order the rest of the language sorts by: route the
            // pair through [`Vm::order_key`] — the one ordering site behind `sort()` / `sort_by_key` /
            // `.min()` / `.max()` (`float_order`, NaN deterministically at one end). So there is exactly ONE total order shared by
            // `compare`/`sort`/`min`/`max`, and exactly ONE documented divergence left: that total
            // order (the method) vs IEEE (the operators — `ordered_bool` answers `false` for every NaN
            // comparison, IEEE-754/Python/Rust parity, arith.rs; untouched by design). Not two
            // orderings plus a fault, as W6-3c originally shipped.
            //
            // The NaN END is not fixed by spec: the signbit of `0.0/0.0` is target-dependent (negative
            // on x86 SSE2 ⇒ NaN ranks below `-inf` ⇒ sorts FIRST, `compare < 0`). The guarantee is
            // "same end `sort()` puts it", which is why this delegates instead of re-deriving.
            //
            // Reached only from the `None` (NaN) branch, never unconditionally: `order_key`'s struct
            // branch calls `struct_compare` (a USER `compare`), and intrinsic dispatch must stay
            // miss-only so a user method always wins. A `±0.0` pair therefore still answers via
            // `self.compare` (IEEE-Equal) exactly as before — only NaN comes through here.
            // `order_key`'s terminal `Err` is unreachable behind the `is_numeric` gate.
            // TICKET-146: a tuple / List / Option receiver walks lexicographically (total order).
            ("compare", 1) if self.seq_pair(recv, args[0]) => {
                let other = args[0];
                Ok(Some(Value::int(self.order_key(recv, other, span)? as i64)))
            }
            ("compare", 1) => match self.compare(recv, args[0]) {
                Some(ord) => Ok(Some(Value::int(ord as i64))),
                None if self.is_numeric(recv) && self.is_numeric(args[0]) => {
                    let other = args[0];
                    Ok(Some(Value::int(self.order_key(recv, other, span)? as i64)))
                }
                // Genuinely incomparable TYPES (unreachable from well-typed source): leave the
                // caller's `has no method` error standing, exactly as before.
                None => Ok(None),
            },
            // `Eq` (M23) → the SAME structural equality `==` uses, so `x.eq(y)` can never disagree
            // with `x == y`. Serves the four scalar grants. Miss-only like the rest, so
            // a user type's own `eq` method is dispatched before this ever runs.
            // (`values_equal_guarded` is the operator's OWN worker — not the fault-swallowing
            // `values_equal` test wrapper — so a cyclic operand raises the same recoverable
            // depth fault `==` raises instead of silently answering "not equal".)
            ("eq", 1) => {
                let other = args[0];
                Ok(Some(Value::bool(
                    self.with_roots(&[recv, other], |vm| {
                        vm.values_equal_guarded(recv, other, 0, span)
                    })?,
                )))
            }
            // W7-8 `PathLike` → the RAW OS bytes of a path spelled as a `str`/`bytes`/`bytearray`.
            // `str` hands back its UTF-8 encoding (exactly what `str.encode()` yields), `bytes` IS the
            // answer (returned unchanged — no copy), and a `bytearray` is COPIED into a fresh immutable
            // `bytes` (the explicit `bytes(ba)` semantics: a later mutation of the buffer must not
            // change a path already handed over). Miss-only dispatch, so a user type with its own
            // `as_path` — including `path.Path` — never reaches here.
            ("as_path", 0) => {
                let Some(h) = recv.as_obj() else {
                    return Ok(None);
                };
                let raw: Vec<u8> = match self.heap.get(h) {
                    Obj::Str(s) => s.as_bytes().to_vec(),
                    Obj::Bytes(_) => return Ok(Some(recv)),
                    Obj::ByteArray(b) => b.to_vec(),
                    Obj::BigInt(_)
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
                    | Obj::Generator(_) => return Ok(None),
                };
                let r = self.heap.alloc(Obj::Bytes(raw.into()));
                Ok(Some(Value::obj(r)))
            }
            ("index", 1) => {
                self.push(recv);
                self.push(args[0]);
                self.get_index(span)?;
                Ok(Some(self.pop()))
            }
            // `set_index` returns `nil` per the protocol; `Vm::set_index` pushes nothing.
            ("set_index", 2) => {
                self.push(recv);
                self.push(args[0]);
                self.push(args[1]);
                self.set_index(span)?;
                Ok(Some(Value::nil()))
            }
            // `Slice[R]`'s one argument is the `Range` value; `c.slice(r)` is `c[r]` read as a slice.
            ("slice", 1) => {
                self.push(recv);
                self.push(args[0]);
                self.slice_range(span)?;
                Ok(Some(self.pop()))
            }
            // `Contains` for bytes / bytearray (the only containers with no `contains` method):
            // the `in` operator's own worker.
            ("contains", 1) => {
                self.push(args[0]);
                self.push(recv);
                self.op_contains(span)?;
                Ok(Some(self.pop()))
            }
            _ => Ok(None),
        }
    }

    /// The miss arm of `core_method`, `bytes_method` and `bytearray_method`: a name no dispatcher
    /// owns is tried as an intrinsic protocol method the checker grants this built-in (W6-3), and
    /// only then is a `has no method` fault. Reached from a miss alone, so a real method's fault
    /// keeps its own message.
    fn container_method_miss(
        &mut self,
        h: GcRef,
        type_name: &str,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match self.intrinsic_proto_method(Value::obj(h), method, args, span)? {
            Some(v) => Ok(v),
            None => Err(self.err(format!("type {type_name} has no method '{method}'"), span)),
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
            match args[i].as_obj().map(|ah| vm.heap.get(ah)) {
                Some(Obj::Str(a)) => Ok(a.to_string()),
                _ => Err(vm.err(
                    format!(
                        "{method}() expects a str argument, got {}",
                        vm.type_name(args[i])
                    ),
                    span,
                )),
            }
        };
        match self.heap.get(h) {
            Obj::Str(s) => {
                // Borrow-only dispatch: these five methods return an `int`/`bool` and allocate
                // nothing, so they answer under the `&Obj` borrow instead of paying the clone
                // below. Anything that allocates, re-enters the VM, or needs `&mut self` must
                // stay under the clone (TICKET-072).
                let Some(which) = StrMethod::from_name(method) else {
                    return self.container_method_miss(h, "str", method, args, span);
                };
                match which {
                    StrMethod::Len => {
                        self.arity_err("len", args, 0, span)?;
                        return Ok(Value::int(s.char_len() as i64));
                    }
                    StrMethod::StartsWith => {
                        self.arity_err(method, args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        return Ok(Value::bool(s.starts_with(sub.as_str())));
                    }
                    StrMethod::EndsWith => {
                        self.arity_err(method, args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        return Ok(Value::bool(s.ends_with(sub.as_str())));
                    }
                    StrMethod::Contains => {
                        self.arity_err(method, args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        return Ok(Value::bool(s.contains(sub.as_str())));
                    }
                    StrMethod::IndexOf => {
                        self.arity_err("index_of", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        if sub.is_empty() {
                            return Ok(Value::int(0));
                        }
                        return Ok(match s.find(sub.as_str()) {
                            Some(byte) => Value::int(if s.ascii_bytes().is_some() {
                                byte as i64
                            } else {
                                s[..byte].chars().count() as i64
                            }),
                            None => Value::int(-1),
                        });
                    }
                    StrMethod::Upper
                    | StrMethod::Lower
                    | StrMethod::Trim
                    | StrMethod::Message
                    | StrMethod::Split
                    | StrMethod::Chars
                    | StrMethod::Join
                    | StrMethod::Encode
                    | StrMethod::Replace
                    | StrMethod::Repeat
                    | StrMethod::Reversed
                    | StrMethod::PadLeft
                    | StrMethod::PadRight
                    | StrMethod::Count
                    | StrMethod::StripPrefix
                    | StrMethod::StripSuffix
                    | StrMethod::SplitLines
                    | StrMethod::Strip
                    | StrMethod::ToInt
                    | StrMethod::ToFloat
                    | StrMethod::ParseInt
                    | StrMethod::ParseFloat => {}
                }
                #[cfg(test)]
                super::STR_RECV_CLONED_BYTES.with(|c| c.set(c.get() + s.len()));
                let s = s.to_string();
                match which {
                    StrMethod::Len
                    | StrMethod::StartsWith
                    | StrMethod::EndsWith
                    | StrMethod::Contains
                    | StrMethod::IndexOf => unreachable!("answered under the borrow above"),
                    StrMethod::Upper => {
                        self.arity_err("upper", args, 0, span)?;
                        Ok(self.alloc_str(s.to_uppercase()))
                    }
                    StrMethod::Lower => {
                        self.arity_err("lower", args, 0, span)?;
                        Ok(self.alloc_str(s.to_lowercase()))
                    }
                    StrMethod::Trim => {
                        self.arity_err("trim", args, 0, span)?;
                        Ok(self.alloc_str(s.trim_matches(py_blank).to_string()))
                    }
                    // `str` conforms to `Error`: `message()` returns the string itself. It copies
                    // the receiver's stamped origin onto the fresh string it allocates
                    // (TICKET-045), so a `panic(e.message())` re-raise can still recover it.
                    StrMethod::Message => {
                        self.arity_err("message", args, 0, span)?;
                        let sp = self.heap.err_span(h);
                        let out = self.alloc_str(s.to_string());
                        if let (Some(sp), Some(oh)) = (sp, out.as_obj()) {
                            self.heap.set_err_span(oh, sp);
                        }
                        Ok(out)
                    }
                    StrMethod::Split => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        // std.string.split faults on an empty sep (Python model); the native
                        // method must too — Rust's `split("")` leaks empty edges. Recoverable.
                        if sep.is_empty() {
                            return Err(self.err("split: sep must not be empty".to_string(), span));
                        }
                        let parts: Vec<Value> = s
                            .split(sep.as_str())
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(parts.into()))))
                    }
                    StrMethod::Chars => {
                        self.arity_err("chars", args, 0, span)?;
                        let cs: Vec<Value> = s.chars().map(|c| self.alloc_char(c)).collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(cs.into()))))
                    }
                    // `encode() -> bytes`: UTF-8 encode (str is UTF-8 internally; copy the bytes out
                    // into a new immutable `bytes`). Always succeeds — no fault path. UTF-8 only.
                    StrMethod::Encode => {
                        self.arity_err("encode", args, 0, span)?;
                        let bytes = s.as_bytes().to_vec().into_boxed_slice();
                        Ok(Value::obj(self.heap.alloc(Obj::Bytes(bytes))))
                    }
                    StrMethod::Join => {
                        self.arity_err("join", args, 1, span)?;
                        let Some(lh) = args[0].as_obj() else {
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
                        for (i, item) in items.to_vec().into_iter().enumerate() {
                            let Some(ih) = item.as_obj() else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(item)
                                    ),
                                    span,
                                ));
                            };
                            let Obj::Str(part) = self.heap.get(ih) else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(item)
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
                    StrMethod::Replace => {
                        self.arity_err("replace", args, 2, span)?;
                        let old = str_arg(self, 0)?;
                        let new = str_arg(self, 1)?;
                        // An empty `old` interleaves `new` between every codepoint and at both
                        // ends, matching CPython 3.14.7 `'abc'.replace('','-')` and Go 1.27
                        // `strings.ReplaceAll("abc", "", "-")`, both `-a-b-c-`. Rust's own
                        // `str::replace` already does this, so no special case is needed.
                        Ok(self.alloc_str(s.replace(old.as_str(), new.as_str())))
                    }
                    StrMethod::Repeat => {
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
                    StrMethod::Reversed => {
                        self.arity_err("reversed", args, 0, span)?;
                        Ok(self.alloc_str(s.chars().rev().collect::<String>()))
                    }
                    // `pad_right` is the mirror (CPython `str.ljust`): same guards, fill after `s`.
                    StrMethod::PadLeft | StrMethod::PadRight => {
                        let name = method;
                        self.arity_err(name, args, 2, span)?;
                        let width = self.int_arg(name, &args[0], span)?;
                        let fill = str_arg(self, 1)?;
                        // An empty `fill` can never reach `width` — the old prepend loop spun
                        // forever. Fault EAGERLY (before the width check) so the diagnostic does
                        // not depend on whether padding was actually needed.
                        if fill.is_empty() {
                            return Err(self.err(format!("{name}: fill must not be empty"), span));
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
                                if name == "pad_left" {
                                    out.extend(fill.chars().cycle().take(need));
                                    out.push_str(&s);
                                } else {
                                    out.push_str(&s);
                                    out.extend(fill.chars().cycle().take(need));
                                }
                                Ok(self.alloc_str(out))
                            }
                            None => Err(self.err("string pad capacity overflow".to_string(), span)),
                        }
                    }
                    StrMethod::Count => {
                        self.arity_err("count", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.string / Python / Go: empty -> codepoint-len + 1; otherwise
                        // non-overlapping count.
                        if sub.is_empty() {
                            Ok(Value::int(s.chars().count() as i64 + 1))
                        } else {
                            Ok(Value::int(s.matches(sub.as_str()).count() as i64))
                        }
                    }
                    StrMethod::StripPrefix => {
                        self.arity_err("strip_prefix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_prefix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    StrMethod::StripSuffix => {
                        self.arity_err("strip_suffix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_suffix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    StrMethod::SplitLines => {
                        self.arity_err("split_lines", args, 0, span)?;
                        // Splits on "\r\n", "\n" or a lone "\r" — the same three terminators
                        // `io::Reader::read_line`/`.lines()` recognize (src/native/mod.rs). A
                        // trailing terminator yields no final empty piece (matches CPython's
                        // `str.splitlines()`), so a plain "\n"-only input drops the historical
                        // trailing `""` too.
                        let mut lines: Vec<String> = Vec::new();
                        let mut cur = String::new();
                        let mut it = s.chars().peekable();
                        while let Some(c) = it.next() {
                            match c {
                                '\r' => {
                                    if it.peek() == Some(&'\n') {
                                        it.next();
                                    }
                                    lines.push(std::mem::take(&mut cur));
                                }
                                '\n' => {
                                    lines.push(std::mem::take(&mut cur));
                                }
                                _ => cur.push(c),
                            }
                        }
                        if !cur.is_empty() {
                            lines.push(cur);
                        }
                        let parts: Vec<Value> =
                            lines.into_iter().map(|p| self.alloc_str(p)).collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(parts.into()))))
                    }
                    // `strip` is a trim alias.
                    StrMethod::Strip => {
                        self.arity_err("strip", args, 0, span)?;
                        Ok(self.alloc_str(s.trim_matches(py_blank).to_string()))
                    }
                    // gap #7: safe numeric parse — None on bad input (trims like int()/float()).
                    StrMethod::ToInt => {
                        self.arity_err("to_int", args, 0, span)?;
                        match strip_num_underscores(s.trim()).and_then(|t| t.parse::<i64>().ok()) {
                            Some(n) => {
                                let nv = self.make_int(n);
                                Ok(self.alloc_enum("Option", "Some", &[nv]))
                            }
                            None => Ok(self.alloc_enum("Option", "None", &[])),
                        }
                    }
                    StrMethod::ToFloat => {
                        self.arity_err("to_float", args, 0, span)?;
                        match strip_num_underscores(s.trim()).and_then(|t| t.parse::<f64>().ok()) {
                            Some(f) => {
                                let fv = self.box_float(f);
                                Ok(self.alloc_enum("Option", "Some", &[fv]))
                            }
                            None => Ok(self.alloc_enum("Option", "None", &[])),
                        }
                    }
                    // Result-returning parse siblings of to_int/to_float — carry a human-readable
                    // Err(msg) instead of None (trims first, like int()/float()/to_int/to_float).
                    StrMethod::ParseInt => {
                        self.arity_err("parse_int", args, 0, span)?;
                        match parse_i64_pep515(s.trim()) {
                            Ok(n) => {
                                let nv = self.make_int(n);
                                Ok(self.alloc_enum("Result", "Ok", &[nv]))
                            }
                            Err(IntParseErr::Overflow) => {
                                let msg = self.alloc_str(format!(
                                    "'{s}' overflows i64 (range -9223372036854775808..=9223372036854775807)"
                                ));
                                Ok(self.alloc_enum("Result", "Err", &[msg]))
                            }
                            Err(IntParseErr::Malformed) => {
                                let msg =
                                    self.alloc_str(format!("cannot parse '{s}' as an integer"));
                                Ok(self.alloc_enum("Result", "Err", &[msg]))
                            }
                        }
                    }
                    StrMethod::ParseFloat => {
                        self.arity_err("parse_float", args, 0, span)?;
                        match strip_num_underscores(s.trim()).and_then(|t| t.parse::<f64>().ok()) {
                            Some(f) => {
                                let fv = self.box_float(f);
                                Ok(self.alloc_enum("Result", "Ok", &[fv]))
                            }
                            None => {
                                let msg = self.alloc_str(format!("cannot parse '{s}' as a float"));
                                Ok(self.alloc_enum("Result", "Err", &[msg]))
                            }
                        }
                    }
                }
            }
            Obj::List(items) => {
                let Some(which) = ListMethod::from_name(method) else {
                    return self.container_method_miss(h, "list", method, args, span);
                };
                match which {
                    ListMethod::Len => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::int(items.len() as i64))
                    }
                    ListMethod::Push => {
                        self.arity_err("push", args, 1, span)?;
                        let v = args[0];
                        let heap = self.heap.write(h);
                        let Obj::List(items) = heap.get(h) else {
                            unreachable!()
                        };
                        items.push(heap, v);
                        Ok(Value::nil())
                    }
                    ListMethod::Pop => {
                        self.arity_err("pop", args, 0, span)?;
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        let popped = items.pop();
                        // M19 lever #2 — route through `alloc_enum` so the dense `variant_id` is stamped
                        // (replacing the two ad-hoc per-instance `Box<str>` builds).
                        Ok(match popped {
                            Some(v) => self.alloc_enum("Option", "Some", &[v]),
                            None => self.alloc_enum("Option", "None", &[]),
                        })
                    }
                    ListMethod::Reverse => {
                        self.arity_err("reverse", args, 0, span)?;
                        let heap = self.heap.write(h);
                        let Obj::List(items) = heap.get(h) else {
                            unreachable!()
                        };
                        let mut elems = items.to_vec();
                        elems.reverse();
                        items.replace(heap, &elems);
                        Ok(Value::nil())
                    }
                    ListMethod::Sort => {
                        self.arity_err("sort", args, 0, span)?;
                        // In place, ascending. Checker guarantees a homogeneous orderable element type.
                        // A list of Comparable structs/enums orders via each one's `compare` (engine
                        // re-entry, so a merge sort that holds `&mut self`); so does a list of tuples /
                        // Lists / Options (TICKET-146; an `Option` is an `Obj::Enum`), whose elements may
                        // hold a struct. Primitives use the faster `value_order`. Str elements live on
                        // the heap, so `value_order` needs `&self.heap` — clone the elements out, sort
                        // (no alloc/closure → no GC for the primitive path), then write back.
                        let user_compare = matches!(items.try_get(0).and_then(|v| v.as_obj()), Some(hh) if matches!(self.heap.get(hh), Obj::Struct { .. } | Obj::Enum { .. } | Obj::Tuple(_) | Obj::List(_)));
                        if user_compare {
                            // A user compare (struct or enum) re-enters the VM (may GC) → rooted, index-based sort.
                            return self.list_sort_structs(h, span);
                        }
                        let mut elems = items.to_vec();
                        elems.sort_by(|a, b| self.value_order(*a, *b));
                        let heap = self.heap.write(h);
                        let Obj::List(items) = heap.get(h) else {
                            unreachable!()
                        };
                        items.replace(heap, &elems);
                        Ok(Value::nil())
                    }
                    // `elems` is a Rust-local clone and `target` came off the stack at dispatch, so
                    // both the receiver and the target are rooted across the (re-entrant) `eq`.
                    ListMethod::Contains => {
                        self.arity_err("contains", args, 1, span)?;
                        let target = args[0];
                        let elems = items.to_vec();
                        let found = self
                            .with_roots(&[Value::obj(h), target], |vm| {
                                vm.seq_slot(&elems, target, span)
                            })?
                            .is_some();
                        Ok(Value::bool(found))
                    }
                    ListMethod::IndexOf => {
                        self.arity_err("index_of", args, 1, span)?;
                        let target = args[0];
                        let elems = items.to_vec();
                        let idx = self.with_roots(&[Value::obj(h), target], |vm| {
                            vm.seq_slot(&elems, target, span)
                        })?;
                        Ok(Value::int(idx.map(|i| i as i64).unwrap_or(-1)))
                    }
                    ListMethod::Concat => {
                        self.arity_err("concat", args, 1, span)?;
                        let mut out = items.to_vec();
                        out.extend(self.expect_list_obj("concat", args[0], span)?);
                        // `out` is fully built and moved into the new Obj before any GC can run.
                        Ok(Value::obj(self.heap.alloc(Obj::List(out.into()))))
                    }
                    ListMethod::Extend => {
                        self.arity_err("extend", args, 1, span)?;
                        // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
                        let appended = self.expect_list_obj("extend", args[0], span)?;
                        let heap = self.heap.write(h);
                        let Obj::List(items) = heap.get(h) else {
                            unreachable!()
                        };
                        items.extend_from_slice(heap, &appended);
                        Ok(Value::nil())
                    }
                    ListMethod::Sum => {
                        // A one-argument call carries the CHECKER's seed: a bare `0.0` for a plain
                        // `List[float]` (a user cannot spell `.sum(x)` — the harvested sig takes no
                        // parameters). Its VALUE is the empty list's own answer and only has to force
                        // the fold below into its float arm.
                        let seed_is_float = if args.len() == 1 {
                            debug_assert!(args[0].is_float());
                            true
                        } else {
                            self.arity_err("sum", args, 0, span)?;
                            false
                        };
                        // Clone out so `make_int`/`box_float` (which mutate the heap) don't collide with
                        // the `items` heap borrow.
                        let elems = items.to_vec();
                        let any_float = seed_is_float || elems.iter().any(|v| v.is_float());
                        if any_float {
                            // CPython 3.12+ `sum`: Neumaier-compensated, ints included. A non-finite
                            // compensation is dropped so an overflowing `hi` stays `inf`, never `nan`.
                            let (mut hi, mut lo) = (0.0_f64, 0.0_f64);
                            for &v in &elems {
                                let x = if v.is_float() {
                                    self.float_of(v)
                                } else if let Some(n) = self.int_val(v) {
                                    n as f64
                                } else {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(v)), span));
                                };
                                let t = hi + x;
                                if hi.abs() >= x.abs() {
                                    lo += (hi - t) + x;
                                } else {
                                    lo += (x - t) + hi;
                                }
                                hi = t;
                            }
                            let r = if lo != 0.0 && lo.is_finite() {
                                hi + lo
                            } else {
                                hi
                            };
                            Ok(self.box_float(r))
                        } else {
                            let mut acc = 0_i64;
                            for &v in &elems {
                                if let Some(n) = self.int_val(v) {
                                    acc = self.int_op(BinaryOp::Add, acc, n, span)?;
                                } else {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(v)), span));
                                }
                            }
                            Ok(self.make_int(acc))
                        }
                    }
                    ListMethod::First | ListMethod::Last => {
                        self.arity_err(method, args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let picked = if which == ListMethod::First {
                            items.try_get(0)
                        } else {
                            items.len().checked_sub(1).and_then(|i| items.try_get(i))
                        };
                        Ok(match picked {
                            Some(v) => self.alloc_enum("Option", "Some", &[v]),
                            None => self.alloc_enum("Option", "None", &[]),
                        })
                    }
                    ListMethod::Reversed => {
                        // A NEW list (clone then reverse) — never mutate the receiver (that is `reverse`).
                        self.arity_err("reversed", args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let mut out = items.to_vec();
                        out.reverse();
                        Ok(Value::obj(self.heap.alloc(Obj::List(out.into()))))
                    }
                    ListMethod::Map
                    | ListMethod::Filter
                    | ListMethod::Fold
                    | ListMethod::TakeWhile
                    | ListMethod::DropWhile
                    | ListMethod::Count
                    | ListMethod::Position => self.list_hof(h, method, args.to_vec(), span),
                    ListMethod::SortBy => self.list_sort_by(h, args.to_vec(), span),
                    ListMethod::SortByKey => self.list_sort_by_key(h, args.to_vec(), span),
                    ListMethod::MinBy | ListMethod::MaxBy => {
                        self.list_min_max_by(h, args.to_vec(), which == ListMethod::MaxBy, span)
                    }
                    ListMethod::Copy => {
                        // TICKET-160 — a NEW list holding the SAME element handles: shallow, Python list.copy().
                        self.arity_err("copy", args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let out = items.clone();
                        Ok(Value::obj(self.heap.alloc(Obj::List(out))))
                    }
                    ListMethod::Insert => {
                        // Python-clamp: i>len appends, negatives are len-relative and clamp to 0. Never faults.
                        self.arity_err("insert", args, 2, span)?;
                        let Some(i) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        let x = args[1];
                        let heap = self.heap.write(h);
                        let Obj::List(items) = heap.get(h) else {
                            unreachable!()
                        };
                        let len = items.len() as i64;
                        let idx = if i < 0 { (i + len).max(0) } else { i.min(len) } as usize;
                        items.insert(heap, idx, x);
                        Ok(Value::nil())
                    }
                    ListMethod::Remove => {
                        // Returns the removed element; the `xs[i]` negative-index rule; true OOB faults.
                        self.arity_err("remove", args, 1, span)?;
                        let Some(i) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        let len = items.len();
                        // M3.0a — `remove` checks `idx` against its own snapshot: another thread
                        // can shrink the list after `len` was read.
                        match crate::slice::norm_index(i, len).and_then(|idx| items.remove(idx)) {
                            Some(v) => Ok(v),
                            None => {
                                Err(self.err(format!("index {i} out of bounds (len {len})"), span))
                            }
                        }
                    }
                    // min/max may re-enter the VM (a Comparable-struct `compare` → GC), which invalidates
                    // the `items` borrow — route to a rooted, index-based helper (mirrors `sort`'s struct path).
                    ListMethod::Min | ListMethod::Max => {
                        self.arity_err(method, args, 0, span)?;
                        self.list_reduce_extreme(h, which == ListMethod::Max, span)
                    }
                    // unique/dedup: NEW list, never mutate the receiver. Equality is the same
                    // `elem_equal` `contains` uses (works on floats) — which since M23 can dispatch a
                    // user `eq` and so re-enter the VM. Root the snapshotted ELEMENTS, not just the
                    // receiver: an `eq` that clears the receiver mid-walk orphans every element
                    // `items`/`out` still hold (`out` ⊆ `items`, so `items` covers both).
                    ListMethod::Unique | ListMethod::Dedup => {
                        self.arity_err(method, args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let items = items.to_vec();
                        let out = self.with_elem_roots(&[Value::obj(h)], &items, |vm| {
                            let mut out: Vec<Value> = Vec::new();
                            if which == ListMethod::Dedup {
                                // Collapse only CONSECUTIVE runs (Rust `Vec::dedup`): keep an element
                                // iff it differs from the previously-kept one.
                                for &v in &items {
                                    let keep = match out.last() {
                                        Some(&p) => !vm.elem_equal(p, v, 0, span)?,
                                        None => true,
                                    };
                                    if keep {
                                        out.push(v);
                                    }
                                }
                            } else if items.iter().all(|&v| vm.is_flat_hash_key(v)) {
                                // W8-34: every element is a flat scalar key, so dedupe through a hash
                                // index in one pass instead of a linear scan per element. The gate is
                                // all-or-nothing over the WHOLE list, never per-element: `bytes ==
                                // bytearray` is content-equal, so mixing would let an indexed `bytes`
                                // and a scanned `bytearray` of equal content both survive. A bucket hit
                                // is still confirmed by `elem_equal` — the hash only picks candidates,
                                // the compare remains the verdict (DEC-014).
                                let seen = SetData::default();
                                for &v in &items {
                                    let hv = vm.scalar_hash(v);
                                    let mut dup = false;
                                    for p in seen.candidates(hv) {
                                        if vm.elem_equal(seen.entries.get(p).1, v, 0, span)? {
                                            dup = true;
                                            break;
                                        }
                                    }
                                    if !dup {
                                        seen.push(&vm.heap, hv, v);
                                    }
                                }
                                out = seen.entries.iter().map(|(_, v)| v).collect();
                            } else {
                                // Remove ALL duplicates, first-occurrence order (Python `dict.fromkeys`).
                                // Fallback for any non-flat element (container, Struct/Enum,
                                // ByteArray) — same O(n^2) scan as before the hash-index fast path above.
                                for &v in &items {
                                    if vm.seq_slot(&out, v, span)?.is_none() {
                                        out.push(v);
                                    }
                                }
                            }
                            Ok(out)
                        })?;
                        Ok(Value::obj(self.heap.alloc(Obj::List(out.into()))))
                    }
                    // chunk/windows build a `List[List[T]]`. Each inner-list alloc may GC, so the outer
                    // list is ROOTED on the operand stack and every inner handle is pushed into it
                    // IMMEDIATELY after allocation (no intervening alloc), keeping prior inners reachable.
                    ListMethod::Chunk | ListMethod::Windows => {
                        self.arity_err(method, args, 1, span)?;
                        let Some(n) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        if n <= 0 {
                            let what = if which == ListMethod::Chunk {
                                "chunk"
                            } else {
                                "window"
                            };
                            return Err(
                                self.err(format!("{what} size must be positive, got {n}"), span)
                            );
                        }
                        let n = n as usize;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let items = items.to_vec();
                        let len = items.len();
                        let outer = self.heap.alloc(Obj::List(RVec::new()));
                        self.push(Value::obj(outer)); // ROOT the outer list across inner allocs
                        if which == ListMethod::Chunk {
                            let mut start = 0;
                            while start < len {
                                let end = (start + n).min(len);
                                let inner = self
                                    .heap
                                    .alloc(Obj::List(RVec::from_slice(&items[start..end])));
                                let heap = self.heap.write(outer);
                                if let Obj::List(o) = heap.get(outer) {
                                    o.push(heap, Value::obj(inner));
                                }
                                start = end;
                            }
                        } else if n <= len {
                            // windows: `n > len` → no windows → empty outer list (loop skipped).
                            for start in 0..=len - n {
                                let inner = self
                                    .heap
                                    .alloc(Obj::List(RVec::from_slice(&items[start..start + n])));
                                let heap = self.heap.write(outer);
                                if let Obj::List(o) = heap.get(outer) {
                                    o.push(heap, Value::obj(inner));
                                }
                            }
                        }
                        self.pop(); // unroot outer
                        Ok(Value::obj(outer))
                    }
                }
            }
            Obj::Map(m) => {
                let Some(which) = MapMethod::from_name(method) else {
                    return self.container_method_miss(h, "map", method, args, span);
                };
                match which {
                    MapMethod::Len => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::int(m.entries.len() as i64))
                    }
                    MapMethod::Contains => {
                        self.arity_err("contains", args, 1, span)?;
                        let key = args[0];
                        let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                        let found = self.map_probe(h, hk, key, span)?.is_some();
                        Ok(Value::bool(found))
                    }
                    MapMethod::Get => {
                        self.arity_err("get", args, 1, span)?;
                        let key = args[0];
                        let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                        match self.map_lookup(h, hk, key, span)? {
                            Some(v) => Ok(self.alloc_enum("Option", "Some", &[v])),
                            None => Ok(self.alloc_enum("Option", "None", &[])),
                        }
                    }
                    MapMethod::Keys => {
                        self.arity_err("keys", args, 0, span)?;
                        let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| k).collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(keys.into()))))
                    }
                    MapMethod::Values => {
                        self.arity_err("values", args, 0, span)?;
                        let vals: Vec<Value> = m.entries.iter().map(|(_, _, v)| v).collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(vals.into()))))
                    }
                    MapMethod::Items => {
                        // TICKET-160 — one (k, v) tuple per entry in m.entries order, the SAME order keys() reads,
                        // so Map(m.items()) == m. Heap::alloc never collects, so the tuples need no rooting
                        // before the list alloc.
                        self.arity_err("items", args, 0, span)?;
                        let out = self.map_pairs(h);
                        Ok(Value::obj(self.heap.alloc(Obj::List(out.into()))))
                    }
                    MapMethod::Copy => {
                        // TICKET-160 — shallow in the VALUES (Python dict.copy()); each struct/enum KEY is
                        // snapshotted exactly as merge() does (Go value-key model), so a key reached through the
                        // copy never aliases the original's. snapshot_key is pure alloc: no rooting.
                        self.arity_err("copy", args, 0, span)?;
                        let mut out = m.entries.to_vec();
                        for e in &mut out {
                            e.1 = self.snapshot_key(e.1);
                        }
                        Ok(Value::obj(
                            self.heap.alloc(Obj::Map(MapData::from_entries(out))),
                        ))
                    }
                    MapMethod::Remove => {
                        self.arity_err("remove", args, 1, span)?;
                        let key = args[0];
                        let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                        let pos = self.map_probe(h, hk, key, span)?;
                        match pos {
                            Some(i) => {
                                let heap = self.heap.write(h);
                                let Obj::Map(m) = heap.get(h) else {
                                    unreachable!()
                                };
                                // M3.0a — `None`: another thread removed the entry after the
                                // probe, a miss.
                                match m.remove_at(heap, i) {
                                    Some((_, _, v)) => Ok(self.alloc_enum("Option", "Some", &[v])),
                                    None => Ok(self.alloc_enum("Option", "None", &[])),
                                }
                            }
                            None => Ok(self.alloc_enum("Option", "None", &[])),
                        }
                    }
                    MapMethod::Insert => {
                        self.arity_err("insert", args, 2, span)?;
                        let (key, val) = (args[0], args[1]);
                        let hk = self.hash_key_rooted(key, &[Value::obj(h), key, val], span)?;
                        match self.map_upsert_in_heap(h, hk, key, val, span)? {
                            Some(old) => Ok(self.alloc_enum("Option", "Some", &[old])),
                            None => Ok(self.alloc_enum("Option", "None", &[])),
                        }
                    }
                    MapMethod::Merge | MapMethod::Update => {
                        self.arity_err(method, args, 1, span)?;
                        // Snapshot the incoming entries (with their cached hashes — engine-wide
                        // consistent, so reuse is sound) first; handles `m.merge(m)`/`m.update(m)`.
                        let incoming = match args[0].as_obj().map(|oh| self.heap.get(oh)) {
                            Some(Obj::Map(o)) => o.entries.to_vec(),
                            _ => {
                                return Err(self.err(
                                    format!(
                                        "{method}() expects a map argument, got {}",
                                        self.type_name(args[0])
                                    ),
                                    span,
                                ));
                            }
                        };
                        if which == MapMethod::Merge {
                            // Build the result fresh, snapshotting EVERY struct/enum key (Go
                            // value-key model) so the new map aliases neither the receiver's nor the
                            // argument's stored keys — `m.clone()` would carry the receiver's key handles
                            // by reference. The receiver's own keys are already unique, so they need no
                            // dedup; the argument's entries upsert (last-wins). `snapshot_key` is pure
                            // alloc (no GC), so the local `out`'s contents stay valid across it.
                            let mine = match self.heap.get(h).as_map() {
                                Some(m) => m.entries.to_vec(),
                                None => unreachable!(),
                            };
                            // Root BOTH source maps: `out` is a Rust local and its snapshot keys are
                            // reachable only from it, so the receiver + argument (whose entries every
                            // value in `out` came from) must stay alive across a re-entrant user `eq`.
                            let out = self.with_roots(&[Value::obj(h), args[0]], |vm| {
                                let out = MapData::default();
                                for (hk, key, val) in mine {
                                    // A snapshot key is a FRESH object reachable only from the local
                                    // `out`, so root it on the operand stack too (`with_roots` truncates
                                    // back past these on the way out).
                                    let key = vm.snapshot_key(key);
                                    vm.push(key);
                                    out.push(&vm.heap, hk, key, val);
                                }
                                for (hk, key, val) in incoming {
                                    let pos =
                                        vm.map_slot(&out.entries, out.candidates(hk), key, span)?;
                                    if pos.and_then(|i| out.set_value(i, val)).is_none() {
                                        let key = vm.snapshot_key(key);
                                        vm.push(key);
                                        out.push(&vm.heap, hk, key, val);
                                    }
                                }
                                Ok(out)
                            })?;
                            Ok(Value::obj(self.heap.alloc(Obj::Map(out))))
                        } else {
                            // Root both maps for the WHOLE loop: the not-yet-upserted tail of `incoming`
                            // is a Rust local reachable only through the argument map, which is unrooted
                            // when it is an inline temporary (`m.update(make_map())`) — and each upsert
                            // now probes with a possibly re-entrant user `eq`.
                            self.with_roots(&[Value::obj(h), args[0]], |vm| {
                                for (hk, key, val) in incoming {
                                    let _replaced = vm.map_upsert_in_heap(h, hk, key, val, span)?;
                                }
                                Ok(())
                            })?;
                            Ok(Value::nil())
                        }
                    }
                }
            }
            Obj::Set(s) => {
                let Some(which) = SetMethod::from_name(method) else {
                    return self.container_method_miss(h, "set", method, args, span);
                };
                match which {
                    SetMethod::Len => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::int(s.entries.len() as i64))
                    }
                    SetMethod::Contains => {
                        self.arity_err("contains", args, 1, span)?;
                        let x = args[0];
                        let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                        Ok(Value::bool(self.set_probe(h, hx, x, span)?.is_some()))
                    }
                    SetMethod::Insert => {
                        self.arity_err("insert", args, 1, span)?;
                        let x = args[0];
                        let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                        let present = self.set_probe(h, hx, x, span)?.is_some();
                        if !present {
                            // Snapshot a struct/enum element on insert (Go value-key model) so a
                            // later mutation of the caller's live value can't corrupt the set. Pure alloc
                            // (no GC), so no rooting needed before the immediately-following push.
                            let x = self.snapshot_key(x);
                            let heap = self.heap.write(h);
                            let Obj::Set(s) = heap.get(h) else {
                                unreachable!()
                            };
                            s.push(heap, hx, x);
                        }
                        Ok(Value::bool(!present))
                    }
                    SetMethod::Remove => {
                        self.arity_err("remove", args, 1, span)?;
                        let x = args[0];
                        let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                        let pos = self.set_probe(h, hx, x, span)?;
                        match pos {
                            Some(i) => {
                                let heap = self.heap.write(h);
                                let Obj::Set(s) = heap.get(h) else {
                                    unreachable!()
                                };
                                // M3.0a — `None` is a miss (see `MapMethod::Remove`).
                                Ok(Value::bool(s.remove_at(heap, i).is_some()))
                            }
                            None => Ok(Value::bool(false)),
                        }
                    }
                    SetMethod::Union | SetMethod::Intersection | SetMethod::Difference => {
                        self.arity_err(method, args, 1, span)?;
                        // Both operands already carry per-element cached hashes, so set algebra needs no
                        // re-hashing — purely build a fresh hash set, deduping and membership-testing via
                        // the cached hashes confirmed by `elem_equal` (which since M23 may dispatch a user
                        // `eq` and re-enter the VM; `mine`/`other`/`out` are Rust locals holding elements
                        // reachable only through the two source sets, so both are rooted).
                        let mine = match self.heap.get(h).as_set() {
                            Some(s) => s.entries.to_vec(),
                            None => unreachable!(),
                        };
                        let other = self.set_arg(args[0], method, span)?;
                        let add = |vm: &mut Vm,
                                   set: &SetData,
                                   he: u64,
                                   e: Value|
                         -> Result<(), RuntimeError> {
                            if vm
                                .set_slot(&set.entries, set.candidates(he), e, span)?
                                .is_none()
                            {
                                set.push(&vm.heap, he, e);
                            }
                            Ok(())
                        };
                        let out = self.with_roots(&[Value::obj(h), args[0]], |vm| {
                            let out = SetData::default();
                            // This arm serves exactly Union, Intersection and Difference.
                            if which == SetMethod::Union {
                                for (he, e) in mine.iter().copied().chain(other.entries.iter()) {
                                    add(vm, &out, he, e)?;
                                }
                            } else {
                                // intersection keeps mine's elements present in other; difference drops them.
                                let keep_when_present = which == SetMethod::Intersection;
                                for (he, e) in &mine {
                                    let in_other = vm
                                        .set_slot(&other.entries, other.candidates(*he), *e, span)?
                                        .is_some();
                                    if in_other == keep_when_present {
                                        add(vm, &out, *he, *e)?;
                                    }
                                }
                            }
                            Ok(out)
                        })?;
                        Ok(Value::obj(self.heap.alloc(Obj::Set(out))))
                    }
                    SetMethod::Copy => {
                        // TICKET-160 — insertion order kept; each struct/enum element snapshotted as Set.insert does.
                        self.arity_err("copy", args, 0, span)?;
                        let mut out = s.entries.to_vec();
                        for e in &mut out {
                            e.1 = self.snapshot_key(e.1);
                        }
                        Ok(Value::obj(
                            self.heap.alloc(Obj::Set(SetData::from_entries(out))),
                        ))
                    }
                }
            }
            Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Tuple(_)
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
            | Obj::Generator(_) => {
                unreachable!("core_method dispatched a non-str/list/map/set receiver")
            }
        }
    }

    /// Built-in methods on a `bytearray` receiver (`h` is the heap handle): `len`, `push(int 0..=255)`,
    /// `pop() -> Option[int]`, `extend(bytes|bytearray|List[int])`. Mirrors the checker's file-backed
    /// `native struct bytearray` method table in `std/prelude.chz` (the retired `bytearray_method_sig`) —
    /// keep them in lockstep.
    /// Mutators write IN PLACE through the heap slot (`get_mut`), exactly like the `list` methods, so a
    /// second binding to the same `bytearray` observes the change.
    pub(super) fn bytearray_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = BytearrayMethod::from_name(method) else {
            return self.container_method_miss(h, "bytearray", method, args, span);
        };
        match which {
            BytearrayMethod::Len => {
                self.arity_err("len", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::int(b.len() as i64))
            }
            BytearrayMethod::Push => {
                self.arity_err("push", args, 1, span)?;
                let byte = match self.int_val(args[0]) {
                    Some(n) if (0..=255).contains(&n) => n as u8,
                    Some(n) => {
                        return Err(self.err(
                            format!("byte value {n} out of range (must be 0..=255)"),
                            span,
                        ));
                    }
                    None => {
                        return Err(self.err(
                            format!("push() expects an int, got {}", self.type_name(args[0])),
                            span,
                        ));
                    }
                };
                let heap = self.heap.write(h);
                let Obj::ByteArray(b) = heap.get(h) else {
                    unreachable!()
                };
                b.push(heap, byte);
                Ok(Value::nil())
            }
            BytearrayMethod::Pop => {
                self.arity_err("pop", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                let popped = b.pop();
                Ok(match popped {
                    Some(x) => self.alloc_enum("Option", "Some", &[Value::int(x as i64)]),
                    None => self.alloc_enum("Option", "None", &[]),
                })
            }
            BytearrayMethod::Extend => {
                self.arity_err("extend", args, 1, span)?;
                // Snapshot the other side first (so `ba.extend(ba)` terminates) — also validates
                // ints 0..=255 / element types up front, mirroring the constructor.
                let appended = self.collect_bytes_arg("extend", args[0], span)?;
                let heap = self.heap.write(h);
                let Obj::ByteArray(b) = heap.get(h) else {
                    unreachable!()
                };
                b.extend_from_slice(heap, &appended);
                Ok(Value::nil())
            }
            // `decode() -> str`: UTF-8 decode the current buffer. Invalid UTF-8 is a RECOVERABLE
            // fault (catchable by `recover:`), never a panic — mirrors the bytes path + the interp.
            BytearrayMethod::Decode => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.to_vec();
                self.decode_utf8(&bytes, span)
            }
            // `reverse() -> None` reverses in place, `reversed() -> bytearray` returns a NEW buffer
            // (Python `bytearray.reverse()`, `bytearray(reversed(b))`).
            BytearrayMethod::Reverse => {
                self.arity_err("reverse", args, 0, span)?;
                let heap = self.heap.write(h);
                let Obj::ByteArray(b) = heap.get(h) else {
                    unreachable!()
                };
                let mut bytes = b.to_vec();
                bytes.reverse();
                b.replace(heap, &bytes);
                Ok(Value::nil())
            }
            BytearrayMethod::Reversed => {
                self.arity_err("reversed", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let mut dup = b.to_vec();
                dup.reverse();
                Ok(Value::obj(self.heap.alloc(Obj::ByteArray(dup.into()))))
            }
            // `copy() -> bytearray`: a new buffer with the same bytes (Python `bytearray.copy()`).
            BytearrayMethod::Copy => {
                self.arity_err("copy", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let dup = b.clone();
                Ok(Value::obj(self.heap.alloc(Obj::ByteArray(dup))))
            }
        }
    }

    /// `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors the checker's
    /// file-backed `native struct bytes` method table in `std/prelude.chz` (the retired `bytes_method_sig`)
    /// — keep them in lockstep.
    pub(super) fn bytes_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = BytesMethod::from_name(method) else {
            return self.container_method_miss(h, "bytes", method, args, span);
        };
        match which {
            BytesMethod::Decode => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            // W7-8 `decode_lossy() -> str`: UTF-8 decode with each maximal invalid subsequence
            // replaced by U+FFFD (Python's `b.decode(errors="replace")`, Rust's
            // `String::from_utf8_lossy`). NEVER faults — it is the DISPLAY twin of `decode()`, and it
            // is what `path.Path.str()` (a `Stringable`, which cannot fail) is built on.
            BytesMethod::DecodeLossy => {
                self.arity_err("decode_lossy", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let s = String::from_utf8_lossy(b).into_owned();
                Ok(self.alloc_str(s))
            }
            BytesMethod::Len => {
                self.arity_err("len", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::int(b.len() as i64))
            }
            // A NEW `bytes` with the bytes in reverse order.
            BytesMethod::Reversed => {
                self.arity_err("reversed", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let out: Box<[u8]> = b.iter().rev().copied().collect();
                Ok(Value::obj(self.heap.alloc(Obj::Bytes(out))))
            }
        }
    }

    /// UTF-8 decode a byte slice into a new heap `str`. Invalid UTF-8 maps to a RECOVERABLE
    /// RuntimeError (catchable by `recover:`), not a panic.
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
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Set(s)) => Ok(s.clone()),
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
        Value::obj(self.heap.alloc(Obj::Str(s.into())))
    }

    /// M19 Phase 3 — the 1-char `str` value for `c`, in a single allocation. `c.to_string()` +
    /// `into_boxed_str` is two allocs (a `String`, then a shrink-to-fit realloc); encoding straight
    /// into a stack buffer and boxing the `&str` is one. Used by string indexing/iteration/`chr`.
    pub(super) fn alloc_char(&mut self, c: char) -> Value {
        let mut buf = [0u8; 4];
        Value::obj(
            self.heap
                .alloc(Obj::Str((&*c.encode_utf8(&mut buf)).into())),
        )
    }
}
