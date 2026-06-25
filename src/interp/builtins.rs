//! Built-in functions available without any import (`range`, `int`/`str`/`float`).
//! Math intrinsics like `sqrt` are NOT builtins — they live in `std.math` (M6c).
//! `print` lives in the interpreter itself because it writes to the captured output buffer.

use super::{RuntimeError, Value};
use crate::ast::Span;
use std::cell::RefCell;
use std::rc::Rc;

/// The names handled here. Used so the interpreter can tell a builtin call from a user call.
pub fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "range"
            | "int"
            | "float"
            | "str"
            | "ord"
            | "chr"
            | "Set"
            | "List"
            | "Map"
            | "bytes"
            | "bytearray"
            // `panic(msg)` is intercepted in `eval_call` (raises an Err, not an Ok value), so the
            // pure `call` table below never sees it — but it must be a recognized builtin name.
            | "panic"
    )
}

/// Dispatch a builtin by name. Caller guarantees `is_builtin(name)`.
pub fn call(name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
    match name {
        "range" => range(&args, span),
        "int" => cast_int(&args, span),
        "float" => cast_float(&args, span),
        "str" => cast_str(&args, span),
        "ord" => ord(&args, span),
        "chr" => chr(&args, span),
        // `set` is intercepted by `Interp` (it may hash struct elements via re-entrant `hash()`).
        _ => unreachable!("call dispatched a non-builtin or engine-routed builtin: {name}"),
    }
}

/// Dispatch a core-type method (`s.upper()`, `xs.push(x)`, …) on a `str` or `list` receiver (M6).
/// Caller guarantees `recv` is `Value::Str` or `Value::List`. Mirrors the VM's `do_method_call`
/// and the checker's `str_method_sig`/`list_method_sig` — keep the three in lockstep.
pub fn call_method(
    recv: &Value,
    method: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match recv {
        Value::Str(s) => str_method(s, method, &args, span),
        Value::List(items) => list_method(items, method, args, span),
        // map/set methods need engine access (a struct key's `hash()`), so they live on `Interp`.
        _ => unreachable!("call_method dispatched a non-str/list receiver"),
    }
}

