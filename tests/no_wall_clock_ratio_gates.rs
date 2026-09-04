//! Six gates on how this repo's tests measure a cost: `no_chz_test_divides_two_wall_clock_samples`
//! and `no_rust_test_divides_two_wall_clock_samples` ban dividing two wall-clock samples (chz, then
//! Rust); `no_new_rust_test_reads_a_wall_clock` ratchets which Rust `#[test]`s may read a wall clock
//! at all; `no_new_rust_test_sleeps_to_order_two_events` and
//! `no_new_chz_test_sleeps_to_order_two_events` ratchet a sleep used as a happens-before edge (Rust,
//! then chz); `ticket_050_deferred_tests_have_left_the_wall_clock_allowlist` pins TICKET-050's
//! twelve-test follow-up.
//!
//! `tests/chz/spec/gc_core_graph_test.chz` asserted `deep / shallow < 2.8` on two ~10 ms
//! `time.monotonic()` samples. The noise in the two samples is uncorrelated and the smaller one is
//! the denominator, so CPU load amplifies the quotient without bound: 3 red runs in 25 under 32-way
//! oversubscription, worst `got 8.157102984533584 from 10.457949ms -> 85.306567ms`, and it reddened
//! the whole `cargo test` gate twice per run because `tests/chz` runs at two worker counts
//! (TICKET-049). A cost that must be pinned gets counted, not timed; a wall-clock bound with real
//! headroom is still fine, which is why the rule keys on the division, not on the clock.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Every file under `dir` whose extension is `ext`, sorted, so a failure names the same file every
/// run.
fn files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            files_with_ext(&p, ext, out);
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
}

