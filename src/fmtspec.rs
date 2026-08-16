//! Shared Python-style format mini-language for `{expr:spec}` string interpolation.
//!
//! ONE source of truth used by the VM (`compiler`/`vm`): it does
//! only (a) the `:`-split of the interpolation inner text ([`split_spec`]) and (b) classifying its
//! own runtime `Value` into the neutral [`FmtArg`]. Spec parsing ([`parse`]) and rendering
//! ([`apply`]) live here so there is a single source of truth.
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
//! Errors are returned as `String`; the caller maps them to its own error type with the same
//! message, so both VM schedulers surface byte-identical diagnostics.

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
/// True when `inner` opens with Chezzi's ternary `if cond: a else: b`, whose top-level colons are
/// STRUCTURAL rather than format-spec separators. Shared with the interpolation scanner
/// (`crate::interpolation`), which must make the same call to know whether a top-level `:` starts
/// the literal spec text — one rule, one place, or the two layers disagree about where the
/// expression ends.
pub(crate) fn is_ternary_head(inner: &str) -> bool {
    inner
        .trim_start()
        .strip_prefix("if")
        .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '('))
}

pub fn split_spec(inner: &str) -> (&str, Option<&str>) {
    // Chezzi's ternary `if cond: a else: b` is an expression whose top-level colons are structural,
    // NOT format-spec separators. A bare top-level ternary therefore carries no spec — splitting on
    // its first colon would corrupt the expression. To attach a spec to a ternary, parenthesize it
    // (`{(if b: 1 else: 2):>5}`), which pushes the inner colons to depth > 0 so only the trailing
    // top-level colon splits.
    if is_ternary_head(inner) {
        return (inner, None);
    }
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
        return Err(format!("format spec: {what} exceeds maximum {MAX_FIELD}"));
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
    matches!(c, 'd' | 'f' | 'x' | 'X' | 'b' | 'o' | 'e' | 'E' | '%')
}

/// Render `arg` per `spec` into `out`. Type/precision mismatches (e.g. `{s:d}`, `{s:.2f}`, zero-pad
/// on a non-number) return a descriptive error — these are runtime errors because
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
    let align = spec.align.unwrap_or(if is_numeric {
        Align::Right
    } else {
        Align::Left
    });
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

/// The scalar kind a value renders as for spec-validity purposes. `bool`/`bytes`/anything mapping to
/// [`FmtArg::Other`] folds into `Str`, matching the runtime path (they render via `render_str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarKind {
    Int,
    Float,
    Str,
}

/// Single source of truth for "is this spec's type/precision/sign valid for this scalar kind?".
/// Called FIRST by `render_int`/`render_float`/`render_str` (so the runtime wording never forks) AND
/// by the checker to reject a provably-wrong `{expr:spec}` at COMPILE time for concrete scalars.
/// The error strings are byte-identical to the runtime diagnostics.
pub fn spec_valid_for_scalar(spec: &FormatSpec, kind: ScalarKind) -> Result<(), String> {
    match kind {
        // Precision on an integer is meaningful only via a float type char; every parse-allowed type
        // char (d/x/X/b/o and the f/e/% promoters) is otherwise valid for an int.
        ScalarKind::Int => {
            if spec.precision.is_some()
                && !matches!(spec.ty, Some('f') | Some('e') | Some('E') | Some('%'))
            {
                return Err("format spec: precision not allowed on an integer".to_string());
            }
            Ok(())
        }
        // A float takes f/e/%/(none); the integer/radix type chars are rejected.
        ScalarKind::Float => match spec.ty {
            Some('d') => Err("format spec: type 'd' not valid for a float".to_string()),
            Some(t @ ('x' | 'X' | 'b' | 'o')) => {
                Err(format!("format spec: type '{t}' not valid for a float"))
            }
            _ => Ok(()),
        },
        // A string takes only fill/align/width/precision — no sign, no zero-pad, no type char.
        ScalarKind::Str => {
            if spec.sign {
                return Err("format spec: sign '+' not allowed on a string".to_string());
            }
            if spec.zero_pad {
                return Err("format spec: zero-pad '0' not allowed on a string".to_string());
            }
            if let Some(t) = spec.ty {
                return Err(format!("format spec: type '{t}' not valid for a string"));
            }
            Ok(())
        }
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
    // Validity (precision/type) is single-sourced in `spec_valid_for_scalar`.
    spec_valid_for_scalar(spec, ScalarKind::Int)?;
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
        Some('E') => return render_float(spec, n as f64),
        Some('%') => return render_float(spec, n as f64),
        Some(_) => unreachable!("validity checked by spec_valid_for_scalar"),
    };
    Ok((sign_prefix(neg, spec.sign), body, true))
}

