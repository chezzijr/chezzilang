//! Shared Python-style format mini-language for `{expr:spec}` string interpolation.
//!
//! ONE source of truth used by the VM (`compiler`/`vm`): it classifies its own runtime `Value` into
//! the neutral [`FmtArg`]. Spec parsing ([`parse`]) and rendering ([`apply`]) live here so there is
//! a single source of truth. Where a spec starts and ends is the lexer's business, not this module's
//! (`Lexer::hole`).
//!
//! Supported mini-language (a coherent subset of Python's): `[[fill]align][sign][z][#][0][width][grouping][.precision][type]`
//!  - align: `<` left, `>` right, `^` center; an optional `fill` char may precede the align.
//!  - sign: `+` forces a leading `+` on non-negative numbers.
//!  - `z`: a float that rounds to zero prints without its minus sign (`{-0.0001:z.1f}` is `0.0`;
//!    CPython 3.11+). Rejected on an integer type and on a string, as CPython does.
//!  - `0`: zero-pad numerics to `width` (sign kept before the zeros).
//!  - width: minimum field width (decimal), or a nested `{expr}` field (`{s:<{w}}`). CAPPED at
//!    [`MAX_FIELD`]: a literal at PARSE time — a pathological width like `{x:>9999999999}` is
//!    rejected before any allocation (the OOM fix) — and a nested field's VALUE in
//!    [`field_from_int`], before any buffer is sized. A negative nested value is an error too.
//!  - precision: `.N` or `.{expr}` — float decimals; on a string it TRUNCATES to N chars (Python
//!    parity). Same cap. A nested field must be an `int` and is legal ONLY as the width or the
//!    precision (CPython also allows one in the fill/align/type slots; Chezzi checks a spec's
//!    shape at `check` time, which a runtime-built spec would defeat).
//!  - type: one of `d f x X b o e %` (numeric); a string takes only fill/align/width/precision.
//!
//! Errors are returned as `String`; the caller maps them to its own error type with the same
//! message, so both VM schedulers surface byte-identical diagnostics.

/// Hard cap on field width / precision, applied at parse time before any allocation. A spec
/// requesting more is rejected. This is the real fix for the prior OOM (unbounded `repeat`).
pub const MAX_FIELD: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub enum Align {
    Left,
    Right,
    Center,
    /// `=` sign-aware fill: pad between the sign and the digits.
    Pad,
}

/// A parsed format spec. A default (all-`None`/`false`) spec renders the base value with no padding
/// — i.e. `{x:}` behaves like `{x}`.
#[derive(Debug, Clone, PartialEq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct FormatSpec {
    #[borsh(serialize_with = "ser_char", deserialize_with = "de_char")]
    pub fill: char,
    pub align: Option<Align>,
    /// The sign char written before the digits: `'+'`, `'-'` or `' '`. `None` means the default
    /// (a leading `-` on negatives only, nothing on non-negatives).
    #[borsh(serialize_with = "ser_opt_char", deserialize_with = "de_opt_char")]
    pub sign: Option<char>,
    /// `z` — coerce a negative zero (after rounding to the precision) to positive zero.
    pub zero_coerce: bool,
    /// `#` alternate form — a radix prefix on `x`/`X`/`o`/`b`, a forced decimal point on a float.
    pub alt: bool,
    /// `0` flag — zero-pad numerics to `width` (with the sign kept ahead of the zeros).
    pub zero_pad: bool,
    pub width: usize,
    /// The width is a nested `{expr}` field (`{s:<{w}}`): `width` stays 0 and the VM fills it from
    /// the field's value ([`field_from_int`]) before rendering.
    pub dyn_width: bool,
    /// The precision is a nested `{expr}` field (`{x:.{p}f}`); `precision` is `Some(0)` as a
    /// placeholder so the presence checks in [`spec_valid_for_scalar`] still fire.
    pub dyn_precision: bool,
    /// `,` or `_` digit-group separator (CPython slot order: `[width][grouping][.precision][type]`).
    #[borsh(serialize_with = "ser_opt_char", deserialize_with = "de_opt_char")]
    pub group: Option<char>,
    pub precision: Option<usize>,
    #[borsh(serialize_with = "ser_opt_char", deserialize_with = "de_opt_char")]
    pub ty: Option<char>,
}

// `borsh` has no `char` encoding: a `char` travels as its `u32` scalar value.
fn ser_char<W: std::io::Write>(c: &char, w: &mut W) -> std::io::Result<()> {
    borsh::BorshSerialize::serialize(&u32::from(*c), w)
}

fn de_char<R: std::io::Read>(r: &mut R) -> std::io::Result<char> {
    let n = <u32 as borsh::BorshDeserialize>::deserialize_reader(r)?;
    char::from_u32(n).ok_or_else(not_a_char)
}

fn ser_opt_char<W: std::io::Write>(c: &Option<char>, w: &mut W) -> std::io::Result<()> {
    borsh::BorshSerialize::serialize(&c.map(u32::from), w)
}

fn de_opt_char<R: std::io::Read>(r: &mut R) -> std::io::Result<Option<char>> {
    match <Option<u32> as borsh::BorshDeserialize>::deserialize_reader(r)? {
        Some(n) => char::from_u32(n).map(Some).ok_or_else(not_a_char),
        None => Ok(None),
    }
}

fn not_a_char() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "not a char")
}