#[test]
fn no_chz_test_divides_two_wall_clock_samples() {
    let mut files = Vec::new();
    files_with_ext(Path::new("tests/chz"), "chz", &mut files);
    assert!(
        !files.is_empty(),
        "tests/chz holds no .chz files -- the scan is looking in the wrong place"
    );
    let mut bad = Vec::new();
    for path in &files {
        let text =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if !text.contains("time.monotonic()") {
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if !code.starts_with('#') && code.contains('/') {
                bad.push(format!("{}:{}: {}", path.display(), i + 1, code.trim_end()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a tests/chz test that samples time.monotonic() must not divide -- a ratio of two \
         wall-clock samples amplifies scheduler noise without bound (TICKET-049). Count the work \
         in a Rust test, or assert a bound with headroom:\n{}",
        bad.join("\n")
    );
}

/// The Rust `#[test]`s that read a wall clock today, sorted.
///
/// TICKET-049 kept a wall-clock bound with real headroom legal, so this is a RATCHET, not a ban.
/// A name may join the list, but only deliberately, in the commit that adds the clock, with the
/// reason in that commit message. A test converted to a counted measure must be DELETED from the
/// list in the same commit that converts it.
const CLOCK_READING_TESTS: [&str; 19] = [
    "a_chezzi_hang_python_survives_is_a_finding",
    "a_cyclic_shared_field_type_graph_is_also_walked_once_per_type",
    "a_shared_field_type_graph_is_walked_once_per_type",
    "a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault",
    "a_slow_but_healthy_job_at_the_exit_drain_is_untouched",
    "d5_blocking_sleeps_run_concurrently_not_serialized",
    "d5_owe3_path_c_sleep_in_callback_demotes_frees_worker",
    "deadline_past_fires_immediately",
    "fibers_scale_ready_queue_not_quadratic",
    "parallel_many_spawns_cheap_and_correct",
    "parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines",
    "rwshared_view_over_shared_bindings_is_not_quadratic",
    // threads_one_serializes_cpu_bound_parallel_tasks / _nested_eager_parallel_tasks (TICKET-059):
    // both now read the clock in tests/support/child_rusage.rs, not in their own bodies -- same
    // precedent as many_idle_workers_do_not_thundering_herd_on_yield, never listed here.
    "ticket_016_bounded_update_guard_acquire_yields_instead_of_blocking",
    "timeout_aborts_a_joiner_whose_job_has_no_checkpoint",
    "timeout_aborts_a_netpoller_parked_test",
    "timeout_aborts_a_sleeping_test_everywhere",
    "timer_fires_after_its_deadline",
    "timer_many_all_fire_on_one_thread",
    "unique_is_not_quadratic",
];

/// This file's own name, for the whole-file scans below to skip. Every gate in this file holds the
/// thing it bans as a string literal, so a scan wide enough to be useful reaches its own source
/// (measured 2026-09-05: `SELF-FLAG: div_duration_f64( present` and a self-flag on
/// `NON_WALL_CLOCK_DURATION_RATIOS`'s own entry). Nothing in this file times anything, so skipping it
/// by name costs nothing real (TICKET-059).
const GATE_FILE: &str = "no_wall_clock_ratio_gates.rs";

/// A `.rs` file : line-text pair that divides two wall-clock-shaped `Duration` accessors but is NOT a
/// wall-clock ratio -- `tests/chezzi_threads_sys_time.rs`'s `sys.as_secs_f64() / user.as_secs_f64()`
/// divides two CPU-TIME samples taken from ONE `wait4` rusage call, so a busy box slows numerator and
/// denominator identically and cannot amplify the quotient the way two wall-clock samples can
/// (TICKET-049's own escape hatch: "A future timing test that genuinely needs a division must change
/// this test and say why in the same commit"). Matched by the expression's TEXT, not a line number, so
/// the entry survives an edit above it and breaks if the expression itself changes.
const NON_WALL_CLOCK_DURATION_RATIOS: [(&str, &str); 1] = [(
    "tests/chezzi_threads_sys_time.rs",
    "let ratio = sys.as_secs_f64() / user.as_secs_f64();",
)];

/// Every `path:line: text` under `src/` and `tests/` that divides two `Duration`-shaped wall-clock
/// samples -- the RUST half of TICKET-049's ratio ban (the original scanned only `tests/chz`).
///
/// WHOLE-FILE, not `#[test]`-body: `tests/chezzi_threads_sys_time.rs` keeps its clock read inside a
/// helper fn (`child_rusage::run_timed`), so a body-only scan would miss it. Joins each file's
/// non-comment lines with a space and collapses whitespace runs to one, so a division split across two
/// source lines is still caught. Flags a `/` (never `//`) that has one of `as_secs_f64()`,
/// `as_secs_f32()`, `as_millis()`, `as_micros()`, `as_nanos()` within 60 characters on BOTH sides, or a
/// file that calls `div_duration_f64(`; a flag is exempt when the file's
/// [`NON_WALL_CLOCK_DURATION_RATIOS`] entry appears inside its 121-character window.
fn wall_clock_ratio_lines() -> Vec<String> {
    let mut files = Vec::new();
    files_with_ext(Path::new("src"), "rs", &mut files);
    files_with_ext(Path::new("tests"), "rs", &mut files);
    let accessors = [
        "as_secs_f64()",
        "as_secs_f32()",
        "as_millis()",
        "as_micros()",
        "as_nanos()",
    ];
    let mut bad = Vec::new();
    for path in &files {
        if path.file_name().is_some_and(|n| n == GATE_FILE) {
            continue;
        }
        let text =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let display = path.display().to_string();
        let allow = NON_WALL_CLOCK_DURATION_RATIOS
            .iter()
            .find(|(f, _)| display.ends_with(f))
            .map(|(_, expr)| *expr);
        let joined: String = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//"))
            .collect::<Vec<_>>()
            .join(" ");
        let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
        let chars: Vec<char> = collapsed.chars().collect();
        for i in 0..chars.len() {
            if chars[i] != '/' || chars.get(i + 1) == Some(&'/') {
                continue;
            }
            let start = i.saturating_sub(60);
            let end = (i + 60).min(chars.len());
            let before: String = chars[start..i].iter().collect();
            let after: String = chars[i..end].iter().collect();
            let window: String = chars[start..end].iter().collect();
            if let Some(expr) = allow
                && window.contains(expr)
            {
                continue;
            }
            if accessors.iter().any(|a| before.contains(a))
                && accessors.iter().any(|a| after.contains(a))
            {
                bad.push(format!("{display}: {window}"));
            }
        }
        if collapsed.contains("div_duration_f64(") && allow != Some("div_duration_f64(") {
            bad.push(format!("{display}: div_duration_f64( present"));
        }
    }
    bad
}

#[test]
fn no_rust_test_divides_two_wall_clock_samples() {
    let bad = wall_clock_ratio_lines();
    assert!(
        bad.is_empty(),
        "a Rust test that divides two wall-clock samples amplifies scheduler noise without bound, \
         the same way TICKET-049 banned it in tests/chz -- count the work instead, or assert a bound \
         with headroom on one sample. If the division is over two CPU-time samples from a single \
         rusage call (immune to that amplification), add it to NON_WALL_CLOCK_DURATION_RATIOS and say \
         why in the same commit:\n{}",
        bad.join("\n")
    );
}

/// The name of every `#[test]` fn under `src` and `tests` whose body contains any of `needles`.
///
/// A body runs from the `fn` line to the first line that is exactly that `fn`'s own indentation
/// followed by `}`. rustfmt guarantees that line closes the fn, and unlike brace counting the rule
/// is immune to a brace inside a string literal: measured 2026-09-03, brace counting attributed a
/// clock to `interpolation_multiline_reports_the_fragments_real_line` (which reads none) and
/// missed `ticket_016_bounded_update_guard_acquire_yields_instead_of_blocking` (which reads one).
///
/// **Every caller must be a plain fn, never a `#[test]` itself** -- the walk reads every line from
/// the `fn` line to its closing brace, so a needle written directly in a `#[test]` body matches
/// that body. Measured 2026-09-05: putting all three needles below in their own `#[test]` bodies
/// made the gate name itself (`elapsed -> ['no_new_rust_test_reads_a_wall_clock']`, `sleeps ->
/// ['no_new_rust_test_sleeps_to_order_two_events', 'no_new_chz_test_sleeps_to_order_two_events']`),
/// while every needle kept in a non-`#[test]` wrapper (as below) reports `[]` for both.
fn tests_matching(needles: &[&str]) -> BTreeSet<String> {
    let mut files = Vec::new();
    files_with_ext(Path::new("src"), "rs", &mut files);
    files_with_ext(Path::new("tests"), "rs", &mut files);
    assert!(
        !files.is_empty(),
        "no .rs file under src/ or tests/ -- the scan is looking in the wrong place"
    );
    let mut found = BTreeSet::new();
    for path in &files {
        let text =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let lines: Vec<&str> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim() != "#[test]" {
                i += 1;
                continue;
            }
            let Some(decl) =
                (i + 1..lines.len()).find(|&j| lines[j].trim_start().starts_with("fn "))
            else {
                break;
            };
            let head = lines[decl];
            let sig = head.trim_start();
            let indent = &head[..head.len() - sig.len()];
            let close = format!("{indent}}}");
            let name: String = sig[3..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let mut end = decl + 1;
            let mut hit = false;
            while end < lines.len() && lines[end] != close {
                hit = hit || needles.iter().any(|n| lines[end].contains(n));
                end += 1;
            }
            if hit {
                found.insert(name);
            }
            i = end + 1;
        }
    }
    found
}

/// The name of every `#[test]` fn under `src` and `tests` whose body holds a `.elapsed()` line.
fn clock_reading_tests() -> BTreeSet<String> {
    tests_matching(&[".elapsed()"])
}

/// The name of every `#[test]` fn under `src` and `tests` whose body holds `thread::sleep(` or
/// `sleep_ms(` -- a sleep used to ORDER two events is the same defect class as a clock read (a
/// producer-reaches-a-point-before-a-consumer-looks-at-it guess about scheduling), reads no clock
/// and asserts no duration, so neither existing gate catches it (TICKET-059/TICKET-060). Never a
/// bare `sleep(`: `usleep(` in the FFI fixture at `tests/ffi_stored_callback.rs` would otherwise add
/// a false name.
fn sleep_synchronised_tests() -> BTreeSet<String> {
    tests_matching(&["thread::sleep(", "sleep_ms("])
}

/// A sleep used to order two events, in one of the 68 Rust `#[test]`s TICKET-059 ratcheted here.
///
/// This 68-name golden is NOT a hand count -- the previous plan pass wrote `67` by hand and the gate
/// rejected it (the walk returns 68). Obtain it by compiling once with an empty array and copying
/// the "sleeps but is not listed" list `no_new_rust_test_sleeps_to_order_two_events` reports,
/// verbatim, the same way `stack_trace_reports_call_chain`'s golden was obtained.
const SLEEP_SYNCHRONISED_TESTS: [&str; 68] = [
    "a_bailed_join_stops_its_jobs_from_starting_new_work",
    "a_cancelled_siblings_defer_runs_whole_on_both_engines",
    "a_finished_executor_job_lets_the_genuine_nursery_deadlock_fire",
    "a_finished_jobs_output_survives_a_timeout_bail",
    "a_job_submitted_to_mains_executor_survives_another_executors_shutdown_now_mn",
    "a_leaked_jobs_exit_aborts_whichever_test_is_running_and_the_run_continues",
    "a_live_timer_still_delivers_under_a_generous_timeout",
    "a_normally_completing_nursery_is_untouched_without_an_exit",
    "a_nursery_judge_re_asks_the_verdict_when_a_party_registers_later",
    "a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault",
    "a_sleeping_nursery_task_is_untouched_without_an_exit",
    "a_slow_but_healthy_job_at_the_exit_drain_is_untouched",
    "a_test_fn_that_exits_does_not_poison_later_blocking_tests",
    "a_top_level_wait_timer_arm_loses_to_an_eager_job",
    "a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value",
    "a_yielded_pool_thread_retires_instead_of_growing_the_pool",
    "abort_diagnoses_even_with_a_full_unread_stdout_pipe",
    "an_eager_wait_timer_arm_loses_to_a_sibling_value",
    "an_executor_jobs_send_wakes_a_task_parked_on_another_scheds_channel",
    "cancel_cascade_crosses_the_airlock",
    "cancel_trip_wakes_parked_wait_under_parallel",
    "connect_to_dead_port_reports_refused",
    "d5_blocking_sleeps_run_concurrently_not_serialized",
    "d5_owe3_path_c_accept_in_callback_demotes",
    "d5_owe3_path_c_recv_in_native_map_callback_demotes",
    "d5_owe3_path_c_sleep_in_callback_correct",
    "d5_owe3_path_c_sleep_in_callback_demotes_frees_worker",
    "d5_owe3_path_c_socket_read_in_callback_demotes",
    "d5_owe3_recv_in_iter_map_callback_parks",
    "deregister_reinjects_and_disarms",
    "drain_sched_reinjects_matching_and_disarms",
    "eager_job_os_exit_beats_a_blocked_nurserys_deadlock_verdict",
    "eager_job_os_exit_kills_a_recv_parked_nursery_task",
    "eager_job_os_exit_kills_a_sleeping_nursery_task",
    "eager_job_os_exit_kills_an_in_callback_sleep",
    "eager_job_os_exit_terminates_a_recv_blocked_main",
    "eager_job_os_exit_terminates_a_socket_blocked_main",
    "executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock",
    "gc_mark_walk_does_not_deadlock_on_a_cyclic_core_graph",
    "max_heap_byte_walk_does_not_deadlock_on_a_cyclic_core_graph",
    "native_time_now_is_int_monotonic_is_float",
    "nested_executor_job_is_cancelled_by_an_outer_shutdown_now_mn",
    "net_read_partial_timeout_then_clean_timeout_is_not_incomplete",
    "net_read_poll_once_mid_codepoint_errs_incomplete_not_timeout",
    "net_read_timeout_bounds_the_in_callback_demote_path",
    "net_read_timeout_bounds_whole_call_across_codepoint_parks",
    "net_write_timeout_when_buffer_full",
    "no_event_does_not_inject",
    "parity_a_blocking_defer_body_completes_when_the_task_is_cancelled",
    "parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines",
    "read_timeout_returns_err",
    "read_without_timeout_still_parks_forever",
    "reaps_idle_thread",
    "register_with_deadline_times_out_when_fd_never_ready",
    "respects_cap",
    "six_sleeping_jobs_overlap_at_one_worker",
    "socket_read_bytes_recovers_the_sticky_invalid_utf8_carry",
    "submit_to_a_nested_executor_after_a_graceful_outer_shutdown_runs_mn",
    "submit_to_an_executor_whose_creating_job_was_cancelled_faults_mn",
    "submit_to_mains_own_executor_after_an_unrelated_shutdown_now_runs_mn",
    "the_deadline_does_not_truncate_a_defer_whose_recv_can_complete",
    "ticket_016_cross_box_update_cycle_faults",
    "ticket_016_cross_task_set_racing_update_is_not_lost",
    "timeout_aborts_a_sleeping_test_everywhere",
    "vm_wait_in_native_callback_demotes_under_parallel",
    "vm_wait_timer_loses_to_send_in_native_callback_parallel",
    "w8_7_demoted_fiber_yield_after_demote_does_not_strand_replacement",
    "write_all_fd_delivers_through_a_full_nonblocking_fd",
];

#[test]
fn no_new_rust_test_sleeps_to_order_two_events() {
    let found = sleep_synchronised_tests();
    let allowed: BTreeSet<String> = SLEEP_SYNCHRONISED_TESTS
        .iter()
        .map(|s| String::from(*s))
        .collect();
    let added: Vec<&String> = found.difference(&allowed).collect();
    let removed: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "a sleep used as a happens-before edge is a guess about scheduling and must become a \
         channel receive, a counted probe, or a latch (TICKET-060, DEC-050); if it genuinely can't \
         be, list the test in SLEEP_SYNCHRONISED_TESTS in the same commit and say why. Converting a \
         listed test off a sleep means deleting its entry in the same commit.\nsleeps but is not \
         listed: {added:?}\nlisted but does not sleep: {removed:?}"
    );
}

/// The 5 `tests/chz` sites that keep a `time.sleep_ms` for a reason recorded on the entry, because
/// nothing else can express the wait: a parked rendezvous sender or a job that must be INSIDE the
/// sleep when cancelled has no Chezzi-visible predicate, and one entry asserts a window a value must
/// NOT change across, which is a legitimate clock use, not a happens-before guess (TICKET-059/060).
const CHZ_SLEEP_SITES: [(&str, &str); 5] = [
    (
        "tests/chz/spec/rendezvous_channel_test.chz",
        "time.sleep_ms(150)",
    ),
    ("tests/chz/stdlib/cancel_test.chz", "time.sleep_ms(240)"),
    (
        "tests/chz/stdlib/sleep_cancel_test.chz",
        "time.sleep_ms(3000)",
    ),
    (
        "tests/chz/stdlib/sleep_cancel_test.chz",
        "time.sleep_ms(200)",
    ),
    (
        "tests/chz/stdlib/sleep_cancel_test.chz",
        "time.sleep_ms(50)",
    ),
];

/// Every `path:line: text` under `tests/chz` that calls `time.sleep_ms(` and is not one of
/// [`CHZ_SLEEP_SITES`]. Skips a line whose trimmed form starts with `#` (a comment), the same way
/// [`no_chz_test_divides_two_wall_clock_samples`] does.
fn chz_sleep_sites() -> Vec<String> {
    let mut files = Vec::new();
    files_with_ext(Path::new("tests/chz"), "chz", &mut files);
    let mut found = Vec::new();
    for path in &files {
        let text =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let display = path.display().to_string();
        for (i, line) in text.lines().enumerate() {
            let code = line.trim();
            if code.starts_with('#') || !code.contains("time.sleep_ms(") {
                continue;
            }
            if CHZ_SLEEP_SITES
                .iter()
                .any(|(f, expr)| display.ends_with(f) && code.contains(expr))
            {
                continue;
            }
            found.push(format!("{display}:{}: {code}", i + 1));
        }
    }
    found
}

#[test]
fn no_new_chz_test_sleeps_to_order_two_events() {
    let bad = chz_sleep_sites();
    assert!(
        bad.is_empty(),
        "a tests/chz test that calls time.sleep_ms must not use it to order two events -- a sleep \
         used as a happens-before edge is a guess about scheduling (TICKET-060, DEC-050); use a \
         channel handshake instead, or list the site in CHZ_SLEEP_SITES in the same commit and say \
         why:\n{}",
        bad.join("\n")
    );
}

/// TICKET-050 named twelve tests it deliberately left on `CLOCK_READING_TESTS` for a follow-up
/// (TICKET-059) to convert. This pins that follow-up: red while any of the twelve is still listed,
/// green once each is converted and its name removed. `stack_trace_reports_call_chain` replaces
/// `stack_trace_reports_call_chain_on_both_engines` here because TICKET-059 step 9 renamed it.
const TICKET_050_DEFERRED_TESTS: [&str; 12] = [
    "polymorphic_recursion_is_refused_in_bounded_time_by_growth_detection",
    "polymorphic_recursion_through_a_func_type_argument_is_refused_in_bounded_time",
    "over_memory_counts_jobs_queued_but_not_started",
    "connect_inside_an_executor_job_errs_instead_of_pinning_a_pool_worker",
    "stack_trace_reports_call_chain",
    "a_top_level_wait_timer_arm_loses_to_an_eager_job",
    "a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value",
    "an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout",
    "an_eager_wait_timer_arm_loses_to_a_sibling_value",
    "d4e_wake_parked_workers_from_true_sleep",
    "eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed",
    "try_recv_drains_residue_after_blocking_recv_resumes",
];

#[test]
fn ticket_050_deferred_tests_have_left_the_wall_clock_allowlist() {
    let allowed: BTreeSet<String> = CLOCK_READING_TESTS
        .iter()
        .map(|s| String::from(*s))
        .collect();
    let still_listed: Vec<&str> = TICKET_050_DEFERRED_TESTS
        .iter()
        .copied()
        .filter(|name| allowed.contains(*name))
        .collect();
    assert!(
        still_listed.is_empty(),
        "TICKET-050 deferred these twelve tests to a follow-up (TICKET-059) instead of converting \
         them off a wall clock; they must be converted to a counted probe, a handshake, or an \
         event count and removed from CLOCK_READING_TESTS, not left on the allowlist \
         indefinitely.\nstill on CLOCK_READING_TESTS: {still_listed:?}"
    );
}

#[test]
fn no_new_rust_test_reads_a_wall_clock() {
    let found = clock_reading_tests();
    let allowed: BTreeSet<String> = CLOCK_READING_TESTS
        .iter()
        .map(|s| String::from(*s))
        .collect();
    let added: Vec<&String> = found.difference(&allowed).collect();
    let removed: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        added.is_empty() && removed.is_empty(),
        "a Rust #[test] that measures elapsed wall-clock time must be listed in \
         CLOCK_READING_TESTS -- under CPU contention such a bound measures the machine, not the \
         code (TICKET-050). Count the work instead; if a clock really is the only measure, add \
         the name to the list in the same commit and say why. Converting a listed test to a \
         counted measure means deleting its entry in the same commit.\nreads a clock but is not \
         listed: {added:?}\nlisted but reads no clock: {removed:?}"
    );
}