fn render_float(spec: &FormatSpec, x: f64) -> Result<(String, String, bool), String> {
    // Validity (type char) is single-sourced in `spec_valid_for_scalar`.
    spec_valid_for_scalar(spec, ScalarKind::Float)?;
    // NaN: mask the sign so the spec path renders `NaN` (not `-NaN`), matching the bare
    // stringify path (`format_float`) which drops the NaN sign. `-inf`/`-0.0` keep their sign.
    let neg = !x.is_nan() && x.is_sign_negative() && (x != 0.0 || x.is_sign_negative());
    let mag = x.abs();
    let body = match spec.ty {
        Some('f') | None => match spec.precision {
            Some(p) => format!("{mag:.*}", p),
            // Bare `{f:>10}` etc. with no type/precision keeps the canonical Python repr.
            None => repr_float(mag),
        },
        // `{:e}`/`{:E}`: Python default precision 6, exponent always signed + 2-digit padded.
        Some('e') | Some('E') => {
            let p = spec.precision.unwrap_or(6);
            let marker = if spec.ty == Some('E') { 'E' } else { 'e' };
            normalize_exp(&format!("{mag:.*e}", p), marker)
        }
        Some('%') => {
            let scaled = mag * 100.0;
            let p = spec.precision.unwrap_or(6);
            format!("{scaled:.*}%", p)
        }
        Some(_) => unreachable!("validity checked by spec_valid_for_scalar"),
    };
    Ok((sign_prefix(neg, spec.sign), body, true))
}

