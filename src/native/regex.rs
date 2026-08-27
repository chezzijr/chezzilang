//! `std.regex` — regular expressions (M9), backed by the `regex` crate.
//!
//! **Stateless by design.** The native seam (`Host`) only passes int/float/str arguments, so a
//! compiled `Regex` can never be handed back into a later call as an argument. Every function
//! therefore takes the *pattern string* plus its subject; a thread-local compile cache keyed by the
//! pattern keeps repeated calls cheap (a `thread_local!` needs no lock and cannot contend). Under the
//! default M:N engine a program runs on many worker threads, so the cache is PER WORKER: a pattern is
//! compiled once per worker that touches it, and the memory bound is `workers × CACHE_CAP`, not
//! `CACHE_CAP`. That is the deliberate trade (no shared lock on a hot path); a single shared cache
//! behind a lock is the upgrade if compile cost ever shows up. A bad pattern lowers to a chezzi `Err`.
//!
//! Offsets (`Match.start`/`Match.end`) are **codepoint** offsets into the subject (Python `re`
//! semantics; the `regex` crate's native byte spans are converted), so the invariant
//! `subject[m.start:m.end] == m.text` holds — Chezzi slicing is codepoint-indexed. `groups` holds
//! capture groups 1..n as strings; a non-participating optional group becomes `""`.

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// One match: the matched text, its codepoint span, and its capture groups (1..n).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchData {
    pub text: String,
    pub start: i64,
    pub end: i64,
    pub groups: Vec<String>,
}

thread_local! {
    /// Pattern → compiled regex, PER WORKER THREAD (see the module doc): lock-free, and avoids
    /// recompiling a pattern reused across calls (e.g. a regex applied inside a loop).
    /// ponytail: per-worker duplication (≤ `workers × CACHE_CAP` compiled patterns); one shared
    /// locked cache if compile cost ever shows up in a bench.
    static CACHE: RefCell<HashMap<String, Rc<regex::Regex>>> = RefCell::new(HashMap::new());
}

/// Upper bound on cached compiled patterns. A program that compiles distinct patterns in an
/// unbounded loop (e.g. from input) must not leak memory monotonically; past the cap we still
/// compile, just without caching. Generous enough that real programs always hit the cache.
const CACHE_CAP: usize = 1024;

/// Compile `pat`, returning a cached handle. A malformed pattern returns its error message.
fn compiled(pat: &str) -> Result<Rc<regex::Regex>, String> {
    CACHE.with(|c| {
        if let Some(re) = c.borrow().get(pat) {
            return Ok(re.clone());
        }
        match regex::Regex::new(pat) {
            Ok(re) => {
                let rc = Rc::new(re);
                let mut cache = c.borrow_mut();
                if cache.len() < CACHE_CAP {
                    cache.insert(pat.to_string(), rc.clone());
                }
                Ok(rc)
            }
            Err(e) => Err(e.to_string()),
        }
    })
}

/// Ascending byte→codepoint cursor over one subject. The `regex` crate reports BYTE spans; Chezzi
/// slicing/indexing is CODEPOINT-based (`s[m.start:m.end] == m.text` must hold on non-ASCII too), so
/// every span is converted here. `captures_iter` yields non-overlapping, monotonically increasing
/// spans, so one forward-only cursor converts a whole `find_all` in O(subject) total — never rescan
/// the prefix from byte 0 per match (that is O(n·m): a document scan becomes a hang).
struct CpCursor {
    byte: usize,
    cp: i64,
}

impl CpCursor {
    fn new() -> Self {
        CpCursor { byte: 0, cp: 0 }
    }

    /// Codepoint index of byte offset `to` (must be ≥ the last one asked for; always a char
    /// boundary — the `regex` crate's spans over a `&str` are char-aligned).
    fn at(&mut self, s: &str, to: usize) -> i64 {
        debug_assert!(to >= self.byte, "CpCursor must advance forward");
        self.cp += s[self.byte..to].chars().count() as i64;
        self.byte = to;
        self.cp
    }
}

