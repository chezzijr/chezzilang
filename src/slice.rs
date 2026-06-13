//! Shared Python-style slice + negative-index resolution.
//!
//! Both engines (the bytecode VM and the frozen tree-walk interpreter) call into this one module so
//! their slice/index semantics — including every clamp boundary and the `slice step cannot be zero`
//! fault — stay byte-identical (the two engines are parity-tested on stdout/stderr). Derived from
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(slice_indices(None, None, Some(0), 5), Err("slice step cannot be zero"));
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
