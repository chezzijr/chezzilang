//! Built-in functions available without any import (`len`, `range`, `int`/`str`/`float`, `sqrt`).
//! `print` lives in the interpreter itself because it writes to the captured output buffer.

use super::{RuntimeError, Value};
use crate::ast::Span;
use std::cell::RefCell;
use std::rc::Rc;

/// The names handled here. Used so the interpreter can tell a builtin call from a user call.
pub fn is_builtin(name: &str) -> bool {
    matches!(name, "len" | "range" | "int" | "float" | "str" | "sqrt")
}

/// Dispatch a builtin by name. Caller guarantees `is_builtin(name)`.
pub fn call(name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
    match name {
        "len" => len(&args, span),
        "range" => range(&args, span),
        "int" => cast_int(&args, span),
        "float" => cast_float(&args, span),
        "str" => cast_str(&args, span),
        "sqrt" => sqrt(&args, span),
        _ => unreachable!("call dispatched a non-builtin: {name}"),
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

fn sqrt(args: &[Value], span: Span) -> Result<Value, RuntimeError> {
    arity("sqrt", args, 1, span)?;
    let x = match &args[0] {
        Value::Int(n) => *n as f64,
        Value::Float(f) => *f,
        other => {
            return Err(RuntimeError {
                message: format!("sqrt() expects a number, got {}", other.type_name()),
                span,
            });
        }
    };
    if x < 0.0 {
        return Err(RuntimeError {
            message: format!("sqrt() of a negative number ({x})"),
            span,
        });
    }
    Ok(Value::Float(x.sqrt()))
}