fn str_method(
    s: &Rc<str>,
    method: &str,
    args: &[Value],
    span: Span,
) -> Result<Value, RuntimeError> {
    let str_arg = |i: usize| -> Result<Rc<str>, RuntimeError> {
        match &args[i] {
            Value::Str(a) => Ok(a.clone()),
            other => Err(RuntimeError {
                message: format!(
                    "{method}() expects a str argument, got {}",
                    other.type_name()
                ),
                span,
            }),
        }
    };
    match method {
        "len" => {
            arity("len", args, 0, span)?;
            Ok(Value::Int(s.chars().count() as i64))
        }
        "upper" => {
            arity("upper", args, 0, span)?;
            Ok(Value::Str(s.to_uppercase().into()))
        }
        "lower" => {
            arity("lower", args, 0, span)?;
            Ok(Value::Str(s.to_lowercase().into()))
        }
        "trim" => {
            arity("trim", args, 0, span)?;
            Ok(Value::Str(s.trim().into()))
        }
        // `str` conforms to the `Error` protocol (Go-style): its message is itself.
        "message" => {
            arity("message", args, 0, span)?;
            Ok(Value::Str(s.clone()))
        }
        "split" => {
            arity("split", args, 1, span)?;
            let sep = str_arg(0)?;
            let parts: Vec<Value> = s
                .split(sep.as_ref())
                .map(|p| Value::Str(p.into()))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(parts))))
        }
        "chars" => {
            arity("chars", args, 0, span)?;
            let cs: Vec<Value> = s
                .chars()
                .map(|c| Value::Str(c.to_string().into()))
                .collect();
            Ok(Value::List(Rc::new(RefCell::new(cs))))
        }
        "starts_with" => {
            arity("starts_with", args, 1, span)?;
            Ok(Value::Bool(s.starts_with(str_arg(0)?.as_ref())))
        }
        "contains" => {
            arity("contains", args, 1, span)?;
            Ok(Value::Bool(s.contains(str_arg(0)?.as_ref())))
        }
        // `encode() -> bytes`: UTF-8 encode (str is UTF-8 internally; copy the bytes out into a new
        // immutable `bytes`). Always succeeds — no fault path. UTF-8 only.
        "encode" => {
            arity("encode", args, 0, span)?;
            Ok(Value::Bytes(s.as_bytes().into()))
        }
        "join" => {
            arity("join", args, 1, span)?;
            let Value::List(items) = &args[0] else {
                return Err(RuntimeError {
                    message: format!("join() expects a list of str, got {}", args[0].type_name()),
                    span,
                });
            };
            let mut out = String::new();
            for (i, item) in items.borrow().iter().enumerate() {
                let Value::Str(part) = item else {
                    return Err(RuntimeError {
                        message: format!(
                            "join() expects a list of str, got an element of type {}",
                            item.type_name()
                        ),
                        span,
                    });
                };
                if i > 0 {
                    out.push_str(s);
                }
                out.push_str(part);
            }
            Ok(Value::Str(out.into()))
        }
        // gap #1 (minimal subset): receiver methods forwarding to the `std.str` free fns. Pure
        // native Rust, byte-identical to the std.str codepoint-loop oracle (see std/str.chz).
        "ends_with" => {
            arity("ends_with", args, 1, span)?;
            Ok(Value::Bool(s.ends_with(str_arg(0)?.as_ref())))
        }
        "replace" => {
            arity("replace", args, 2, span)?;
            let old = str_arg(0)?;
            let new = str_arg(1)?;
            // std.str returns `s` unchanged for an empty `old` (Rust would insert `new`
            // between every char) — guard to match the oracle.
            if old.is_empty() {
                Ok(Value::Str(s.clone()))
            } else {
                Ok(Value::Str(s.replace(old.as_ref(), new.as_ref()).into()))
            }
        }
        "repeat" => {
            arity("repeat", args, 1, span)?;
            let n = int_arg(method, &args[0], span)?;
            // std.str: n <= 0 yields "".
            if n <= 0 {
                Ok(Value::Str("".into()))
            } else {
                // Guard the allocation: `str::repeat` hard-panics on capacity overflow.
                // Raise a recoverable fault instead (repo convention for overflow).
                match s
                    .len()
                    .checked_mul(n as usize)
                    .filter(|&t| t <= isize::MAX as usize)
                {
                    Some(_) => Ok(Value::Str(s.repeat(n as usize).into())),
                    None => Err(RuntimeError {
                        message: "string repeat capacity overflow".to_string(),
                        span,
                    }),
                }
            }
        }
        "reverse" => {
            arity("reverse", args, 0, span)?;
            Ok(Value::Str(s.chars().rev().collect::<String>().into()))
        }
        "pad_left" => {
            arity("pad_left", args, 2, span)?;
            let width = int_arg(method, &args[0], span)?;
            let fill = str_arg(1)?;
            // std.str: prepend `fill` until the codepoint length reaches `width`; never shrink.
            let mut out = s.to_string();
            while (out.chars().count() as i64) < width {
                out = format!("{fill}{out}");
            }
            Ok(Value::Str(out.into()))
        }
        "index_of" => {
            arity("index_of", args, 1, span)?;
            let sub = str_arg(0)?;
            // std.str: empty substring is found at 0; otherwise return the CODEPOINT index.
            if sub.is_empty() {
                Ok(Value::Int(0))
            } else {
                match s.find(sub.as_ref()) {
                    Some(byte) => Ok(Value::Int(s[..byte].chars().count() as i64)),
                    None => Ok(Value::Int(-1)),
                }
            }
        }
        "count" => {
            arity("count", args, 1, span)?;
            let sub = str_arg(0)?;
            // std.str: empty substring counts as 0; otherwise non-overlapping count
            // (Rust `matches` is non-overlapping, matching the std.str `i += m` loop).
            if sub.is_empty() {
                Ok(Value::Int(0))
            } else {
                Ok(Value::Int(s.matches(sub.as_ref()).count() as i64))
            }
        }
        "strip_prefix" => {
            arity("strip_prefix", args, 1, span)?;
            let p = str_arg(0)?;
            Ok(Value::Str(s.strip_prefix(p.as_ref()).unwrap_or(s).into()))
        }
        "strip_suffix" => {
            arity("strip_suffix", args, 1, span)?;
            let p = str_arg(0)?;
            Ok(Value::Str(s.strip_suffix(p.as_ref()).unwrap_or(s).into()))
        }
        "split_lines" => {
            arity("split_lines", args, 0, span)?;
            let parts: Vec<Value> = s.split('\n').map(|p| Value::Str(p.into())).collect();
            Ok(Value::List(Rc::new(RefCell::new(parts))))
        }
        // `strip` is a trim alias.
        "strip" => {
            arity("strip", args, 0, span)?;
            Ok(Value::Str(s.trim().into()))
        }
        // gap #7: safe numeric parse — None on bad input (trims like int()/float()).
        "to_int" => {
            arity("to_int", args, 0, span)?;
            Ok(option_value(s.trim().parse::<i64>().ok().map(Value::Int)))
        }
        "to_float" => {
            arity("to_float", args, 0, span)?;
            Ok(option_value(s.trim().parse::<f64>().ok().map(Value::Float)))
        }
        _ => Err(RuntimeError {
            message: format!("type str has no method '{method}'"),
            span,
        }),
    }
}

