//! Shared Python-style format mini-language for `{expr:spec}` string interpolation.
//!
//! ONE source of truth used by BOTH engines (`compiler`/`vm` and `interp`): each engine does
//! only (a) the `:`-split of the interpolation inner text ([`split_spec`]) and (b) classifying its
//! own runtime `Value` into the neutral [`FmtArg`]. Spec parsing ([`parse`]) and rendering
//! ([`apply`]) live here so the VM and the interpreter cannot diverge.
//!
//! Supported mini-language (a coherent subset of Python's): `[[fill]align][sign][0][width][.precision][type]`
//!  - align: `<` left, `>` right, `^` center; an optional `fill` char may precede the align.
//!  - sign: `+` forces a leading `+` on non-negative numbers.
//!  - `0`: zero-pad numerics to `width` (sign kept before the zeros).
//!  - width: minimum field width (decimal). CAPPED at [`MAX_FIELD`] at PARSE time — a pathological
//!    width like `{x:>9999999999}` is rejected before any allocation (the OOM fix).
//!  - precision: `.N` — float decimals; on a string it TRUNCATES to N chars (Python parity).
//!  - type: one of `d f x X b o e %` (numeric); a string takes only fill/align/width/precision.
//!
//! Errors are returned as `String`; each engine maps them to its own error type with the same
//! message, so the VM and interpreter surface byte-identical diagnostics.

/// Hard cap on field width / precision, applied at parse time before any allocation. A spec
/// requesting more is rejected. This is the real fix for the prior OOM (unbounded `repeat`).
pub const MAX_FIELD: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
    Center,
}

/// A parsed format spec. A default (all-`None`/`false`) spec renders the base value with no padding
/// — i.e. `{x:}` behaves like `{x}`.
#[derive(Debug, Clone, PartialEq)]
pub struct FormatSpec {
    pub fill: char,
    pub align: Option<Align>,
    /// Force a leading `+` on non-negative numbers.
    pub sign: bool,
    /// `0` flag — zero-pad numerics to `width` (with the sign kept ahead of the zeros).
    pub zero_pad: bool,
    pub width: usize,
    pub precision: Option<usize>,
    pub ty: Option<char>,
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec {
            fill: ' ',
            align: None,
            sign: false,
            zero_pad: false,
            width: 0,
            precision: None,
            ty: None,
        }
    }
}

