//! M1.8 — a value that a blocking door holds in a Rust local survives a collection on another
//! thread. `tests/chz/spec/gc_blocking_pins_test.chz` has one test per door kind; this runs it with
//! `CHEZZI_GC_STRESS=1` (every instruction boundary requests a collection) at 2 workers, unseeded
//! and at 5 scheduler seeds. A value the door did not pin is freed by another thread's sweep, and
//! its next read panics `dangling GcRef` or reads a reused slot.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;
#[path = "support/stressed_chz.rs"]
mod stressed_chz;

fn assert_seed(seed: &str) {
    stressed_chz::assert_stressed_pass(
        "tests/chz/spec/gc_blocking_pins_test.chz",
        "2",
        seed.strip_prefix('s'),
    );
}

grid_tests!(SEEDS in seed: [unseeded, s1, s2, s3, s4, s5] => crate::assert_seed);