fn render_str(spec: &FormatSpec, s: &str) -> Result<(String, String, bool), String> {
    // Validity (sign/zero-pad/type char) is single-sourced in `spec_valid_for_scalar`.
    spec_valid_for_scalar(spec, ScalarKind::Str)?;
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

/// Normalize a Rust `{:e}`/`{:.Ne}` exponent string into Python form: `{mant}{marker}{sign}{dd}`
/// where the exponent always carries an explicit sign and is zero-padded to at least 2 digits
/// (`1.5e300` → `1.5e+300`, `-2.5e-8` → `-2.5e-08`, `1e0` → `1e+00`). A non-exponent input
/// (`inf`/`NaN`, which Rust's float formatters emit with no `e`) passes through unchanged. Shared by
/// the `{:e}`/`{:E}` spec arm and the default repr path so the exponent logic lives in one place.
pub(crate) fn normalize_exp(rust_e: &str, marker: char) -> String {
    match rust_e.split_once('e') {
        None => rust_e.to_string(),
        Some((mant, exp)) => {
            // Rust only ever prefixes a '-' (never '+') on the exponent.
            let (sign, digits) = match exp.strip_prefix('-') {
                Some(d) => ('-', d),
                None => ('+', exp),
            };
            format!("{mant}{marker}{sign}{digits:0>2}")
        }
    }
}

/// Canonical Python `repr()`/`str()` of a float — the single source of truth for the bare stringify
/// path (`str(f)`, `print`, `{f}` interpolation with no type char, `json.stringify`). Matches
/// CPython: scientific notation when the decimal exponent is `< -4` or `>= 16`, otherwise fixed with
/// an always-present `.0` on integer-valued floats. Non-finite floats keep Rust's `inf`/`-inf`/`NaN`
/// (the NaN sign is dropped, as before). `x` is signed here; callers that pre-split the sign pass the
/// magnitude.
///
/// **Tie rule (W7-32).** When the two shortest candidate reprs are *exactly* equidistant from `x`
/// (the exact binary value's decimal expansion ends in a `5` one digit past the cut, e.g.
/// `771.54620361328125`), CPython's `repr` (David Gay's `_Py_dg_dtoa`) breaks the tie **to even**
/// while Rust's shortest formatter breaks it **away from zero**. They can therefore only disagree
/// when Rust's last significant digit is **odd**; in that case re-render `x` at the same
/// significant-digit count with Rust's *fixed-precision* formatter, which is exact and already
/// rounds half-to-even. (That is also why the `{:.N}` spec paths never needed this fix.)
/// The re-render is kept only if it still round-trips (`round_trips`): at a binade boundary the
/// even candidate can fall outside `x`'s rounding interval — `2f64.powi(-24)` is exactly
/// `5.9604644775390625e-08`, yet `5.960464477539062e-08` parses to a *different* float, so CPython
/// keeps the odd `…063` there too.
pub(crate) fn repr_float(x: f64) -> String {
    if !x.is_finite() {
        return format!("{x}");
    }
    let e = format!("{x:e}");
    let (mant, exp_s) = e.split_once('e').unwrap_or((e.as_str(), "0"));
    let exp: i32 = exp_s.parse().unwrap_or(0);
    // Number of significant digits Rust's *shortest* repr used, and whether its last one is odd
    // (the only case where CPython's tie-to-even can differ — see the doc comment above).
    let ndigits = mant.bytes().filter(u8::is_ascii_digit).count();
    let odd_last = mant.bytes().next_back().is_some_and(|b| b % 2 == 1);
    // A candidate only replaces Rust's shortest form if it still names the very same float.
    let round_trips = |c: &str| c.parse::<f64>().is_ok_and(|v| v.to_bits() == x.to_bits());
    if !(-4..16).contains(&exp) {
        let even = odd_last
            .then(|| format!("{x:.*e}", ndigits - 1))
            .filter(|c| round_trips(c));
        normalize_exp(&even.unwrap_or(e), 'e')
    } else if x.fract() == 0.0 {
        format!("{x}.0")
    } else {
        // `ndigits` significant digits with the point after digit `exp + 1` → this many after it.
        // Always >= 1 here: `x` has a fractional part, so the shortest repr shows one.
        odd_last
            .then(|| format!("{x:.*}", (ndigits as i32 - 1 - exp) as usize))
            .filter(|c| round_trips(c))
            .unwrap_or_else(|| format!("{x}"))
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
        assert!(
            parse(">100000000")
                .unwrap_err()
                .contains("exceeds maximum 4096")
        );
        assert!(parse(">4096").is_ok());
        assert!(parse(">4097").unwrap_err().contains("exceeds maximum 4096"));
        assert!(
            parse(".99999999")
                .unwrap_err()
                .contains("exceeds maximum 4096")
        );
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
        assert_eq!(ok_apply("e", FmtArg::Float(2.5)), "2.500000e+00");
        // string precision truncates
        assert_eq!(ok_apply(".3", FmtArg::Str("hello")), "hel");
        // bare float keeps `.0`
        assert_eq!(ok_apply("", FmtArg::Float(5.0)), "5.0");
        assert_eq!(ok_apply(".2f", FmtArg::Float(5.0)), "5.00");
        // int promoted by float type char
        assert_eq!(ok_apply(".2f", FmtArg::Int(3)), "3.00");
    }

    #[test]
    fn python_float_repr_and_e_spec() {
        // Bare repr (str/print/interp-no-spec/json path) — CPython repr() differential table.
        let repr_cases: &[(f64, &str)] = &[
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.5e300, "1.5e+300"),
            (1.0, "1.0"),
            (1e100, "1e+100"),
            (-2.5e-8, "-2.5e-08"),
            (0.0, "0.0"),
            (123.5, "123.5"),
            // --- W7-32: exact-tie shortest reprs. Every expected value below is a real
            // `python3 -c "print(repr(...))"` run (CPython 3.14.6), not hand-derived.
            // Rust's shortest formatter breaks these away from zero (…813 / …651.3 / …812.3 /
            // …313e-08); CPython breaks them to even.
            (771.5462036132812, "771.5462036132812"), // exact 771.54620361328125
            (1007730844620651.2, "1007730844620651.2"), // exact 1007730844620651.25
            // negative: the rule is derived on the MAGNITUDE's digits. Spelled as an exact sum
            // because clippy::excessive_precision rejects the literal `-887777373534812.25` and
            // "helpfully" suggests `-887777373534812.3` — which is the away-from-zero rendering
            // this row exists to reject. Both terms and the sum are exactly representable.
            (-(887777373534812.0 + 0.25), "-887777373534812.2"),
            (2.9802322387695312e-08, "2.9802322387695312e-08"), // 2^-25, scientific branch
            // Near-ties that must NOT be adjusted: the last significant digit is odd, and the
            // decremented neighbour either round-trips but is FARTHER (5e-324, whose exact value
            // is 4.94…e-324 — decrementing here would be a new bug) or does not round-trip (0.1).
            (5e-324, "5e-324"),
            (0.1, "0.1"),
            // A tie whose EVEN candidate does not round-trip (binade boundary): `2^-24` is exactly
            // `5.9604644775390625e-08`, but `5.960464477539062e-08` parses to a different float,
            // so CPython keeps the odd `…063` — the round-trip guard must not "fix" this one.
            (5.960464477539063e-8, "5.960464477539063e-08"),
        ];
        for (x, want) in repr_cases {
            assert_eq!(repr_float(*x), *want, "repr_float({x})");
        }
        // `{:e}`/`{:E}` spec — Python default precision 6, signed 2-digit exponent.
        assert_eq!(ok_apply("e", FmtArg::Float(123456.789)), "1.234568e+05");
        assert_eq!(ok_apply("e", FmtArg::Float(1.0)), "1.000000e+00");
        assert_eq!(ok_apply(".2e", FmtArg::Float(0.000123)), "1.23e-04");
        assert_eq!(ok_apply("E", FmtArg::Float(123456.789)), "1.234568E+05");
    }

    #[test]
    fn nan_sign_masked_in_format_spec() {
        // A negative-signed quiet NaN (e.g. from 0.0/0.0) must render `NaN`, not `-NaN`,
        // to match the bare stringify path which masks the NaN sign.
        let nn = f64::from_bits(0xFFF8000000000000); // negative qNaN
        assert!(nn.is_sign_negative() && nn.is_nan());
        assert_eq!(ok_apply(".2f", FmtArg::Float(nn)), "NaN");
        assert_eq!(ok_apply("f", FmtArg::Float(nn)), "NaN");
        assert_eq!(ok_apply("e", FmtArg::Float(nn)), "NaN");
        // Positive NaN unchanged.
        assert_eq!(ok_apply(".2f", FmtArg::Float(f64::NAN)), "NaN");
        // Infinities keep their sign.
        assert_eq!(ok_apply(".2f", FmtArg::Float(f64::NEG_INFINITY)), "-inf");
        assert_eq!(ok_apply(".2f", FmtArg::Float(f64::INFINITY)), "inf");
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
    fn spec_valid_for_scalar_predicate() {
        let p = |s: &str| parse(s).unwrap();
        // invalid combos — messages must match the runtime wording verbatim
        assert!(
            spec_valid_for_scalar(&p(".2f"), ScalarKind::Str)
                .unwrap_err()
                .contains("type 'f' not valid for a string")
        );
        assert!(
            spec_valid_for_scalar(&p("d"), ScalarKind::Float)
                .unwrap_err()
                .contains("type 'd' not valid for a float")
        );
        assert!(
            spec_valid_for_scalar(&p(".3d"), ScalarKind::Int)
                .unwrap_err()
                .contains("precision not allowed on an integer")
        );
        assert!(
            spec_valid_for_scalar(&p("x"), ScalarKind::Float)
                .unwrap_err()
                .contains("type 'x' not valid for a float")
        );
        assert!(
            spec_valid_for_scalar(&p("+"), ScalarKind::Str)
                .unwrap_err()
                .contains("sign '+' not allowed on a string")
        );
        // valid combos
        assert!(spec_valid_for_scalar(&p(".2f"), ScalarKind::Float).is_ok());
        assert!(spec_valid_for_scalar(&p("d"), ScalarKind::Int).is_ok());
        assert!(spec_valid_for_scalar(&p(".3"), ScalarKind::Str).is_ok());
        assert!(spec_valid_for_scalar(&p("x"), ScalarKind::Int).is_ok());
        assert!(spec_valid_for_scalar(&p(".2f"), ScalarKind::Int).is_ok()); // int promoted by float type
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

    #[test]
    fn split_spec_bare_ternary_has_no_spec() {
        // Chezzi's ternary `if cond: a else: b` is an expression whose top-level colons are NOT
        // format-spec separators. A bare top-level ternary therefore carries no spec (regression
        // guard: `{if b: 10 else: 20}` must keep working, not be mis-split into expr `if b`).
        assert_eq!(split_spec("if b: 10 else: 20"), ("if b: 10 else: 20", None));
        assert_eq!(
            split_spec("if x > 0: a else: b"),
            ("if x > 0: a else: b", None)
        );
        // A parenthesized ternary CAN carry a spec — the inner colons are bracketed (depth > 0),
        // so only the trailing top-level colon splits.
        assert_eq!(
            split_spec("(if b: 1 else: 2):>5"),
            ("(if b: 1 else: 2)", Some(">5"))
        );
        // `if` as a leading substring of an identifier is not the keyword — still splits normally.
        assert_eq!(split_spec("iffy:>5"), ("iffy", Some(">5")));
    }
}
