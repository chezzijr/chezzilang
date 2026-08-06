//! Shared Python-style slice + negative-index resolution.
//!
//! Both VM schedulers (serial + M:N) call into this one module so
//! their slice/index semantics — including every clamp boundary and the `slice step cannot be zero`
//! fault — stay byte-identical (the two schedulers are parity-tested on stdout/stderr). Derived from
//! CPython's `PySlice_GetIndicesEx` (`slice.indices`).

/// Normalize a possibly-negative *plain* index `n` against length `len`, Python-style: a negative
/// index counts from the end (`n + len`). Returns `Some(i)` with `i < len` iff the (normalized)
/// index is in bounds; `None` otherwise (the caller faults "index out of bounds"). NOTE: unlike
/// slice bounds, a plain index does NOT clamp — out of range is an error (Python's asymmetry).
pub fn norm_index(n: i64, len: usize) -> Option<usize> {
    let i = if n < 0 { n + len as i64 } else { n };
    if i >= 0 && (i as usize) < len {
        Some(i as usize)
    } else {
        None
    }
}

/// Upper bound on the number of elements a `range()` call may materialize, to keep an absurd argument
/// from exhausting memory. (A `for` loop over a `..` range is lazy and not subject to this; this only
/// caps building an actual list — via `range()` or slicing a range value.) Shared by BOTH engines so
/// the cap (and its fault message) is byte-identical.
pub const MAX_RANGE_LEN: i64 = 10_000_000;

/// Materialize `range(start, end, step)` into the concrete list of ints, half-open `[start, end)`.
/// A positive `step` counts up, a negative `step` counts down (still excluding `end`); a
/// wrong-direction step or `start == end` yields `[]`. Returns `Err` with the byte-identical runtime
/// fault text for a zero step or an over-cap length. All arithmetic is done in `i128` so a huge span
/// or `i64::MIN` bound/step can't overflow or panic (`i64::MIN.abs()` would). Shared by BOTH engines.
pub fn range_values(start: i64, end: i64, step: i64) -> Result<Vec<i64>, String> {
    if step == 0 {
        return Err("range() step cannot be zero".to_string());
    }
    let (start, end, step) = (i128::from(start), i128::from(end), i128::from(step));
    // Number of emitted elements when the step points toward `end`, else 0 (wrong-direction/empty).
    let span = end - start;
    let n: i128 = if (step > 0 && span > 0) || (step < 0 && span < 0) {
        // ceil(|span| / |step|) without overflow: (|span| + |step| - 1) / |step|.
        let s = span.abs();
        let st = step.abs();
        (s + st - 1) / st
    } else {
        0
    };
    if n > i128::from(MAX_RANGE_LEN) {
        return Err(format!(
            "range() length {n} exceeds the maximum of {MAX_RANGE_LEN}"
        ));
    }
    let mut out = Vec::with_capacity(n as usize);
    let mut i = start;
    if step > 0 {
        while i < end {
            out.push(i as i64);
            i += step;
        }
    } else {
        while i > end {
            out.push(i as i64);
            i += step;
        }
    }
    Ok(out)
}

/// Resolve a Python slice `start:end:step` against a length-`len` sequence into the concrete list of
/// indices to materialize, in order. `None` components take their direction-dependent defaults.
/// Returns `Err` only for a zero step (the message is the runtime fault text, byte-identical across
/// engines). Mirrors CPython's `slice.indices` + the stepping loop.
pub fn slice_indices(
    start: Option<i64>,
    end: Option<i64>,
    step: Option<i64>,
    len: usize,
) -> Result<Vec<usize>, &'static str> {
    let step = step.unwrap_or(1);
    if step == 0 {
        return Err("slice step cannot be zero");
    }
    let len_i = len as i64;

    // Clamp a raw bound into the valid range for the step direction, after normalizing negatives.
    // Positive step: index in [0, len]. Negative step: index in [-1, len-1] (-1 = "before start").
    let clamp = |b: i64, is_start: bool| -> i64 {
        let mut b = if b < 0 { b + len_i } else { b };
        if step > 0 {
            b = b.clamp(0, len_i);
        } else {
            // For a negative step the lower sentinel is -1 (one before index 0).
            let _ = is_start;
            b = b.clamp(-1, len_i - 1);
        }
        b
    };

    let (lo, hi) = if step > 0 {
        let s = start.map(|b| clamp(b, true)).unwrap_or(0);
        let e = end.map(|b| clamp(b, false)).unwrap_or(len_i);
        (s, e)
    } else {
        let s = start.map(|b| clamp(b, true)).unwrap_or(len_i - 1);
        let e = end.map(|b| clamp(b, false)).unwrap_or(-1);
        (s, e)
    };

    let mut out = Vec::new();
    let mut i = lo;
    if step > 0 {
        while i < hi {
            out.push(i as usize);
            i += step;
        }
    } else {
        while i > hi {
            out.push(i as usize);
            i += step; // step is negative
        }
    }
    Ok(out)
}

