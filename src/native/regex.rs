//! `std.regex` — regular expressions (M9), backed by the `regex` crate.
//!
//! **Stateless by design.** The native seam (`Host`) only passes int/float/str arguments, so a
//! compiled `Regex` can never be handed back into a later call as an argument. Every function
//! therefore takes the *pattern string* plus its subject; a thread-local compile cache keyed by the
//! pattern keeps repeated calls cheap (the program is single-threaded, so a `thread_local!` is both
//! safe and contention-free). A bad pattern lowers to a chezzi `Err`.
//!
//! Offsets (`Match.start`/`Match.end`) are **byte** offsets into the subject (the `regex` crate's
//! native unit). `groups` holds capture groups 1..n as strings; a non-participating optional group
//! becomes `""`.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// One match: the matched text, its byte span, and its capture groups (1..n).
#[derive(Debug, Clone, PartialEq)]
pub struct MatchData {
    pub text: String,
    pub start: i64,
    pub end: i64,
    pub groups: Vec<String>,
}

thread_local! {
    /// Pattern → compiled regex. Single-threaded program ⇒ a `thread_local!` is safe and avoids
    /// recompiling a pattern reused across calls (e.g. a regex applied inside a loop).
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

/// Build a `MatchData` from a `Captures`: group 0 is the whole match (text + span), groups 1..n are
/// the capture subgroups (a non-participating optional group becomes `""`).
fn match_from_caps(caps: &regex::Captures) -> MatchData {
    let whole = caps.get(0).expect("group 0 always present on a match");
    let groups = (1..caps.len())
        .map(|i| {
            caps.get(i)
                .map_or(String::new(), |g| g.as_str().to_string())
        })
        .collect();
    MatchData {
        text: whole.as_str().to_string(),
        start: whole.start() as i64,
        end: whole.end() as i64,
        groups,
    }
}

fn do_is_match(pat: &str, s: &str) -> Result<bool, String> {
    Ok(compiled(pat)?.is_match(s))
}

fn do_find(pat: &str, s: &str) -> Result<Option<MatchData>, String> {
    Ok(compiled(pat)?
        .captures(s)
        .map(|caps| match_from_caps(&caps)))
}

fn do_find_all(pat: &str, s: &str) -> Result<Vec<MatchData>, String> {
    let re = compiled(pat)?;
    Ok(re
        .captures_iter(s)
        .map(|caps| match_from_caps(&caps))
        .collect())
}

fn do_replace_all(pat: &str, s: &str, repl: &str) -> Result<String, String> {
    Ok(compiled(pat)?.replace_all(s, repl).into_owned())
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

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("is_match", is_match),
    ("find", find),
    ("find_all", find_all),
    ("replace_all", replace_all),
    ("split", split),
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
}