/// Build a `MatchData` from a `Captures`: group 0 is the whole match (text + span), groups 1..n are
/// the capture subgroups (a non-participating optional group becomes `""`). Spans are converted from
/// bytes to codepoints via the caller's ascending `cur` (see `CpCursor`).
fn match_from_caps(caps: &regex::Captures, s: &str, cur: &mut CpCursor) -> MatchData {
    let whole = caps.get(0).expect("group 0 always present on a match");
    let groups = (1..caps.len())
        .map(|i| {
            caps.get(i)
                .map_or(String::new(), |g| g.as_str().to_string())
        })
        .collect();
    MatchData {
        text: whole.as_str().to_string(),
        start: cur.at(s, whole.start()),
        end: cur.at(s, whole.end()),
        groups,
    }
}

fn do_is_match(pat: &str, s: &str) -> Result<bool, String> {
    Ok(compiled(pat)?.is_match(s))
}

fn do_find(pat: &str, s: &str) -> Result<Option<MatchData>, String> {
    let mut cur = CpCursor::new();
    Ok(compiled(pat)?
        .captures(s)
        .map(|caps| match_from_caps(&caps, s, &mut cur)))
}

fn do_find_all(pat: &str, s: &str) -> Result<Vec<MatchData>, String> {
    let re = compiled(pat)?;
    let mut cur = CpCursor::new();
    Ok(re
        .captures_iter(s)
        .map(|caps| match_from_caps(&caps, s, &mut cur))
        .collect())
}

/// Python's `re.sub` replacement dialect is not RE2's: `\1` and `\g<name>` are group references in
/// Python, and mean NOTHING to the `regex` crate's expander, which copies them through as literal
/// text — so the call returned a plausible `Ok` (`docs/gaps.md` W8-6). Reject both and name the RE2
/// form. A backslash NOT followed by a digit or `g<` stays literal, so `r"\n"` and `r"C:\tmp"` are
/// untouched; that is the only way left to put a backslash in a replacement, and it is deliberate.
fn check_replacement_dialect(repl: &str) -> Result<(), String> {
    for (i, c) in repl.bytes().enumerate() {
        if c != b'\\' {
            continue;
        }
        let rest = &repl[i + 1..];
        if rest.starts_with(|c: char| c.is_ascii_digit()) {
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            return Err(format!(
                "replace_all: '\\{n}' is not a group reference here — the replacement dialect is \
                 RE2 (the Rust regex crate), not Python's re; write '${n}' or '${{{n}}}'"
            ));
        }
        let named = rest
            .strip_prefix("g<")
            .and_then(|a| a.find('>').map(|end| &a[..end]));
        if let Some(name) = named {
            return Err(format!(
                "replace_all: '\\g<{name}>' is not a group reference here — the replacement \
                 dialect is RE2 (the Rust regex crate), not Python's re; write '${{{name}}}'"
            ));
        }
    }
    Ok(())
}

fn do_replace_all(pat: &str, s: &str, repl: &str) -> Result<String, String> {
    let re = compiled(pat)?;
    check_replacement_dialect(repl)?;
    Ok(re.replace_all(s, repl).into_owned())
}

fn do_split(pat: &str, s: &str) -> Result<Vec<String>, String> {
    Ok(compiled(pat)?.split(s).map(|x| x.to_string()).collect())
}

// ----- Host seam: thin wrappers mapping (pattern, subject) args → lowered `NativeRet`. -----

/// Lower a `MatchData` to a `Match` struct value (the checker seeds the matching struct shape).
fn match_to_ret(m: MatchData) -> NativeRet {
    NativeRet::Struct {
        name: "Match".into(),
        fields: vec![
            ("text".into(), NativeRet::Str(m.text)),
            ("start".into(), NativeRet::Int(m.start)),
            ("end".into(), NativeRet::Int(m.end)),
            (
                "groups".into(),
                NativeRet::List(m.groups.into_iter().map(NativeRet::Str).collect()),
            ),
        ],
    }
}

/// Map a helper's `Result<T, String>` to a chezzi `Result` (`Err` carries the regex error message).
fn result_of<T>(r: Result<T, String>, ok: impl FnOnce(T) -> NativeRet) -> NativeRet {
    match r {
        Ok(v) => NativeRet::Ok(Box::new(ok(v))),
        Err(e) => NativeRet::Err(e),
    }
}

fn is_match(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_match", 2)?;
    let (pat, s) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(result_of(do_is_match(&pat, &s), NativeRet::Bool))
}

fn find(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "find", 2)?;
    let (pat, s) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(result_of(do_find(&pat, &s), |opt| match opt {
        Some(m) => NativeRet::Some(Box::new(match_to_ret(m))),
        None => NativeRet::None,
    }))
}

