//! Two gates on how this repo's tests measure a cost: no `tests/chz` test may divide two
//! wall-clock samples, and no NEW Rust `#[test]` may read a wall clock without saying so.
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
const CLOCK_READING_TESTS: [&str; 28] = [
    "a_chezzi_hang_python_survives_is_a_finding",
    "a_cyclic_shared_field_type_graph_is_also_walked_once_per_type",
    "a_shared_field_type_graph_is_walked_once_per_type",
    "a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault",
    "a_slow_but_healthy_job_at_the_exit_drain_is_untouched",
    "a_top_level_wait_timer_arm_loses_to_an_eager_job",
    "a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value",
    "an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout",
    "an_eager_wait_timer_arm_loses_to_a_sibling_value",
    "connect_inside_an_executor_job_errs_instead_of_pinning_a_pool_worker",
    "d5_blocking_sleeps_run_concurrently_not_serialized",
    "d5_owe3_path_c_sleep_in_callback_demotes_frees_worker",
    "deadline_past_fires_immediately",
    "fibers_scale_ready_queue_not_quadratic",
    "parallel_many_spawns_cheap_and_correct",
    "parity_blocking_native_is_an_entry_cancellation_checkpoint_on_both_engines",
    "polymorphic_recursion_is_refused_in_bounded_time_by_growth_detection",
    "polymorphic_recursion_through_a_func_type_argument_is_refused_in_bounded_time",
    "rwshared_view_over_shared_bindings_is_not_quadratic",
    "threads_one_serializes_cpu_bound_parallel_tasks",
    "threads_one_serializes_nested_eager_parallel_tasks",
    "ticket_016_bounded_update_guard_acquire_yields_instead_of_blocking",
    "timeout_aborts_a_joiner_whose_job_has_no_checkpoint",
    "timeout_aborts_a_netpoller_parked_test",
    "timeout_aborts_a_sleeping_test_everywhere",
    "timer_fires_after_its_deadline",
    "timer_many_all_fire_on_one_thread",
    "unique_is_not_quadratic",
];

/// The name of every `#[test]` fn under `src` and `tests` whose body holds a `.elapsed()` line.
///
/// A body runs from the `fn` line to the first line that is exactly that `fn`'s own indentation
/// followed by `}`. rustfmt guarantees that line closes the fn, and unlike brace counting the rule
/// is immune to a brace inside a string literal: measured 2026-09-03, brace counting attributed a
/// clock to `interpolation_multiline_reports_the_fragments_real_line` (which reads none) and
/// missed `ticket_016_bounded_update_guard_acquire_yields_instead_of_blocking` (which reads one).
fn clock_reading_tests() -> BTreeSet<String> {
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
            let mut reads_clock = false;
            while end < lines.len() && lines[end] != close {
                reads_clock = reads_clock || lines[end].contains(".elapsed()");
                end += 1;
            }
            if reads_clock {
                found.insert(name);
            }
            i = end + 1;
        }
    }
    found
}

/// TICKET-050 named twelve tests it deliberately left on `CLOCK_READING_TESTS` for a follow-up
/// (TICKET-059) to convert. This pins that follow-up: red while any of the twelve is still listed,
/// green once each is converted and its name removed.
const TICKET_050_DEFERRED_TESTS: [&str; 12] = [
    "polymorphic_recursion_is_refused_in_bounded_time_by_growth_detection",
    "polymorphic_recursion_through_a_func_type_argument_is_refused_in_bounded_time",
    "over_memory_counts_jobs_queued_but_not_started",
    "connect_inside_an_executor_job_errs_instead_of_pinning_a_pool_worker",
    "stack_trace_reports_call_chain_on_both_engines",
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