impl Default for FormatSpec {
    fn default() -> Self {
        FormatSpec {
            fill: ' ',
            align: None,
            sign: None,
            zero_coerce: false,
            alt: false,
            zero_pad: false,
            width: 0,
            dyn_width: false,
            dyn_precision: false,
            group: None,
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

/// Parse a format spec (the text after the `:`). Width/precision are bounded by [`MAX_FIELD`] at
/// parse time — the digit accumulator bails to an error the instant it would exceed the cap, so the
/// parsed integer itself never grows pathologically and NO allocation occurs. A nested `{expr}`
/// field is bounded later, when its value is known ([`field_from_int`]).
pub fn parse(spec: &str) -> Result<FormatSpec, String> {
    let items: Vec<SpecItem> = spec.chars().map(SpecItem::Ch).collect();
    parse_nested(&items)
}

/// One element of a format spec as the lexer found it: a char of spec text, or a nested `{expr}`
/// field. The lexer decides where a field starts and ends (`Lexer::hole`), so this parser never
/// scans for a brace: a `{` that is a `Ch` is a fill char, nothing more.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpecItem {
    Ch(char),
    Field,
}

const NESTED_SLOTS: &str =
    "format spec: a nested field is allowed only as the width or the precision";

/// [`parse`] over the lexer's items. A [`SpecItem::Field`] sets `dyn_width` in the width slot and
/// `dyn_precision` in the precision slot; anywhere else it is an error. The caller reads the fields
/// out of its own copy of the items, in the same order (width before precision).
pub fn parse_nested(items: &[SpecItem]) -> Result<FormatSpec, String> {
    let mut out = FormatSpec::default();
    // Index just past the most recent nested field.
    let mut field_end: Option<usize> = None;
    // The char at `i`, `None` for a field or past the end.
    let ch = |i: usize| match items.get(i) {
        Some(SpecItem::Ch(c)) => Some(*c),
        Some(SpecItem::Field) | None => None,
    };
    let is_field = |i: usize| items.get(i) == Some(&SpecItem::Field);
    let mut i = 0;
    let mut fill_explicit = false;

    // [[fill]align] — an align char at position 0 or 1. If char 1 is an align, char 0 is the fill.
    match (ch(0), ch(1)) {
        (Some(fill), Some(align)) if is_align(align) => {
            out.fill = fill;
            fill_explicit = true;
            out.align = Some(to_align(align));
            i = 2;
        }
        (Some(align), _) if is_align(align) => {
            out.align = Some(to_align(align));
            i = 1;
        }
        _ => {}
    }

    // [sign]
    if let Some(sign) = ch(i).filter(|c| matches!(c, '+' | '-' | ' ')) {
        out.sign = Some(sign);
        i += 1;
    }

    // [z] negative-zero coercion (CPython 3.11; its slot is after the sign, before `#`).
    if ch(i) == Some('z') {
        out.zero_coerce = true;
        i += 1;
    }

    // [#] alternate form.
    if ch(i) == Some('#') {
        out.alt = true;
        i += 1;
    }

    // [0] zero-pad flag (a leading zero before the width digits). The `0` flag sets the fill to
    // `'0'` unless an explicit fill was already written, under EVERY align — matching CPython's
    // `parse_internal_render_format_spec`. It defaults the align to `=` only when no align was
    // written at all; `apply` encodes that default itself (`align.is_none()`) rather than `parse`
    // mutating `out.align` here, because a string's zero-pad reject must still report
    // `zero-pad '0' not allowed on a string`, not `'=' alignment not allowed on a string`.
    if ch(i) == Some('0') {
        out.zero_pad = true;
        if !fill_explicit {
            out.fill = '0';
        }
        i += 1;
    }

    // [width]
    let mut width: usize = 0;
    let mut saw_width = false;
    if is_field(i) {
        out.dyn_width = true;
        i += 1;
        field_end = Some(i);
    } else {
        while let Some(digit) = ch(i).filter(char::is_ascii_digit) {
            saw_width = true;
            width = bump(width, digit, "width")?;
            i += 1;
        }
    }
    if saw_width {
        out.width = width;
    }

    // [grouping] — `,` or `_`, CPython's slot between width and precision.
    if let Some(group) = ch(i).filter(|c| matches!(c, ',' | '_')) {
        out.group = Some(group);
        i += 1;
    }

    // [.precision]
    if ch(i) == Some('.') {
        i += 1;
        let mut prec: usize = 0;
        let mut saw = false;
        if is_field(i) {
            out.dyn_precision = true;
            i += 1;
            field_end = Some(i);
            saw = true;
        } else {
            while let Some(digit) = ch(i).filter(char::is_ascii_digit) {
                saw = true;
                prec = bump(prec, digit, "precision")?;
                i += 1;
            }
        }
        if !saw {
            return Err("format spec: '.' must be followed by a precision".to_string());
        }
        // A nested field leaves `prec` at 0: a placeholder the VM overwrites.
        out.precision = Some(prec);
    }

    // [type]
    if i < items.len() {
        match ch(i) {
            Some(t) if is_type(t) => {
                out.ty = Some(t);
                i += 1;
            }
            Some(t) if field_end == Some(i) && is_align(t) => {
                return Err(NESTED_SLOTS.to_string());
            }
            Some(t) => return Err(format!("format spec: unknown type char '{t}'")),
            None => return Err(NESTED_SLOTS.to_string()),
        }
    }

    if i != items.len() {
        if items[i..].contains(&SpecItem::Field) {
            return Err(NESTED_SLOTS.to_string());
        }
        let spec: String = items
            .iter()
            .map(|item| match item {
                SpecItem::Ch(c) => c.to_string(),
                SpecItem::Field => "{}".to_string(),
            })
            .collect();
        return Err(format!("format spec: trailing characters in '{spec}'"));
    }
    Ok(out)
}

/// Turn a nested field's runtime value into a width/precision. `None` (not an int) is an error, as
/// is a negative value; a value over [`MAX_FIELD`] is rejected with the same text [`bump`] uses for
/// a literal. The VM calls this BEFORE sizing any buffer, so a huge runtime width is a clean error,
/// never an allocation (CPython would try to build the string).
pub fn field_from_int(n: Option<i64>, what: &str) -> Result<usize, String> {
    match n {
        None => Err(format!("format spec: a nested {what} field must be an int")),
        Some(n) if n < 0 => Err(format!("format spec: {what} must not be negative, got {n}")),
        Some(n) if n as u64 > MAX_FIELD as u64 => {
            Err(format!("format spec: {what} exceeds maximum {MAX_FIELD}"))
        }
        Some(n) => Ok(n as usize),
    }
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
    matches!(c, '<' | '>' | '^' | '=')
}

fn to_align(c: char) -> Align {
    match c {
        '<' => Align::Left,
        '>' => Align::Right,
        '^' => Align::Center,
        '=' => Align::Pad,
        _ => unreachable!(),
    }
}

fn is_type(c: char) -> bool {
    matches!(
        c,
        'd' | 'f' | 'F' | 'x' | 'X' | 'b' | 'o' | 'e' | 'E' | '%' | 'g' | 'G'
    )
}

/// Render `arg` per `spec` into `out`. Type/precision mismatches (e.g. `{s:d}`, `{s:.2f}`, zero-pad
/// on a non-number) return a descriptive error — these are runtime errors because
/// they depend on the value's type. All padding is bounded by the already-capped `spec.width`.
pub fn apply(spec: &FormatSpec, arg: FmtArg, out: &mut String) -> Result<(), String> {
    // 1) base render → `body`, plus an optional sign prefix that zero-padding must keep ahead of
    //    the inserted zeros. `is_numeric` gates the `0` flag and align defaulting.
    let (sign_prefix, mut body, is_numeric) = render_base(spec, arg)?;

    // CPython's (fill `'0'`, align `'='`) pair: zero-fill applies under the bare `0` flag (no
    // explicit align) OR under an explicit `=` align whose fill is `'0'` (`parse` already sets that
    // fill from the `0` flag when no fill was written explicitly).
    let zero_fill = is_numeric
        && ((spec.zero_pad && spec.align.is_none())
            || (spec.align == Some(Align::Pad) && spec.fill == '0'));

    // 1b) grouping (`,`/`_`): insert a separator every `size` digits of the LEADING digit run —
    // hex under `x`/`X` (so `format(1048575,'_x')` groups `f_ffff`), decimal otherwise (`b`/`o`
    // bodies are already digits). An empty run (`NaN`, `inf`) is left untouched. Zero-pad plus
    // grouping may OVERSHOOT `width` on purpose (CPython: `{1000:08,}` is `0,001,000`, nine chars
    // for a width of eight) — the digit count grows until it covers the requested width, it is
    // never trimmed back down.
    if let Some(sep) = spec.group {
        let size = if matches!(spec.ty, Some('x') | Some('X') | Some('b') | Some('o')) {
            4
        } else {
            3
        };
        let is_digit: fn(char) -> bool = if matches!(spec.ty, Some('x') | Some('X')) {
            |c| c.is_ascii_hexdigit()
        } else {
            |c| c.is_ascii_digit()
        };
        let digit_end = body.find(|c| !is_digit(c)).unwrap_or(body.len());
        let (digits, tail) = body.split_at(digit_end);
        if !digits.is_empty() {
            let mut want = digits.chars().count();
            if zero_fill {
                let target = spec
                    .width
                    .saturating_sub(sign_prefix.chars().count() + tail.chars().count());
                while want + want.saturating_sub(1) / size < target {
                    want += 1;
                }
            }
            body = group_digits(digits, want, sep, size) + tail;
        }
    }

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
        Align::Pad => {
            out.push_str(&sign_prefix);
            push_fill(out, spec.fill, pad);
            out.push_str(&body);
        }
    }
    Ok(())
}

/// Insert `sep` every `size` digits from the right of `digits`, left-padding with `'0'` first so
/// the result has at least `want` digits. `want` never exceeds the parse-capped `MAX_FIELD`, so
/// the allocation stays bounded.
fn group_digits(digits: &str, want: usize, sep: char, size: usize) -> String {
    let n = digits.len().max(want);
    let pad = n - digits.len();
    let digit_bytes = digits.as_bytes();
    let mut out = String::with_capacity(n + n / size);
    for k in 0..n {
        if k > 0 && (n - k).is_multiple_of(size) {
            out.push(sep);
        }
        if k < pad {
            out.push('0');
        } else {
            out.push(digit_bytes[k - pad] as char);
        }
    }
    out
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
        // char (d/x/X/b/o and the f/e/%/g/G promoters) is otherwise valid for an int.
        ScalarKind::Int => {
            let float_ty = matches!(
                spec.ty,
                Some('f') | Some('F') | Some('e') | Some('E') | Some('%') | Some('g') | Some('G')
            );
            if spec.zero_coerce && !float_ty {
                return Err(
                    "format spec: negative-zero coercion 'z' not allowed on an integer".to_string(),
                );
            }
            if spec.precision.is_some()
                && !matches!(
                    spec.ty,
                    Some('f')
                        | Some('F')
                        | Some('e')
                        | Some('E')
                        | Some('%')
                        | Some('g')
                        | Some('G')
                )
            {
                return Err("format spec: precision not allowed on an integer".to_string());
            }
            // `,` groups in threes and clashes with radix output; `_` groups `x`/`X`/`b`/`o` in
            // fours instead (CPython: `format(1048575,'_x')` is `f_ffff`), so only `,` is rejected.
            if spec.group == Some(',')
                && matches!(spec.ty, Some(t) if matches!(t, 'x' | 'X' | 'b' | 'o'))
            {
                let t = spec.ty.unwrap();
                return Err(format!(
                    "format spec: grouping ',' not valid with type '{t}'"
                ));
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
            if spec.zero_coerce {
                return Err(
                    "format spec: negative-zero coercion 'z' not allowed on a string".to_string(),
                );
            }
            if let Some(c) = spec.sign {
                return Err(format!("format spec: sign '{c}' not allowed on a string"));
            }
            if spec.alt {
                return Err("format spec: alternate form '#' not allowed on a string".to_string());
            }
            if spec.align == Some(Align::Pad) {
                return Err("format spec: '=' alignment not allowed on a string".to_string());
            }
            if spec.zero_pad {
                return Err("format spec: zero-pad '0' not allowed on a string".to_string());
            }
            if let Some(sep) = spec.group {
                return Err(format!(
                    "format spec: grouping '{sep}' not allowed on a string"
                ));
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
        Some('f') | Some('F') => return render_float(spec, n as f64),
        Some('e') => return render_float(spec, n as f64),
        Some('E') => return render_float(spec, n as f64),
        Some('%') => return render_float(spec, n as f64),
        Some('g') | Some('G') => return render_float(spec, n as f64),
        Some(_) => unreachable!("validity checked by spec_valid_for_scalar"),
    };
    let radix = if spec.alt {
        match spec.ty {
            Some('x') => "0x",
            Some('X') => "0X",
            Some('o') => "0o",
            Some('b') => "0b",
            _ => "",
        }
    } else {
        ""
    };
    Ok((sign_prefix(neg, spec.sign) + radix, body, true))
}

fn render_float(spec: &FormatSpec, x: f64) -> Result<(String, String, bool), String> {
    // Validity (type char) is single-sourced in `spec_valid_for_scalar`.
    spec_valid_for_scalar(spec, ScalarKind::Float)?;
    // NaN: mask the sign so the spec path renders `NaN` (not `-NaN`), matching the bare
    // stringify path (`format_float`) which drops the NaN sign. `-inf`/`-0.0` keep their sign.
    let neg = !x.is_nan() && x.is_sign_negative() && (x != 0.0 || x.is_sign_negative());
    let mag = x.abs();
    let body = match spec.ty {
        // Fixed-point at CPython's default precision 6. This must NEVER emit scientific notation
        // and must not share the no-type `repr_float` path below.
        Some('f') | Some('F') => {
            let s = format!("{mag:.*}", spec.precision.unwrap_or(6));
            if spec.alt { force_point(s) } else { s }
        }
        // No type char: with a precision, CPython's general format plus `Py_DTSF_ADD_DOT_0`
        // (`render_general(add_dot_0 = true)`); with none, plain `repr` — no precision at all.
        None => match spec.precision {
            Some(p) => render_general(mag, p, 'e', spec.alt, true),
            None => {
                let s = repr_float(mag);
                if spec.alt { force_point(s) } else { s }
            }
        },
        // `{:e}`/`{:E}`: Python default precision 6, exponent always signed + 2-digit padded.
        Some('e') | Some('E') => {
            let p = spec.precision.unwrap_or(6);
            let marker = if spec.ty == Some('E') { 'E' } else { 'e' };
            let s = normalize_exp(&format!("{mag:.*e}", p), marker);
            if spec.alt { force_point(s) } else { s }
        }
        Some('%') => {
            let scaled = mag * 100.0;
            let p = spec.precision.unwrap_or(6);
            let s = format!("{scaled:.*}", p);
            let s = if spec.alt { force_point(s) } else { s };
            s + "%"
        }
        Some('g') | Some('G') => render_g(mag, spec),
        Some(_) => unreachable!("validity checked by spec_valid_for_scalar"),
    };
    // CPython upper-cases `inf`/`nan` under `E`/`F`/`G` (`INF`, `NAN`); lower-case type chars and
    // the no-type path keep `inf` (and Chezzi's `NaN`).
    let body = if !x.is_finite() && matches!(spec.ty, Some('E') | Some('F') | Some('G')) {
        body.to_ascii_uppercase()
    } else {
        body
    };
    // `z`: a value that ROUNDED to zero (no nonzero digit left in the body) loses its minus sign.
    let neg = neg
        && !(spec.zero_coerce && x.is_finite() && body.chars().all(|c| !matches!(c, '1'..='9')));
    Ok((sign_prefix(neg, spec.sign), body, true))
}

/// The general float format shared by `g`/`G` (`add_dot_0 = false`) and a bare `.N` precision with
/// no type char (`add_dot_0 = true`, CPython's `Py_DTSF_ADD_DOT_0`): fixed-point when the decimal
/// exponent (taken AFTER rounding to `p` significant digits) falls in `-4..hi`, scientific
/// otherwise; trailing zeros stripped unless `#`. Default precision 6, minimum 1. `add_dot_0` lowers
/// the scientific crossover by one exponent (`hi = p - 1` instead of `p`) and appends a trailing
/// `.0` to an integral fixed-point result.
fn render_general(mag: f64, precision: usize, marker: char, alt: bool, add_dot_0: bool) -> String {
    if !mag.is_finite() {
        return format!("{mag}");
    }
    let p = precision.max(1);
    let e = format!("{mag:.*e}", p - 1);
    let (_, exp_s) = e.split_once('e').expect("scientific format always has 'e'");
    let exp: i32 = exp_s.parse().expect("Rust exponent is always a valid i32");
    let hi = if add_dot_0 { p as i32 - 1 } else { p as i32 };
    let use_exp = !(-4..hi).contains(&exp);
    let body = if use_exp {
        normalize_exp(&e, marker)
    } else {
        format!("{mag:.*}", (p as i32 - 1 - exp).max(0) as usize)
    };
    let body = if alt {
        force_point(body)
    } else {
        strip_g_zeros(body)
    };
    if add_dot_0 && !use_exp && !body.contains('.') {
        body + ".0"
    } else {
        body
    }
}

/// The `g`/`G` general float format: fixed-point when the decimal exponent (taken AFTER rounding to
/// `p` significant digits) falls in `-4..p`, scientific otherwise; trailing zeros stripped unless
/// `#`. Default precision 6, minimum 1.
fn render_g(mag: f64, spec: &FormatSpec) -> String {
    let marker = if spec.ty == Some('G') { 'E' } else { 'e' };
    render_general(mag, spec.precision.unwrap_or(6), marker, spec.alt, false)
}

/// Trim trailing zeros (then a trailing bare `.`) off `s`'s mantissa, leaving any exponent suffix
/// untouched. A mantissa with no `.` is returned unchanged (nothing to trim).
fn strip_g_zeros(s: String) -> String {
    let (mant, marker_exp) = if let Some((m, e)) = s.split_once('e') {
        (m, Some(('e', e)))
    } else if let Some((m, e)) = s.split_once('E') {
        (m, Some(('E', e)))
    } else {
        (s.as_str(), None)
    };
    if !mant.contains('.') {
        return s;
    }
    let trimmed = mant.trim_end_matches('0').trim_end_matches('.');
    match marker_exp {
        Some((marker, e)) => format!("{trimmed}{marker}{e}"),
        None => trimmed.to_string(),
    }
}

/// `#` alternate form on a float: force a decimal point when the value renders with none. Returns
/// `s` unchanged when the mantissa carries no digit (`inf`, `-inf`, `NaN` must never gain a point)
/// or already has a point.
fn force_point(s: String) -> String {
    let (mant, marker_exp) = if let Some((m, e)) = s.split_once('e') {
        (m, Some(('e', e)))
    } else if let Some((m, e)) = s.split_once('E') {
        (m, Some(('E', e)))
    } else {
        (s.as_str(), None)
    };
    if !mant.bytes().any(|b| b.is_ascii_digit()) || mant.contains('.') {
        return s;
    }
    match marker_exp {
        Some((marker, e)) => format!("{mant}.{marker}{e}"),
        None => format!("{mant}."),
    }
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

fn sign_prefix(neg: bool, sign: Option<char>) -> String {
    if neg {
        "-".to_string()
    } else {
        match sign {
            Some('+') => "+".to_string(),
            Some(' ') => " ".to_string(),
            _ => String::new(),
        }
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
/// path (`print`, `{f}` interpolation with no type char, `json.stringify`). Matches
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
pub fn repr_float(x: f64) -> String {
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
        assert_eq!(s.sign, Some('+'));
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
    fn z_is_rejected_on_an_int_type_and_a_string() {
        let z = parse("z").unwrap();
        assert!(z.zero_coerce);
        assert_eq!(
            spec_valid_for_scalar(&z, ScalarKind::Int).unwrap_err(),
            "format spec: negative-zero coercion 'z' not allowed on an integer"
        );
        assert_eq!(
            spec_valid_for_scalar(&z, ScalarKind::Str).unwrap_err(),
            "format spec: negative-zero coercion 'z' not allowed on a string"
        );
        // An int under a float type char is rendered as a float, so `z` applies (CPython: `0.0`).
        assert!(spec_valid_for_scalar(&parse("z.1f").unwrap(), ScalarKind::Int).is_ok());
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
        assert!(spec_valid_for_scalar(&p("g"), ScalarKind::Float).is_ok());
        assert!(spec_valid_for_scalar(&p(".3g"), ScalarKind::Int).is_ok());
        assert!(
            spec_valid_for_scalar(&p("g"), ScalarKind::Str)
                .unwrap_err()
                .contains("type 'g' not valid for a string")
        );
    }

    #[test]
    fn general_format_matches_cpython() {
        assert_eq!(ok_apply("g", FmtArg::Float(1234.5)), "1234.5");
        assert_eq!(ok_apply("g", FmtArg::Float(1234567.0)), "1.23457e+06");
        assert_eq!(ok_apply("g", FmtArg::Float(0.0001234)), "0.0001234");
        assert_eq!(ok_apply("g", FmtArg::Float(1e-5)), "1e-05");
        assert_eq!(ok_apply("g", FmtArg::Float(1e15)), "1e+15");
        assert_eq!(ok_apply("g", FmtArg::Float(1e16)), "1e+16");
        assert_eq!(ok_apply("g", FmtArg::Float(999999.0)), "999999");
        assert_eq!(ok_apply("g", FmtArg::Float(1000000.0)), "1e+06");
        assert_eq!(ok_apply("g", FmtArg::Float(999999.5)), "1e+06");
        assert_eq!(ok_apply("g", FmtArg::Float(999999.4)), "999999");
        assert_eq!(ok_apply("g", FmtArg::Float(0.0)), "0");
        assert_eq!(ok_apply("g", FmtArg::Float(-0.0)), "-0");
        assert_eq!(ok_apply("g", FmtArg::Int(255)), "255");
        assert_eq!(ok_apply(".2g", FmtArg::Float(1234.5)), "1.2e+03");
        assert_eq!(ok_apply(".0g", FmtArg::Float(1234.5)), "1e+03");
        assert_eq!(ok_apply(".0g", FmtArg::Float(0.5)), "0.5");
        assert_eq!(ok_apply(".4g", FmtArg::Float(9.9999)), "10");
        assert_eq!(ok_apply(".5g", FmtArg::Float(99999.9)), "1e+05");
        assert_eq!(ok_apply(".16g", FmtArg::Float(1e15)), "1000000000000000");
        assert_eq!(ok_apply("G", FmtArg::Float(1.2345e-5)), "1.2345E-05");
        assert_eq!(ok_apply(",g", FmtArg::Float(1234.5)), "1,234.5");
        assert_eq!(ok_apply("_g", FmtArg::Float(1234.5)), "1_234.5");
        assert_eq!(ok_apply(",g", FmtArg::Float(1234567.0)), "1.23457e+06");
        assert_eq!(ok_apply("#g", FmtArg::Int(255)), "255.000");
        assert_eq!(ok_apply("#g", FmtArg::Float(123456.0)), "123456.");
        assert_eq!(ok_apply("#.0g", FmtArg::Float(1234.0)), "1.e+03");
        assert_eq!(ok_apply("#.0g", FmtArg::Float(1.0)), "1.");
        assert_eq!(ok_apply("#g", FmtArg::Float(0.0)), "0.00000");
        assert_eq!(ok_apply("#g", FmtArg::Float(1e20)), "1.00000e+20");
        assert_eq!(ok_apply("g", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("#g", FmtArg::Float(f64::INFINITY)), "inf");
        // Upper-case type chars upper-case a non-finite value like CPython (`INF`/`-INF`/`NAN`);
        // lower-case ones keep `inf` (and `NaN`, the one documented casing divergence).
        assert_eq!(ok_apply("G", FmtArg::Float(f64::NAN)), "NAN");
        assert_eq!(ok_apply("g", FmtArg::Float(f64::NAN)), "NaN");
        assert_eq!(ok_apply("E", FmtArg::Float(f64::INFINITY)), "INF");
        assert_eq!(ok_apply("E", FmtArg::Float(f64::NEG_INFINITY)), "-INF");
        assert_eq!(ok_apply("F", FmtArg::Float(f64::NEG_INFINITY)), "-INF");
        assert_eq!(ok_apply("F", FmtArg::Float(1.5)), "1.500000");
        assert_eq!(ok_apply("F", FmtArg::Int(2)), "2.000000");
        assert_eq!(
            ok_apply("10.3E", FmtArg::Float(f64::INFINITY)),
            "       INF"
        );
        assert_eq!(ok_apply("e", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("#f", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("%", FmtArg::Float(f64::INFINITY)), "inf%");
    }

    #[test]
    fn pad_align_matches_cpython() {
        assert_eq!(ok_apply("=10", FmtArg::Int(42)), "        42");
        assert_eq!(ok_apply("=10", FmtArg::Int(-42)), "-       42");
        assert_eq!(ok_apply("=+10", FmtArg::Int(42)), "+       42");
        assert_eq!(ok_apply("*=10", FmtArg::Int(-42)), "-*******42");
        assert_eq!(ok_apply("=010", FmtArg::Int(-42)), "-000000042");
        assert_eq!(ok_apply("0=10", FmtArg::Int(42)), "0000000042");
        // fill `=`, align `^`
        assert_eq!(ok_apply("=^10", FmtArg::Int(42)), "====42====");
        assert_eq!(ok_apply("=10", FmtArg::Float(f64::INFINITY)), "       inf");
        assert_eq!(ok_apply("=+012,", FmtArg::Int(1234567)), "+001,234,567");
        assert_eq!(ok_apply("=012,", FmtArg::Int(-1234567)), "-001,234,567");
        // the four values that discriminate the overshoot rule
        assert_eq!(ok_apply("=08,", FmtArg::Int(1000)), "0,001,000");
        assert_eq!(ok_apply("0=8,", FmtArg::Int(1000)), "0,001,000");
        assert_eq!(ok_apply("*=08,", FmtArg::Int(1000)), "***1,000");
        assert_eq!(ok_apply("=8,", FmtArg::Int(1000)), "   1,000");
        assert_eq!(ok_apply("=09,", FmtArg::Int(-1000)), "-0,001,000");
    }

    #[test]
    fn grouping_parses_in_the_cpython_slot() {
        let s = parse(",").unwrap();
        assert_eq!(s.group, Some(','));

        let s = parse("_").unwrap();
        assert_eq!(s.group, Some('_'));

        let s = parse("010,.2f").unwrap();
        assert!(s.zero_pad);
        assert_eq!(s.width, 10);
        assert_eq!(s.group, Some(','));
        assert_eq!(s.precision, Some(2));
        assert_eq!(s.ty, Some('f'));

        let s = parse("_>9").unwrap();
        assert_eq!(s.fill, '_');
        assert_eq!(s.align, Some(Align::Right));
        assert_eq!(s.group, None);

        let err = parse(",,").unwrap_err();
        assert_eq!(err, "format spec: unknown type char ','");
    }

    #[test]
    fn grouping_validity_matches_cpython() {
        let p = |s: &str| parse(s).unwrap();
        assert!(
            spec_valid_for_scalar(&p(","), ScalarKind::Str)
                .unwrap_err()
                .contains("grouping ',' not allowed on a string")
        );
        assert!(
            spec_valid_for_scalar(&p(",x"), ScalarKind::Int)
                .unwrap_err()
                .contains("grouping ',' not valid with type 'x'")
        );
        assert!(spec_valid_for_scalar(&p(","), ScalarKind::Int).is_ok());
        assert!(spec_valid_for_scalar(&p(","), ScalarKind::Float).is_ok());
        assert!(spec_valid_for_scalar(&p("_x"), ScalarKind::Int).is_ok());
    }

    #[test]
    fn sign_slot_matches_cpython() {
        assert_eq!(ok_apply(" d", FmtArg::Int(42)), " 42");
        assert_eq!(ok_apply(" d", FmtArg::Int(-42)), "-42");
        assert_eq!(ok_apply(" ", FmtArg::Int(42)), " 42");
        assert_eq!(ok_apply(" 010", FmtArg::Int(-42)), "-000000042");
        assert_eq!(ok_apply(" .2f", FmtArg::Float(1.5)), " 1.50");
        assert_eq!(ok_apply("-d", FmtArg::Int(42)), "42");
        assert_eq!(ok_apply("-d", FmtArg::Int(-42)), "-42");
        assert_eq!(parse(" d").unwrap().sign, Some(' '));
        assert_eq!(parse("+d").unwrap().sign, Some('+'));
        assert_eq!(parse("d").unwrap().sign, None);
    }

    #[test]
    fn alternate_form_matches_cpython() {
        assert_eq!(ok_apply("#x", FmtArg::Int(255)), "0xff");
        assert_eq!(ok_apply("#X", FmtArg::Int(255)), "0XFF");
        assert_eq!(ok_apply("#o", FmtArg::Int(255)), "0o377");
        assert_eq!(ok_apply("#b", FmtArg::Int(255)), "0b11111111");
        assert_eq!(ok_apply("#x", FmtArg::Int(-255)), "-0xff");
        assert_eq!(ok_apply("#x", FmtArg::Int(0)), "0x0");
        assert_eq!(ok_apply("#010x", FmtArg::Int(255)), "0x000000ff");
        assert_eq!(ok_apply("#10x", FmtArg::Int(255)), "      0xff");
        assert_eq!(ok_apply("#d", FmtArg::Int(255)), "255");
        assert_eq!(ok_apply("#", FmtArg::Int(255)), "255");
        assert_eq!(ok_apply("#_x", FmtArg::Int(1048575)), "0xf_ffff");
        assert_eq!(ok_apply("#012_x", FmtArg::Int(1048575)), "0x0_000f_ffff");
        assert_eq!(ok_apply("#_b", FmtArg::Int(255)), "0b1111_1111");
        // fill `#`, not alternate form
        assert_eq!(ok_apply("#<8x", FmtArg::Int(255)), "ff######");
        assert_eq!(ok_apply("#", FmtArg::Float(2.0)), "2.0");
        assert_eq!(ok_apply("#.0f", FmtArg::Float(0.5)), "0.");
        assert_eq!(ok_apply("#.0e", FmtArg::Float(1.5)), "2.e+00");
        assert_eq!(ok_apply("#.0%", FmtArg::Float(0.5)), "50.%");
        // non-finite: `#` must never add a point
        assert_eq!(ok_apply("#f", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("#e", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("#%", FmtArg::Float(f64::INFINITY)), "inf%");
        assert_eq!(ok_apply("#f", FmtArg::Float(f64::NEG_INFINITY)), "-inf");
        assert_eq!(
            ok_apply("#010f", FmtArg::Float(f64::INFINITY)),
            "0000000inf"
        );
        // CPython prints `nan`; the casing is the documented divergence (Gotcha 5)
        assert_eq!(ok_apply("#f", FmtArg::Float(f64::NAN)), "NaN");
    }

    #[test]
    fn grouping_renders_like_cpython() {
        assert_eq!(ok_apply(",", FmtArg::Int(1000)), "1,000");
        assert_eq!(ok_apply("_", FmtArg::Int(1000)), "1_000");
        assert_eq!(ok_apply(",", FmtArg::Int(-1234567)), "-1,234,567");
        assert_eq!(ok_apply("08,", FmtArg::Int(1000)), "0,001,000");
        assert_eq!(ok_apply("06,", FmtArg::Int(1000)), "01,000");
        assert_eq!(ok_apply("010,", FmtArg::Int(1000)), "00,001,000");
        assert_eq!(ok_apply("012,", FmtArg::Int(1000)), "0,000,001,000");
        assert_eq!(ok_apply("09,", FmtArg::Int(-1000)), "-0,001,000");
        assert_eq!(ok_apply("05,", FmtArg::Int(0)), "0,000");
        assert_eq!(ok_apply(">9,", FmtArg::Int(1000)), "    1,000");
        assert_eq!(ok_apply("+,", FmtArg::Int(1000)), "+1,000");
        assert_eq!(ok_apply("012,.1f", FmtArg::Float(1234.5)), "00,001,234.5");
        assert_eq!(ok_apply(",.2f", FmtArg::Float(1234.5678)), "1,234.57");
        assert_eq!(ok_apply(",", FmtArg::Float(1e20)), "1e+20");
        assert_eq!(ok_apply("_x", FmtArg::Int(1048575)), "f_ffff");
        assert_eq!(ok_apply("_b", FmtArg::Int(255)), "1111_1111");
    }

    #[test]
    fn f_type_char_never_uses_scientific_notation() {
        assert_eq!(
            ok_apply("f", FmtArg::Float(1e16)),
            "10000000000000000.000000"
        );
        assert_eq!(ok_apply("f", FmtArg::Float(2.5)), "2.500000");
        assert_eq!(ok_apply("f", FmtArg::Float(1e-7)), "0.000000");
        assert_eq!(ok_apply("f", FmtArg::Int(3)), "3.000000");
        assert_eq!(ok_apply("f", FmtArg::Float(-2.5)), "-2.500000");
        assert_eq!(
            ok_apply("f", FmtArg::Float(1e17)),
            "100000000000000000.000000"
        );
        assert_eq!(ok_apply(".2f", FmtArg::Float(1e16)), "10000000000000000.00");
        assert_eq!(ok_apply(".0f", FmtArg::Float(2.5)), "2");
        assert_eq!(ok_apply("#.0f", FmtArg::Float(0.5)), "0.");
        assert_eq!(ok_apply("f", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("#f", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("f", FmtArg::Float(f64::NAN)), "NaN");
        assert_eq!(ok_apply("012,f", FmtArg::Float(1234.5)), "1,234.500000");
    }

    #[test]
    fn no_type_precision_matches_cpython() {
        assert_eq!(ok_apply(".3", FmtArg::Float(123.456)), "1.23e+02");
        assert_eq!(ok_apply(".3", FmtArg::Float(2.5)), "2.5");
        assert_eq!(ok_apply(".3", FmtArg::Float(0.0)), "0.0");
        assert_eq!(ok_apply(".6", FmtArg::Float(100.0)), "100.0");
        assert_eq!(ok_apply(".6", FmtArg::Float(1234.5678)), "1234.57");
        assert_eq!(ok_apply(".1", FmtArg::Float(2.5)), "2e+00");
        assert_eq!(ok_apply(".0", FmtArg::Float(2.5)), "2e+00");
        assert_eq!(ok_apply(".3", FmtArg::Float(1e16)), "1e+16");
        assert_eq!(ok_apply(".3", FmtArg::Float(1e-7)), "1e-07");
        assert_eq!(ok_apply(".10", FmtArg::Float(999999.5)), "999999.5");
        assert_eq!(ok_apply(".3", FmtArg::Float(999999.5)), "1e+06");
        assert_eq!(ok_apply("#.6", FmtArg::Float(100.0)), "100.000");
        assert_eq!(ok_apply(".3", FmtArg::Float(f64::INFINITY)), "inf");
        assert_eq!(ok_apply("", FmtArg::Float(5.0)), "5.0");
        assert_eq!(ok_apply(">8.3", FmtArg::Float(123.456)), "1.23e+02");
    }

    #[test]
    fn zero_flag_survives_explicit_align() {
        assert_eq!(ok_apply(">08", FmtArg::Int(42)), "00000042");
        assert_eq!(ok_apply("08", FmtArg::Int(42)), "00000042");
        assert_eq!(ok_apply("<08", FmtArg::Int(42)), "42000000");
        assert_eq!(ok_apply("^08", FmtArg::Int(42)), "00042000");
        assert_eq!(ok_apply(">08", FmtArg::Int(-42)), "00000-42");
        assert_eq!(ok_apply("<08", FmtArg::Int(-42)), "-4200000");
        assert_eq!(ok_apply("^08", FmtArg::Int(-42)), "00-42000");
        assert_eq!(ok_apply("=08", FmtArg::Int(-42)), "-0000042");
        assert_eq!(ok_apply("*>08", FmtArg::Int(42)), "******42");
        assert_eq!(ok_apply("0>8", FmtArg::Int(-42)), "00000-42");
        assert_eq!(ok_apply(">+08", FmtArg::Int(42)), "00000+42");
        assert_eq!(ok_apply(">08x", FmtArg::Int(42)), "0000002a");
        assert_eq!(ok_apply("#>08x", FmtArg::Int(42)), "######2a");
        assert_eq!(ok_apply(">08,", FmtArg::Int(1000)), "0001,000");
        assert_eq!(ok_apply("<08,", FmtArg::Int(1000)), "1,000000");
        assert_eq!(ok_apply("^08,", FmtArg::Int(-1000)), "0-1,0000");
        assert_eq!(ok_apply(">08.2f", FmtArg::Float(-2.5)), "000-2.50");
        assert_eq!(ok_apply(">010f", FmtArg::Float(-2.5)), "0-2.500000");
        assert_eq!(ok_apply("<08.1f", FmtArg::Float(-2.5)), "-2.50000");
        assert_eq!(ok_apply(">08.2f", FmtArg::Int(-42)), "00-42.00");
    }

    /// The items a spec string spells, with `{}` standing for one nested field.
    fn items(spec: &str) -> Vec<SpecItem> {
        let mut out = Vec::new();
        let mut chars = spec.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' && chars.peek() == Some(&'}') {
                chars.next();
                out.push(SpecItem::Field);
            } else {
                out.push(SpecItem::Ch(c));
            }
        }
        out
    }

    #[test]
    fn nested_field_slots_parse() {
        let s = parse_nested(&items("<{}")).unwrap();
        assert!(s.dyn_width && !s.dyn_precision);
        assert_eq!(s.align, Some(Align::Left));

        let s = parse_nested(&items("{}.{}f")).unwrap();
        assert!(s.dyn_width && s.dyn_precision);
        assert_eq!(s.ty, Some('f'));
        assert_eq!(s.precision, Some(0));

        // A literal spec carries no fields and `parse` is the same function.
        let s = parse_nested(&items(">8.2f")).unwrap();
        assert!(!s.dyn_width && !s.dyn_precision);
        assert_eq!(parse(">8.2f").unwrap(), s);

        let slots = "format spec: a nested field is allowed only as the width or the precision";
        for bad in ["{}<3", ">5{}", "{}{}", "d{}"] {
            assert_eq!(parse_nested(&items(bad)).unwrap_err(), slots, "{bad}");
        }
    }

    /// The lexer decides where a nested field sits; a `{` that is plain spec text is a fill char.
    #[test]
    fn nested_field_is_an_item_not_a_brace_scan() {
        use SpecItem::{Ch, Field};
        let s = parse_nested(&[Ch('<'), Field]).unwrap();
        assert!(s.dyn_width);
        let s = parse_nested(&[Ch('{'), Ch('>'), Ch('4')]).unwrap();
        assert_eq!((s.fill, s.align, s.width), ('{', Some(Align::Right), 4));
        assert!(!s.dyn_width);
        // A text `{` outside the fill slot is an unknown type char, not a field.
        assert_eq!(
            parse_nested(&[Ch('{')]).unwrap_err(),
            "format spec: unknown type char '{'"
        );
        // A field in the fill slot is not a fill char.
        assert_eq!(
            parse_nested(&[Field, Ch('>'), Ch('4')]).unwrap_err(),
            "format spec: a nested field is allowed only as the width or the precision"
        );
    }

    #[test]
    fn runtime_field_value_is_bounded() {
        assert_eq!(field_from_int(Some(6), "width"), Ok(6));
        assert_eq!(field_from_int(Some(0), "width"), Ok(0));
        assert_eq!(field_from_int(Some(4096), "width"), Ok(4096));
        let over = "format spec: width exceeds maximum 4096".to_string();
        assert_eq!(field_from_int(Some(4097), "width"), Err(over.clone()));
        assert_eq!(field_from_int(Some(9999999999), "width"), Err(over));
        assert_eq!(
            field_from_int(Some(i64::MAX), "precision"),
            Err("format spec: precision exceeds maximum 4096".to_string())
        );
        assert_eq!(
            field_from_int(Some(-1), "width"),
            Err("format spec: width must not be negative, got -1".to_string())
        );
        assert_eq!(
            field_from_int(None, "width"),
            Err("format spec: a nested width field must be an int".to_string())
        );
    }
}
