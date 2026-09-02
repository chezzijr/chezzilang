//! The open-row count is written in three places and each ticket that closes a row edits them by
//! hand, so they drift: after TICKET-012 and TICKET-013 landed they read 21 / 25 / 19 and then
//! 18 / 25 / 16, each locally correct when written. TICKET-011 established that all three must
//! agree; nothing enforced it, because a stale number fails no assertion. This is that assertion.
//!
//! The table in `docs/gaps.md` is the source of truth: one `| **W8-n** |` row per OPEN row (a
//! closed row is struck as `| ~~**W8-n**~~ |` and stops matching). The two prose counters —
//! `docs/gaps.md`'s section header and `CLAUDE.md`'s START HERE paragraph — must equal it.

use std::fs;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// The count a `**N open rows**` / `**N OPEN ROWS**` claim states, whatever its case.
fn claimed(text: &str, path: &str) -> usize {
    let lower = text.to_lowercase();
    let idx = lower
        .find(" open rows**")
        .unwrap_or_else(|| panic!("{path}: no '**N open rows**' claim found"));
    let head = &lower[..idx];
    let digits: String = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits
        .parse()
        .unwrap_or_else(|e| panic!("{path}: '{digits}' before 'open rows' is not a number: {e}"))
}

#[test]
fn open_row_counters_agree_with_the_gaps_table() {
    let gaps = read("docs/gaps.md");
    let rows = gaps.lines().filter(|l| l.starts_with("| **W8-")).count();
    assert_eq!(
        claimed(&gaps, "docs/gaps.md"),
        rows,
        "docs/gaps.md's section header disagrees with its own table ({rows} unstruck `| **W8-n** |` rows)"
    );
    assert_eq!(
        claimed(&read("CLAUDE.md"), "CLAUDE.md"),
        rows,
        "CLAUDE.md's START HERE count disagrees with docs/gaps.md's table ({rows} unstruck rows) — \
         re-derive it with: grep -c '^| \\*\\*W8-' docs/gaps.md"
    );
}

/// A row whose text already says CLOSED must have a STRUCK id, or it keeps counting as open.
/// W8-21 shipped in TICKET-025 and said "CLOSED 2026-08-30" in its own prose while its id stayed
/// `| **W8-21** |`, so all three counters agreed at 3 when the real answer was 2 — agreement is not
/// correctness when every source counts the same stale row.
#[test]
fn a_row_that_says_closed_is_struck() {
    let gaps = read("docs/gaps.md");
    let stale: Vec<&str> = gaps
        .lines()
        .filter(|l| l.starts_with("| **W8-"))
        .filter(|l| l.get(..400).unwrap_or(l).contains("CLOSED"))
        .map(|l| l.split('|').nth(1).unwrap_or(l).trim())
        .collect();
    assert!(
        stale.is_empty(),
        "these rows say CLOSED but their id is not struck, so they still count as open: {stale:?}"
    );
}

/// The W8-19 row's `Option`/`Result`-methods sub-item is DECLINED (TICKET-037): `??` already covers
/// the `Option` half, so the row must record the decline the same way its global-helpers sub-item
/// does, plus the measured `Result` caveat `??` does not cover.
#[test]
fn w8_19_option_result_sub_item_is_declined() {
    let gaps = read("docs/gaps.md");
    let row = gaps
        .lines()
        .find(|l| l.starts_with("| **W8-19** |"))
        .unwrap_or_else(|| panic!("docs/gaps.md: no unstruck `| **W8-19** |` row found"));
    assert!(
        row.contains("DECLINED"),
        "W8-19 row must record the Option/Result methods sub-item as DECLINED"
    );
    assert!(
        row.contains("'??' applies to an Option"),
        "W8-19 row must record the measured Result caveat: '??' applies to an Option"
    );
}