/// Python `bytes` `repr`: `b'...'` with printable ASCII shown literally, `\n \t \r \\ \'` escaped,
/// and every other byte as `\xHH` (lowercase hex). Shared by BOTH engines (VM `display_guarded` and
/// interp `display_value`) so the `b'...'` representation is byte-identical across the engines —
/// `str(bytes)`, interpolation, and bare `print(bytes)` all route through this one function.
pub fn bytes_repr(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() + 3);
    out.push('b');
    out.push('\'');
    for &b in bytes {
        match b {
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            // Printable ASCII (space..=~, the escapes above already handled) prints literally.
            0x20..=0x7E => out.push(b as char),
            // Everything else (control chars, ≥0x80) as `\xHH`.
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('\'');
    out
}

/// Python `bytearray` `repr`: `bytearray(b'...')` — the bare `b'...'` of [`bytes_repr`] wrapped in
/// `bytearray(...)`. Shared by BOTH engines (VM + interp) so the mutable buffer's `Display`/`str()`/
/// interpolation are byte-identical, distinct from `bytes`' bare `b'...'`.
pub fn bytearray_repr(bytes: &[u8]) -> String {
    format!("bytearray({})", bytes_repr(bytes))
}

/// Python `str` `repr`: the quoted, escaped form used whenever a string is rendered **nested inside**
/// something else — a list/tuple/map/set element, a struct field, an enum payload. Top-level
/// `str(s)` / `print(s)` stay the bare characters, exactly like CPython's `str` vs `repr` split.
/// Without it, `["a", "b"]` and `["a, b"]` both printed `[a, b]` and `[""]` printed `[]` — different
/// values with identical output (`docs/gaps.md` §W7-25).
///
/// Quote choice follows CPython: `'` normally, switching to `"` only when the string contains a `'`
/// and no `"`. Escapes `\\`, `\n`, `\t`, `\r`, the chosen quote, and ASCII control characters
/// (`\xHH`). Non-ASCII stays literal — `repr('é')` is `'é'` in Python 3 too.
pub fn str_repr(s: &str) -> String {
    use std::fmt::Write;
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            // ASCII control characters (the three common ones are already handled above).
            c if (c as u32) < 0x20 || c as u32 == 0x7F => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checked against CPython 3.14: `repr` of each of these strings is byte-identical.
    #[test]
    fn str_repr_python_style() {
        assert_eq!(str_repr(""), "''");
        assert_eq!(str_repr("a"), "'a'");
        assert_eq!(str_repr("a, b"), "'a, b'");
        // A `'` flips the quote to `"`; a string with both keeps `'` and escapes the inner one.
        assert_eq!(str_repr("it's"), "\"it's\"");
        assert_eq!(str_repr("it's \"q\""), "'it\\'s \"q\"'");
        assert_eq!(str_repr("a\nb"), "'a\\nb'");
        assert_eq!(str_repr("a\tb\rc"), "'a\\tb\\rc'");
        assert_eq!(str_repr("back\\slash"), "'back\\\\slash'");
        assert_eq!(str_repr("\u{0}\u{1f}\u{7f}"), "'\\x00\\x1f\\x7f'");
        // Non-ASCII stays literal, like CPython 3.
        assert_eq!(str_repr("é😀"), "'é😀'");
    }

    #[test]
    fn bytes_repr_python_style() {
        assert_eq!(bytes_repr(b""), "b''");
        assert_eq!(bytes_repr(b"hi"), "b'hi'");
        assert_eq!(bytes_repr(b"hi\n"), "b'hi\\n'");
        assert_eq!(bytes_repr(&[0xFF]), "b'\\xff'");
        assert_eq!(bytes_repr(&[0x00, 0x01]), "b'\\x00\\x01'");
        assert_eq!(bytes_repr(b"a'b"), "b'a\\'b'");
    }

    #[test]
    fn bytearray_repr_wraps_bytes_repr() {
        assert_eq!(bytearray_repr(b""), "bytearray(b'')");
        assert_eq!(bytearray_repr(b"hi"), "bytearray(b'hi')");
        assert_eq!(bytearray_repr(&[0x00, 0xFF]), "bytearray(b'\\x00\\xff')");
    }

    fn idx(start: Option<i64>, end: Option<i64>, step: Option<i64>, len: usize) -> Vec<usize> {
        slice_indices(start, end, step, len).unwrap()
    }

    #[test]
    fn basic_forward() {
        // xs[1:3] on len 5 -> [1, 2]
        assert_eq!(idx(Some(1), Some(3), None, 5), vec![1, 2]);
        // xs[1:] -> [1,2,3,4]
        assert_eq!(idx(Some(1), None, None, 5), vec![1, 2, 3, 4]);
        // xs[:3] -> [0,1,2]
        assert_eq!(idx(None, Some(3), None, 5), vec![0, 1, 2]);
        // xs[:] -> all
        assert_eq!(idx(None, None, None, 5), vec![0, 1, 2, 3, 4]);
        // xs[1:99] clamps end
        assert_eq!(idx(Some(1), Some(99), None, 5), vec![1, 2, 3, 4]);
        // xs[3:1] -> empty
        assert_eq!(idx(Some(3), Some(1), None, 5), Vec::<usize>::new());
    }

    #[test]
    fn step_and_reverse() {
        // xs[0:5:2] -> [0,2,4]
        assert_eq!(idx(Some(0), Some(5), Some(2), 5), vec![0, 2, 4]);
        // xs[::-1] -> [4,3,2,1,0]
        assert_eq!(idx(None, None, Some(-1), 5), vec![4, 3, 2, 1, 0]);
        // xs[4:0:-1] -> [4,3,2,1]
        assert_eq!(idx(Some(4), Some(0), Some(-1), 5), vec![4, 3, 2, 1]);
    }

    #[test]
    fn negative_bounds_clamp() {
        // xs[-100:] clamps start to 0 (no fault on slices)
        assert_eq!(idx(Some(-100), None, None, 5), vec![0, 1, 2, 3, 4]);
        // xs[-2:] -> [3,4]
        assert_eq!(idx(Some(-2), None, None, 5), vec![3, 4]);
        // xs[:-1] -> [0,1,2,3]
        assert_eq!(idx(None, Some(-1), None, 5), vec![0, 1, 2, 3]);
        // negative-step over-range start clamps to len-1
        assert_eq!(idx(Some(100), None, Some(-1), 5), vec![4, 3, 2, 1, 0]);
    }

    #[test]
    fn empty_sequence() {
        assert_eq!(idx(None, None, None, 0), Vec::<usize>::new());
        assert_eq!(idx(None, None, Some(-1), 0), Vec::<usize>::new());
    }

    #[test]
    fn zero_step_errs() {
        assert_eq!(
            slice_indices(None, None, Some(0), 5),
            Err("slice step cannot be zero")
        );
    }

    #[test]
    fn range_values_up_down_byn() {
        assert_eq!(range_values(0, 10, 2).unwrap(), vec![0, 2, 4, 6, 8]);
        assert_eq!(range_values(1, 7, 3).unwrap(), vec![1, 4]);
        assert_eq!(
            range_values(10, 0, -1).unwrap(),
            vec![10, 9, 8, 7, 6, 5, 4, 3, 2, 1]
        );
        assert_eq!(range_values(10, 2, -3).unwrap(), vec![10, 7, 4]);
    }

    #[test]
    fn range_values_empty_and_zero_step() {
        assert_eq!(range_values(5, 5, 1).unwrap(), Vec::<i64>::new());
        assert_eq!(range_values(5, 5, -1).unwrap(), Vec::<i64>::new());
        assert_eq!(range_values(0, 10, -1).unwrap(), Vec::<i64>::new());
        assert_eq!(range_values(10, 0, 1).unwrap(), Vec::<i64>::new());
        assert_eq!(
            range_values(0, 5, 0),
            Err("range() step cannot be zero".to_string())
        );
    }

    #[test]
    fn range_values_overflow_edges() {
        // INT_MIN step must not panic (i64::MIN.abs() overflows); single huge step → one or zero elems.
        assert_eq!(
            range_values(i64::MAX, i64::MAX - 1, -1).unwrap(),
            vec![i64::MAX]
        );
        // Negative step with an ascending span is wrong-direction → empty (no i64::MIN.abs() panic).
        assert_eq!(range_values(0, 10, i64::MIN).unwrap(), Vec::<i64>::new());
        // A huge positive step over a small ascending span emits exactly one element.
        assert_eq!(range_values(0, 10, i64::MAX).unwrap(), vec![0]);
        // INT_MIN step counting down by a giant stride still emits just the start.
        assert_eq!(range_values(0, i64::MIN, i64::MIN).unwrap(), vec![0]);
        // Over-cap length is rejected, not materialized.
        assert!(range_values(0, MAX_RANGE_LEN + 5, 1).is_err());
        // A large step keeps the count under the cap.
        assert_eq!(
            range_values(0, 10_000_000_000, 1_000_000_000)
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn norm_index_python() {
        assert_eq!(norm_index(0, 5), Some(0));
        assert_eq!(norm_index(4, 5), Some(4));
        assert_eq!(norm_index(-1, 5), Some(4));
        assert_eq!(norm_index(-5, 5), Some(0));
        assert_eq!(norm_index(5, 5), None);
        assert_eq!(norm_index(-6, 5), None);
        assert_eq!(norm_index(-100, 5), None);
    }
}