/// An `int` method-argument, with a uniform type error matching the VM.
fn int_arg(method: &str, v: &Value, span: Span) -> Result<i64, RuntimeError> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(RuntimeError {
            message: format!(
                "{method}() expects an int argument, got {}",
                other.type_name()
            ),
            span,
        }),
    }
}

/// Wrap an optional payload into the native `Option` enum (`Some(v)` / `None`).
fn option_value(v: Option<Value>) -> Value {
    match v {
        Some(v) => Value::Enum {
            ty: "Option".into(),
            variant: "Some".into(),
            payload: vec![v],
        },
        None => Value::Enum {
            ty: "Option".into(),
            variant: "None".into(),
            payload: vec![],
        },
    }
}

/// Total order over scalar values for `sort()`. The checker restricts `sort` to homogeneous
/// int/float/str lists, so only the matching arms are ever hit; anything else compares Equal
/// (a stable no-op) rather than panicking.
fn value_order(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::Equal;
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        _ => Equal,
    }
}

fn list_method(
    items: &Rc<RefCell<Vec<Value>>>,
    method: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match method {
        "len" => {
            arity("len", &args, 0, span)?;
            Ok(Value::Int(items.borrow().len() as i64))
        }
        "push" => {
            arity("push", &args, 1, span)?;
            items.borrow_mut().push(args.into_iter().next().unwrap());
            Ok(Value::Nil)
        }
        "pop" => {
            arity("pop", &args, 0, span)?;
            match items.borrow_mut().pop() {
                Some(v) => Ok(Value::Enum {
                    ty: "Option".into(),
                    variant: "Some".into(),
                    payload: vec![v],
                }),
                None => Ok(Value::Enum {
                    ty: "Option".into(),
                    variant: "None".into(),
                    payload: vec![],
                }),
            }
        }
        "reverse" => {
            arity("reverse", &args, 0, span)?;
            items.borrow_mut().reverse();
            Ok(Value::Nil)
        }
        "sort" => {
            arity("sort", &args, 0, span)?;
            // In place, ascending. The checker guarantees a homogeneous orderable element type
            // (int/float/str); `value_order` falls back to Equal on anything else.
            items.borrow_mut().sort_by(value_order);
            Ok(Value::Nil)
        }
        "contains" => {
            arity("contains", &args, 1, span)?;
            let target = &args[0];
            // `values_equal` (not derived `==`) for numeric/cyclic parity with the VM: it unifies
            // int/float and is depth-guarded, so a cyclic element degrades to "not equal" instead of
            // overflowing the host stack.
            let found = items
                .borrow()
                .iter()
                .any(|v| super::values_equal(v, target));
            Ok(Value::Bool(found))
        }
        "index_of" => {
            arity("index_of", &args, 1, span)?;
            let target = &args[0];
            let idx = items
                .borrow()
                .iter()
                .position(|v| super::values_equal(v, target));
            Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
        }
        "concat" => {
            arity("concat", &args, 1, span)?;
            let other = expect_list_arg("concat", &args[0], span)?;
            let mut out = items.borrow().clone();
            out.extend(other.borrow().iter().cloned());
            Ok(Value::List(Rc::new(RefCell::new(out))))
        }
        "extend" => {
            arity("extend", &args, 1, span)?;
            let other = expect_list_arg("extend", &args[0], span)?;
            // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
            let appended: Vec<Value> = other.borrow().iter().cloned().collect();
            items.borrow_mut().extend(appended);
            Ok(Value::Nil)
        }
        "sum" => {
            arity("sum", &args, 0, span)?;
            let items = items.borrow();
            let any_float = items.iter().any(|v| matches!(v, Value::Float(_)));
            if any_float {
                let mut acc = 0.0_f64;
                for v in items.iter() {
                    match v {
                        Value::Int(n) => acc += *n as f64,
                        Value::Float(f) => acc += *f,
                        other => {
                            return Err(RuntimeError {
                                message: format!(
                                    "sum() expects a numeric list, got an element of type {}",
                                    other.type_name()
                                ),
                                span,
                            });
                        }
                    }
                }
                Ok(Value::Float(acc))
            } else {
                let mut acc = 0_i64;
                for v in items.iter() {
                    match v {
                        Value::Int(n) => {
                            acc = acc.checked_add(*n).ok_or_else(|| RuntimeError {
                                message: "integer overflow in Add".to_string(),
                                span,
                            })?;
                        }
                        other => {
                            return Err(RuntimeError {
                                message: format!(
                                    "sum() expects a numeric list, got an element of type {}",
                                    other.type_name()
                                ),
                                span,
                            });
                        }
                    }
                }
                Ok(Value::Int(acc))
            }
        }
        _ => Err(RuntimeError {
            message: format!("type list has no method '{method}'"),
            span,
        }),
    }
}

