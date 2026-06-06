//! Built-in functions available without any import (`len`, `range`, `int`/`str`/`float`).
//! Math intrinsics like `sqrt` are NOT builtins — they live in `std.math` (M6c).
//! `print` lives in the interpreter itself because it writes to the captured output buffer.

use super::{RuntimeError, Value};
use crate::ast::Span;
use std::cell::RefCell;
use std::rc::Rc;

/// The names handled here. Used so the interpreter can tell a builtin call from a user call.
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "len" | "range" | "int" | "float" | "str" | "ord" | "chr")
}

/// Dispatch a builtin by name. Caller guarantees `is_builtin(name)`.
pub fn call(name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
    match name {
        "len" => len(&args, span),
        "range" => range(&args, span),
        "int" => cast_int(&args, span),
        "float" => cast_float(&args, span),
        "str" => cast_str(&args, span),
        "ord" => ord(&args, span),
        "chr" => chr(&args, span),
        _ => unreachable!("call dispatched a non-builtin: {name}"),
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
        Value::Map(entries) => map_method(entries, method, args, span),
        _ => unreachable!("call_method dispatched a non-str/list/map receiver"),
    }
}

/// Built-in methods on `map[K, V]` (gap #5). Mirrors the VM's `core_method` Map arm and the
/// checker's `map_method_sig` — keep the three in lockstep (error strings included). `get`/`remove`
/// return `Option[V]` via the built-in `Option` enum (last-built-style like `list.pop`).
fn map_method(
    entries: &Rc<RefCell<Vec<(Value, Value)>>>,
    method: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let some = |v: Value| Value::Enum {
        ty: "Option".into(),
        variant: "Some".into(),
        payload: vec![v],
    };
    let none = || Value::Enum {
        ty: "Option".into(),
        variant: "None".into(),
        payload: vec![],
    };
    match method {
        "len" => {
            arity("len", &args, 0, span)?;
            Ok(Value::Int(entries.borrow().len() as i64))
        }
        "has" => {
            arity("has", &args, 1, span)?;
            let key = &args[0];
            let found = entries.borrow().iter().any(|(k, _)| super::values_equal(k, key));
            Ok(Value::Bool(found))
        }
        "get" => {
            arity("get", &args, 1, span)?;
            let key = &args[0];
            let v = entries.borrow().iter().find(|(k, _)| super::values_equal(k, key)).map(|(_, v)| v.clone());
            Ok(v.map(some).unwrap_or_else(none))
        }
        "keys" => {
            arity("keys", &args, 0, span)?;
            let keys: Vec<Value> = entries.borrow().iter().map(|(k, _)| k.clone()).collect();
            Ok(Value::List(Rc::new(RefCell::new(keys))))
        }
        "values" => {
            arity("values", &args, 0, span)?;
            let vals: Vec<Value> = entries.borrow().iter().map(|(_, v)| v.clone()).collect();
            Ok(Value::List(Rc::new(RefCell::new(vals))))
        }
        "remove" => {
            arity("remove", &args, 1, span)?;
            let key = &args[0];
            let pos = entries.borrow().iter().position(|(k, _)| super::values_equal(k, key));
            match pos {
                Some(i) => {
                    let (_, v) = entries.borrow_mut().remove(i);
                    Ok(some(v))
                }
                None => Ok(none()),
            }
        }
        _ => Err(RuntimeError {
            message: format!("type map has no method '{method}'"),
            span,
        }),
    }
}

fn str_method(s: &Rc<str>, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    let str_arg = |i: usize| -> Result<Rc<str>, RuntimeError> {
        match &args[i] {
            Value::Str(a) => Ok(a.clone()),
            other => Err(RuntimeError {
                message: format!("{method}() expects a str argument, got {}", other.type_name()),
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
        "split" => {
            arity("split", args, 1, span)?;
            let sep = str_arg(0)?;
            let parts: Vec<Value> = s.split(sep.as_ref()).map(|p| Value::Str(p.into())).collect();
            Ok(Value::List(Rc::new(RefCell::new(parts))))
        }
        "chars" => {
            arity("chars", args, 0, span)?;
            let cs: Vec<Value> = s.chars().map(|c| Value::Str(c.to_string().into())).collect();
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
        "join" => {
            arity("join", args, 1, span)?;
            let Value::List(items) = &args[0] else {
                return Err(RuntimeError {
                    message: format!(
                        "join() expects a list of str, got {}",
                        args[0].type_name()
                    ),
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
        _ => Err(RuntimeError {
            message: format!("type str has no method '{method}'"),
            span,
        }),
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
            let found = items.borrow().iter().any(|v| v == target);
            Ok(Value::Bool(found))
        }
        "index_of" => {
            arity("index_of", &args, 1, span)?;
            let target = &args[0];
            let idx = items.borrow().iter().position(|v| v == target);
            Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
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
                                message: format!("sum() expects a numeric list, got an element of type {}", other.type_name()),
                                span,
                            })
                        }
                    }
                }
                Ok(Value::Float(acc))
            } else {
                let mut acc = 0_i64;
                for v in items.iter() {
                    match v {
                        Value::Int(n) => acc += *n,
                        other => {
                            return Err(RuntimeError {
                                message: format!("sum() expects a numeric list, got an element of type {}", other.type_name()),
                                span,
                            })
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

fn arity(name: &str, args: &[Value], n: usize, span: Span) -> Result<(), RuntimeError> {
    if args.len() == n {
        Ok(())
    } else {
        Err(RuntimeError {
            message: format!("{name}() expects {n} argument(s), got {}", args.len()),
            span,
        })
    }
}

fn len(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("len", args, 1, span)?;
    match &args[0] {
        Value::List(items) => Ok(Value::Int(items.borrow().len() as i64)),
        Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
        other => Err(RuntimeError {
            message: format!("len() expects a list or str, got {}", other.type_name()),
            span,
        }),
    }
}

/// Upper bound on the length of a list produced by `range()`, to prevent an absurd argument from
/// exhausting memory. (A `for` loop over a range is lazy and not subject to this; this only caps
/// building an actual list.)
const MAX_RANGE_LEN: i64 = 10_000_000;

fn range(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    let (start, end) = match args {
        [Value::Int(n)] => (0, *n),
        [Value::Int(a), Value::Int(b)] => (*a, *b),
        _ => {
            return Err(RuntimeError {
                message: "range() expects range(end) or range(start, end) of ints".to_string(),
                span,
            });
        }
    };
    let len = i128::from(end) - i128::from(start);
    if len > i128::from(MAX_RANGE_LEN) {
        return Err(RuntimeError {
            message: format!("range() length {len} exceeds the maximum of {MAX_RANGE_LEN}"),
            span,
        });
    }
    let items = (start..end).map(Value::Int).collect();
    Ok(Value::List(Rc::new(RefCell::new(items))))
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
        Value::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| RuntimeError {
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
        Value::Str(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| RuntimeError {
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
