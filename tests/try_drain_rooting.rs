//! M3.1b — a value that a `?` or a `return` carries out of a `parallel:` or a `recover:` survives
//! the drains before its delivery. `tests/chz/spec/try_drain_rooting_test.chz` has one test per
//! drain of the family; this runs it with `CHEZZI_GC_STRESS=1` at 1 and 2 workers. With the value
//! in a Rust local across a drain, a collection freed it: a `dangling GcRef` panic or a reused slot
//! (`cannot match on List`).

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;
#[path = "support/stressed_chz.rs"]
mod stressed_chz;

fn assert_threads(threads: &str) {
    stressed_chz::assert_stressed_pass(
        "tests/chz/spec/try_drain_rooting_test.chz",
        threads.trim_start_matches('t'),
        None,
    );
}

grid_tests!(THREADS in threads: [t1, t2] => crate::assert_threads);