/// A neutral, engine-independent view of an evaluated interpolation value. Each engine maps its
/// own `Value` here: scalars to `Int`/`Float`/`Str`, everything else to `Other` (already rendered
/// via that engine's `stringify`, treated like a string for fill/align/width/precision).
#[derive(Debug, Clone, Copy)]
pub enum FmtArg<'a> {
    Int(i64),
    Float(f64),
    Str(&'a str),
    Other(&'a str),
}

/// Split an interpolation's inner text on the FIRST top-level `:` into `(expr, Some(spec))`, or
/// `(expr, None)` if there is no spec. A `:` inside `()[]{}` or inside a `"`/`'` string literal is
/// NOT a separator (so `{m["a:b"]:>5}` splits only at the final colon, and `{m["a:b"]}` not at all).
pub fn split_spec(inner: &str) -> (&str, Option<&str>) {
    let mut depth: i32 = 0;
    let mut in_str: Option<char> = None;
    for (i, c) in inner.char_indices() {
        if let Some(q) = in_str {
            if c == q {
                in_str = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => in_str = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ':' if depth == 0 => {
                return (&inner[..i], Some(&inner[i + 1..]));
            }
            _ => {}
        }
    }
    (inner, None)
}

/// Parse a format spec (the text after the `:`). Width/precision are bounded by [`MAX_FIELD`] at
/// parse time — the digit accumulator bails to an error the instant it would exceed the cap, so the
/// parsed integer itself never grows pathologically and NO allocation occurs.
pub fn parse(spec: &str) -> Result<FormatSpec, String> {
    let mut out = FormatSpec::default();
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;

    // [[fill]align] — an align char at position 0 or 1. If char 1 is an align, char 0 is the fill.
    if chars.len() >= 2 && is_align(chars[1]) {
        out.fill = chars[0];
        out.align = Some(to_align(chars[1]));
        i = 2;
    } else if !chars.is_empty() && is_align(chars[0]) {
        out.align = Some(to_align(chars[0]));
        i = 1;
    }

    // [sign]
    if i < chars.len() && chars[i] == '+' {
        out.sign = true;
        i += 1;
    }

    // [0] zero-pad flag (a leading zero before the width digits).
    if i < chars.len() && chars[i] == '0' {
        out.zero_pad = true;
        i += 1;
    }

    // [width]
    let mut width: usize = 0;
    let mut saw_width = false;
    while i < chars.len() && chars[i].is_ascii_digit() {
        saw_width = true;
        width = bump(width, chars[i], "width")?;
        i += 1;
    }
    if saw_width {
        out.width = width;
    }

    // [.precision]
    if i < chars.len() && chars[i] == '.' {
        i += 1;
        let mut prec: usize = 0;
        let mut saw = false;
        while i < chars.len() && chars[i].is_ascii_digit() {
            saw = true;
            prec = bump(prec, chars[i], "precision")?;
            i += 1;
        }
        if !saw {
            return Err("format spec: '.' must be followed by a precision".to_string());
        }
        out.precision = Some(prec);
    }

    // [type]
    if i < chars.len() {
        let t = chars[i];
        if is_type(t) {
            out.ty = Some(t);
            i += 1;
        } else {
            return Err(format!("format spec: unknown type char '{t}'"));
        }
    }

    if i != chars.len() {
        return Err(format!("format spec: trailing characters in '{spec}'"));
    }
    Ok(out)
}

/// Accumulate one decimal digit into `acc`, rejecting the moment it would exceed [`MAX_FIELD`].
fn bump(acc: usize, c: char, what: &str) -> Result<usize, String> {
    let next = acc
        .saturating_mul(10)
        .saturating_add((c as u8 - b'0') as usize);
    if next > MAX_FIELD {
        return Err(format!(
            "format spec: {what} exceeds maximum {MAX_FIELD}"
        ));
    }
    Ok(next)
}

fn is_align(c: char) -> bool {
    matches!(c, '<' | '>' | '^')
}

fn to_align(c: char) -> Align {
    match c {
        '<' => Align::Left,
        '>' => Align::Right,
        '^' => Align::Center,
        _ => unreachable!(),
    }
}

fn is_type(c: char) -> bool {
    matches!(c, 'd' | 'f' | 'x' | 'X' | 'b' | 'o' | 'e' | '%')
}

/// Render `arg` per `spec` into `out`. Type/precision mismatches (e.g. `{s:d}`, `{s:.2f}`, zero-pad
/// on a non-number) return a descriptive error — these are runtime errors in BOTH engines because
/// they depend on the value's type. All padding is bounded by the already-capped `spec.width`.
pub fn apply(spec: &FormatSpec, arg: FmtArg, out: &mut String) -> Result<(), String> {
    // 1) base render → `body`, plus an optional sign prefix that zero-padding must keep ahead of
    //    the inserted zeros. `is_numeric` gates the `0` flag and align defaulting.
    let (sign_prefix, body, is_numeric) = render_base(spec, arg)?;

    // 2) zero-pad: pad the numeric body with leading zeros to `width` (after the sign).
    if spec.zero_pad && is_numeric && spec.align.is_none() {
        let cur = sign_prefix.chars().count() + body.chars().count();
        if cur < spec.width {
            out.push_str(&sign_prefix);
            for _ in 0..(spec.width - cur) {
                out.push('0');
            }
            out.push_str(&body);
            return Ok(());
        }
        out.push_str(&sign_prefix);
        out.push_str(&body);
        return Ok(());
    }

    // 3) fill/align to width. Default align: numbers right, everything else left.
    let mut full = String::with_capacity(sign_prefix.len() + body.len());
    full.push_str(&sign_prefix);
    full.push_str(&body);
    let len = full.chars().count();
    if len >= spec.width {
        out.push_str(&full);
        return Ok(());
    }
    let pad = spec.width - len;
    let align = spec.align.unwrap_or(if is_numeric { Align::Right } else { Align::Left });
    match align {
        Align::Left => {
            out.push_str(&full);
            push_fill(out, spec.fill, pad);
        }
        Align::Right => {
            push_fill(out, spec.fill, pad);
            out.push_str(&full);
        }
        Align::Center => {
            let left = pad / 2;
            let right = pad - left; // extra on the right (Python parity)
            push_fill(out, spec.fill, left);
            out.push_str(&full);
            push_fill(out, spec.fill, right);
        }
    }
    Ok(())
}

/// `n` copies of `fill`. `n <= MAX_FIELD` always (width is capped at parse time), so this never
/// allocates pathologically.
fn push_fill(out: &mut String, fill: char, n: usize) {
    for _ in 0..n {
        out.push(fill);
    }
}

/// Produce `(sign_prefix, body, is_numeric)` for the value under the spec's type/precision rules.
/// `sign_prefix` is `"-"`/`"+"`/`""` split off the front so zero-padding can insert zeros after it.
fn render_base(spec: &FormatSpec, arg: FmtArg) -> Result<(String, String, bool), String> {
    match arg {
        FmtArg::Int(n) => render_int(spec, n),
        FmtArg::Float(x) => render_float(spec, x),
        FmtArg::Str(s) => render_str(spec, s),
        FmtArg::Other(s) => render_str(spec, s),
    }
}

fn render_int(spec: &FormatSpec, n: i64) -> Result<(String, String, bool), String> {
    // Precision on an integer is meaningful only via a float type char.
    if spec.precision.is_some() && !matches!(spec.ty, Some('f') | Some('e') | Some('%')) {
        return Err("format spec: precision not allowed on an integer".to_string());
    }
    let neg = n < 0;
    let mag = (n as i128).unsigned_abs(); // safe for i64::MIN
    let body = match spec.ty {
        None | Some('d') => format!("{mag}"),
        Some('x') => format!("{mag:x}"),
        Some('X') => format!("{mag:X}"),
        Some('b') => format!("{mag:b}"),
        Some('o') => format!("{mag:o}"),
        // Float type chars promote the int to a float.
        Some('f') => return render_float(spec, n as f64),
        Some('e') => return render_float(spec, n as f64),
        Some('%') => return render_float(spec, n as f64),
        Some(t) => return Err(format!("format spec: type '{t}' not valid for an integer")),
    };
    Ok((sign_prefix(neg, spec.sign), body, true))
}

fn render_float(spec: &FormatSpec, x: f64) -> Result<(String, String, bool), String> {
    let neg = x.is_sign_negative() && (x != 0.0 || x.is_sign_negative());
    let mag = x.abs();
    let body = match spec.ty {
        Some('f') | None => match spec.precision {
            Some(p) => format!("{mag:.*}", p),
            // Bare `{f:>10}` etc. with no type/precision keeps full float repr.
            None => format_float_like(mag),
        },
        Some('e') => match spec.precision {
            Some(p) => format!("{mag:.*e}", p),
            None => format!("{mag:e}"),
        },
        Some('%') => {
            let scaled = mag * 100.0;
            let p = spec.precision.unwrap_or(6);
            format!("{scaled:.*}%", p)
        }
        Some('d') => return Err("format spec: type 'd' not valid for a float".to_string()),
        Some(t) => return Err(format!("format spec: type '{t}' not valid for a float")),
    };
    Ok((sign_prefix(neg, spec.sign), body, true))
}

fn render_str(spec: &FormatSpec, s: &str) -> Result<(String, String, bool), String> {
    if spec.sign {
        return Err("format spec: sign '+' not allowed on a string".to_string());
    }
    if spec.zero_pad {
        return Err("format spec: zero-pad '0' not allowed on a string".to_string());
    }
    if let Some(t) = spec.ty {
        return Err(format!("format spec: type '{t}' not valid for a string"));
    }
    // `.N` truncates a string to N chars (Python parity).
    let body = match spec.precision {
        Some(p) => s.chars().take(p).collect(),
        None => s.to_string(),
    };
    Ok((String::new(), body, false))
}

fn sign_prefix(neg: bool, force_plus: bool) -> String {
    if neg {
        "-".to_string()
    } else if force_plus {
        "+".to_string()
    } else {
        String::new()
    }
}

/// Mirror the engines' canonical float rendering (`vm::format_float` / `interp::value::format_float`:
/// a finite whole-valued float prints with one decimal, e.g. `5.0`) for a bare `{f:>10}` with no
/// type char. `x` is the already-non-negative magnitude; the sign is handled by the caller.
fn format_float_like(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_apply(spec: &str, arg: FmtArg) -> String {
        let fs = parse(spec).expect("parse ok");
        let mut out = String::new();
        apply(&fs, arg, &mut out).expect("apply ok");
        out
    }

    #[test]
    fn parse_basic() {
        let s = parse(">10").unwrap();
        assert_eq!(s.align, Some(Align::Right));
        assert_eq!(s.width, 10);

        let s = parse("*^8").unwrap();
        assert_eq!(s.fill, '*');
        assert_eq!(s.align, Some(Align::Center));
        assert_eq!(s.width, 8);

        let s = parse("04d").unwrap();
        assert!(s.zero_pad);
        assert_eq!(s.width, 4);
        assert_eq!(s.ty, Some('d'));

        let s = parse(".2f").unwrap();
        assert_eq!(s.precision, Some(2));
        assert_eq!(s.ty, Some('f'));

        let s = parse("+d").unwrap();
        assert!(s.sign);
        assert_eq!(s.ty, Some('d'));

        // Empty spec → default (acts like no spec).
        assert_eq!(parse("").unwrap(), FormatSpec::default());
    }

    #[test]
    fn parse_width_cap_rejected() {
        assert!(parse(">100000000").unwrap_err().contains("exceeds maximum 4096"));
        assert!(parse(">4096").is_ok());
        assert!(parse(">4097").unwrap_err().contains("exceeds maximum 4096"));
        assert!(parse(".99999999").unwrap_err().contains("exceeds maximum 4096"));
    }

    #[test]
    fn parse_malformed() {
        assert!(parse(">10q").is_err()); // unknown type
        assert!(parse("@").is_err()); // bogus
        assert!(parse(".").is_err()); // dot with no precision
    }

    #[test]
    fn apply_align_fill_width() {
        assert_eq!(ok_apply(">10", FmtArg::Str("hi")), "        hi");
        assert_eq!(ok_apply("<10", FmtArg::Str("hi")), "hi        ");
        assert_eq!(ok_apply("^6", FmtArg::Str("hi")), "  hi  ");
        assert_eq!(ok_apply("*>5", FmtArg::Str("ab")), "***ab");
        // odd remainder: extra on the right
        assert_eq!(ok_apply("^7", FmtArg::Str("hi")), "  hi   ");
    }

    #[test]
    fn apply_zero_pad() {
        assert_eq!(ok_apply("05", FmtArg::Int(42)), "00042");
        assert_eq!(ok_apply("05", FmtArg::Int(-7)), "-0007"); // sign before zeros
        assert_eq!(ok_apply("04d", FmtArg::Int(7)), "0007");
        assert_eq!(ok_apply("04x", FmtArg::Int(255)), "00ff");
    }

    #[test]
    fn apply_types() {
        assert_eq!(ok_apply("x", FmtArg::Int(255)), "ff");
        assert_eq!(ok_apply("X", FmtArg::Int(255)), "FF");
        assert_eq!(ok_apply("b", FmtArg::Int(255)), "11111111");
        assert_eq!(ok_apply("o", FmtArg::Int(255)), "377");
        assert_eq!(ok_apply(".2f", FmtArg::Float(3.14559)), "3.15");
        assert_eq!(ok_apply(".1%", FmtArg::Float(0.1357)), "13.6%");
        assert_eq!(ok_apply("+d", FmtArg::Int(5)), "+5");
        assert_eq!(ok_apply("+d", FmtArg::Int(-5)), "-5");
        assert_eq!(ok_apply("e", FmtArg::Float(2.5)), "2.5e0");
        // string precision truncates
        assert_eq!(ok_apply(".3", FmtArg::Str("hello")), "hel");
        // bare float keeps `.0`
        assert_eq!(ok_apply("", FmtArg::Float(5.0)), "5.0");
        assert_eq!(ok_apply(".2f", FmtArg::Float(5.0)), "5.00");
        // int promoted by float type char
        assert_eq!(ok_apply(".2f", FmtArg::Int(3)), "3.00");
    }

    #[test]
    fn apply_type_mismatch() {
        let bad = |spec: &str, arg: FmtArg| {
            let fs = parse(spec).unwrap();
            let mut out = String::new();
            apply(&fs, arg, &mut out).is_err()
        };
        assert!(bad(".2f", FmtArg::Str("hi")));
        assert!(bad("x", FmtArg::Str("hi")));
        assert!(bad("d", FmtArg::Float(1.5)));
        assert!(bad("05", FmtArg::Str("hi"))); // zero-pad on string
        assert!(bad("+", FmtArg::Str("hi"))); // sign on string
        assert!(bad(".2", FmtArg::Int(3))); // precision on plain int
    }

    #[test]
    fn split_spec_edge_cases() {
        assert_eq!(split_spec("x"), ("x", None));
        assert_eq!(split_spec("x:>5"), ("x", Some(">5")));
        assert_eq!(split_spec("m[\"a:b\"]"), ("m[\"a:b\"]", None));
        assert_eq!(split_spec("m[\"a:b\"]:>8"), ("m[\"a:b\"]", Some(">8")));
        assert_eq!(split_spec("a[1:2]"), ("a[1:2]", None)); // slice colon is inside brackets
        assert_eq!(split_spec("f(x):.2f"), ("f(x)", Some(".2f")));
    }
}