fn find_all(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "find_all", 2)?;
    let (pat, s) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(result_of(do_find_all(&pat, &s), |ms| {
        NativeRet::List(ms.into_iter().map(match_to_ret).collect())
    }))
}

fn replace_all(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "replace_all", 3)?;
    let (pat, s, repl) = (h.arg_str(0)?, h.arg_str(1)?, h.arg_str(2)?);
    Ok(result_of(do_replace_all(&pat, &s, &repl), NativeRet::Str))
}

fn split(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "split", 2)?;
    let (pat, s) = (h.arg_str(0)?, h.arg_str(1)?);
    Ok(result_of(do_split(&pat, &s), |parts| {
        NativeRet::List(parts.into_iter().map(NativeRet::Str).collect())
    }))
}

/// Callable members. `(name, fn, kind)`.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("is_match", is_match, Kind::Inline),
    ("find", find, Kind::Inline),
    ("find_all", find_all, Kind::Inline),
    ("replace_all", replace_all, Kind::Inline),
    ("split", split, Kind::Inline),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_match_true_and_false() {
        assert_eq!(do_is_match(r"\d+", "ab12"), Ok(true));
        assert_eq!(do_is_match(r"\d+", "abc"), Ok(false));
    }

    #[test]
    fn bad_pattern_is_err() {
        assert!(do_is_match(r"(", "x").is_err());
        assert!(do_find(r"(", "x").is_err());
    }

    #[test]
    fn find_first_match_with_span() {
        let m = do_find(r"\d+", "ab12cd34").unwrap().unwrap();
        assert_eq!(m.text, "12");
        assert_eq!((m.start, m.end), (2, 4));
        assert_eq!(m.groups, Vec::<String>::new());
    }

    #[test]
    fn find_offsets_are_codepoints_not_bytes() {
        // "héllo": bytes h=0, é=1..3, l=3, l=4, o=5 — codepoints h=0, é=1, l=2, l=3, o=4.
        let m = do_find(r"l+", "héllo").unwrap().unwrap();
        assert_eq!(m.text, "ll");
        assert_eq!((m.start, m.end), (2, 4));
    }

    /// The acceptance invariant: slicing the subject by (start, end) — Chezzi slices by CODEPOINT —
    /// must reproduce `.text`, on ASCII and non-ASCII alike.
    #[test]
    fn offsets_slice_back_to_text() {
        let cases: &[(&str, &str)] = &[
            (r"\d+", "ab12cd34"),
            (r"l+", "héllo"),
            (r"\d+", "π1 π22"),
            (r"x*", "héllo"), // zero-width match at codepoint 0
        ];
        for (pat, subj) in cases {
            for m in do_find_all(pat, subj).unwrap() {
                let sliced: String = subj
                    .chars()
                    .skip(m.start as usize)
                    .take((m.end - m.start) as usize)
                    .collect();
                assert_eq!(
                    sliced,
                    m.text,
                    "pat={pat} subj={subj} span={:?}",
                    (m.start, m.end)
                );
            }
        }
    }

    /// The byte→codepoint conversion must be LINEAR in the subject across a whole `find_all`, not a
    /// from-zero prefix rescan per match (that is O(n·m) — a document scan turns into a hang).
    /// 200k matches over a 400k-char subject: ~8e10 byte steps if quadratic (measured 2.9s in a debug
    /// build, and it grows with the square), ~400k if linear (measured ~0.1s, allocs dominating).
    #[test]
    fn find_all_offset_conversion_is_linear() {
        let subj = "a ".repeat(200_000);
        let t = std::time::Instant::now();
        let ms = do_find_all("a", &subj).unwrap();
        let dt = t.elapsed();
        assert_eq!(ms.len(), 200_000);
        assert_eq!((ms[199_999].start, ms[199_999].end), (399_998, 399_999));
        assert!(
            dt < std::time::Duration::from_secs(1),
            "find_all offset conversion looks quadratic: {dt:?}"
        );
    }

    #[test]
    fn find_none_when_no_match() {
        assert_eq!(do_find(r"\d+", "abc"), Ok(None));
    }

    #[test]
    fn find_captures_subgroups() {
        let m = do_find(r"(\w+)@(\w+)", "ann@host rest").unwrap().unwrap();
        assert_eq!(m.text, "ann@host");
        assert_eq!(m.groups, vec!["ann".to_string(), "host".to_string()]);
    }

    #[test]
    fn non_participating_group_is_empty_string() {
        // `(a)?b` — the optional group does not participate when matching plain "b".
        let m = do_find(r"(a)?b", "b").unwrap().unwrap();
        assert_eq!(m.text, "b");
        assert_eq!(m.groups, vec!["".to_string()]);
    }

    #[test]
    fn find_all_returns_every_match() {
        let ms = do_find_all(r"\d+", "1 22 333").unwrap();
        let texts: Vec<&str> = ms.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["1", "22", "333"]);
        assert_eq!((ms[1].start, ms[1].end), (2, 4));
    }

    #[test]
    fn replace_all_substitutes() {
        assert_eq!(
            do_replace_all(r"\s+", "a  b   c", "_"),
            Ok("a_b_c".to_string())
        );
    }

    #[test]
    fn split_on_pattern() {
        assert_eq!(
            do_split(r",\s*", "a, b,c"),
            Ok(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn compile_cache_reuses_handle() {
        let a = compiled(r"\d+").unwrap();
        let b = compiled(r"\d+").unwrap();
        // Same cached allocation: pointer-equal Rc.
        assert!(Rc::ptr_eq(&a, &b));
    }

    /// W8-6: Python's `\1` is not an RE2 group reference — the crate's expander copies it through as
    /// literal text, so this used to return a plausible `Ok("\2/\1")`.
    #[test]
    fn replace_all_rejects_python_backslash_group_ref() {
        let e = do_replace_all(r"(\d+)-(\d+)", "10-20", r"\2/\1").unwrap_err();
        assert!(e.contains(r"'\2'"), "{e}");
        assert!(e.contains("'$2'"), "{e}");
    }

    #[test]
    fn replace_all_rejects_python_multi_digit_group_ref() {
        let e = do_replace_all(r"(a)", "a", r"\12").unwrap_err();
        assert!(e.contains(r"'\12'"), "{e}");
        assert!(e.contains("'$12'"), "{e}");
    }

    #[test]
    fn replace_all_rejects_python_named_group_ref() {
        let e = do_replace_all(r"(?<y>\d+)", "1999", r"\g<y>!").unwrap_err();
        assert!(e.contains(r"'\g<y>'"), "{e}");
        assert!(e.contains("'${y}'"), "{e}");
    }

    #[test]
    fn replace_all_dollar_group_refs_still_work() {
        assert_eq!(
            do_replace_all(r"(\d+)-(\d+)", "10-20", r"$2/$1"),
            Ok("20/10".to_string())
        );
    }

    /// A backslash NOT before a digit or `g<` keeps its old literal meaning — the escape hatch for a
    /// replacement that genuinely wants a backslash.
    #[test]
    fn replace_all_keeps_a_literal_backslash_before_a_non_digit() {
        assert_eq!(do_replace_all(r"x", "x", r"\n"), Ok(r"\n".to_string()));
        assert_eq!(
            do_replace_all(r"x", "x", r"C:\tmp"),
            Ok(r"C:\tmp".to_string())
        );
    }

    /// Ordering: the pattern compiles first, so a bad pattern still wins over a bad replacement.
    #[test]
    fn bad_pattern_beats_the_replacement_check() {
        let e = do_replace_all(r"(", "x", r"\1").unwrap_err();
        assert!(e.contains("regex parse error"), "{e}");
    }

    /// W8-6's doc half: `docs/stdlib.md` documented the OLD `Ok('\2/\1')` result. That sentence is now
    /// false, and prose has no other gate — this one reads the shipped file (`src/main.rs:707` compiles
    /// the same doc into `chezzi docs`), so a revert of the fix without a revert of the doc fails here.
    #[test]
    fn stdlib_doc_no_longer_claims_the_old_ok_result() {
        let doc = include_str!("../../docs/stdlib.md");
        assert!(
            !doc.contains(r"Ok('\\2/\\1')"),
            "docs/stdlib.md still claims replace_all(r\"(\\d+)-(\\d+)\", \"10-20\", r\"\\2/\\1\") returns Ok"
        );
        assert!(
            doc.contains("W8-6") && doc.contains("closed 2026-08-27"),
            "docs/stdlib.md must record W8-6 as closed on 2026-08-27"
        );
    }
}