/// Unwrap a `list` argument for `concat`/`extend`. The checker guarantees the type, so a non-list
/// here is an internal invariant break — reported as a runtime error for safety.
fn expect_list_arg<'a>(
    method: &str,
    arg: &'a Value,
    span: Span,
) -> Result<&'a Rc<RefCell<Vec<Value>>>, RuntimeError> {
    match arg {
        Value::List(items) => Ok(items),
        other => Err(RuntimeError {
            message: format!(
                "{method}() expects a list argument, got {}",
                other.type_name()
            ),
            span,
        }),
    }
}

pub(super) fn arity(name: &str, args: &[Value], n: usize, span: Span) -> Result<(), RuntimeError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(RuntimeError {
            message: format!("{name}() expects {n} argument(s), got {}", args.len()),
            span,
        })
    }
}

fn range(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    let (start, end, step) = match args {
        [Value::Int(n)] => (0, *n, 1),
        [Value::Int(a), Value::Int(b)] => (*a, *b, 1),
        [Value::Int(a), Value::Int(b), Value::Int(s)] => (*a, *b, *s),
        _ => {
            return Err(RuntimeError {
                message: "range() expects range(end), range(start, end), or range(start, end, step) of ints"
                    .to_string(),
                span,
            });
        }
    };
    let items = crate::slice::range_values(start, end, step)
        .map_err(|message| RuntimeError { message, span })?;
    Ok(Value::List(Rc::new(RefCell::new(
        items.into_iter().map(Value::Int).collect(),
    ))))
}

fn cast_int(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("int", args, 1, span)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Float(f) => {
            // `as i64` saturates silently on overflow and maps NaN to 0 — reject those instead.
            if !f.is_finite() || *f < i64::MIN as f64 || *f >= 9_223_372_036_854_775_808.0 {
                return Err(RuntimeError {
                    message: format!("int(): {f} is out of integer range"),
                    span,
                });
            }
            Ok(Value::Int(*f as i64))
        }
        Value::Bool(b) => Ok(Value::Int(i64::from(*b))),
        Value::Str(s) => s
            .trim()
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| RuntimeError {
                message: format!("int(): cannot parse '{s}' as an integer"),
                span,
            }),
        other => Err(RuntimeError {
            message: format!("int() cannot convert {}", other.type_name()),
            span,
        }),
    }
}

fn cast_float(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("float", args, 1, span)?;
    match &args[0] {
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Int(n) => Ok(Value::Float(*n as f64)),
        Value::Bool(b) => Ok(Value::Float(f64::from(*b))),
        Value::Str(s) => s
            .trim()
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| RuntimeError {
                message: format!("float(): cannot parse '{s}' as a float"),
                span,
            }),
        other => Err(RuntimeError {
            message: format!("float() cannot convert {}", other.type_name()),
            span,
        }),
    }
}

fn cast_str(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("str", args, 1, span)?;
    Ok(Value::Str(args[0].to_string().into()))
}

/// `ord(s)` — the Unicode codepoint of the first (and, by convention, only) character of `s`.
/// Errors on a non-str argument or an empty string. With `s[i]` yielding a 1-char str, this is the
/// char→int bridge (e.g. `ord(c) - ord("0")` for a digit value).
fn ord(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("ord", args, 1, span)?;
    match &args[0] {
        Value::Str(s) => match s.chars().next() {
            Some(c) => Ok(Value::Int(c as i64)),
            None => Err(RuntimeError {
                message: "ord() of an empty string".to_string(),
                span,
            }),
        },
        other => Err(RuntimeError {
            message: format!("ord() expects a str, got {}", other.type_name()),
            span,
        }),
    }
}

/// `chr(n)` — the 1-character str for Unicode codepoint `n`. Errors on a non-int argument or a
/// value that is not a valid codepoint (negative, > 0x10FFFF, or a surrogate).
fn chr(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("chr", args, 1, span)?;
    match &args[0] {
        Value::Int(n) => u32::try_from(*n)
            .ok()
            .and_then(char::from_u32)
            .map(|c| Value::Str(c.to_string().into()))
            .ok_or_else(|| RuntimeError {
                message: format!("chr(): {n} is not a valid Unicode codepoint"),
                span,
            }),
        other => Err(RuntimeError {
            message: format!("chr() expects an int, got {}", other.type_name()),
            span,
        }),
    }
}
