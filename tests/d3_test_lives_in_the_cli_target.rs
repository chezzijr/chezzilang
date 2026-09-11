//! TICKET-114 (W12-23): source-text rule -- the 10k-CPU-fiber D3 soundness test must live in its own
//! process (`tests/d3_thousands_of_cpu_fibers_cli.rs`, driving the built binary at a fixed
//! `CHEZZI_THREADS`), never in the lib target at the default pool. At the default pool the debug
//! build hangs to its 60 s bound whenever the box has other load (measured in `## Digest`).
//! `vm::pool` is one process-wide `OnceLock` (DEC-095), so a lib test cannot size its own pool.
//!
//! Red on base: `src/vm/tests.rs` still holds `fn d3_thousands_of_cpu_fibers_all_complete`. Green
//! once the fix moves it out (plan step 8) and the CLI test exists with a fixed `CHEZZI_THREADS`.

use std::fs;

#[test]
fn d3_test_lives_in_the_cli_target() {
    let vm_tests = fs::read_to_string("src/vm/tests.rs").expect("read src/vm/tests.rs");
    assert!(
        !vm_tests.contains("fn d3_thousands_of_cpu_fibers_all_complete"),
        "src/vm/tests.rs still defines d3_thousands_of_cpu_fibers_all_complete in the lib target; \
         it must move to tests/d3_thousands_of_cpu_fibers_cli.rs (TICKET-114, W12-23)"
    );

    let cli_test = fs::read_to_string("tests/d3_thousands_of_cpu_fibers_cli.rs")
        .expect("read tests/d3_thousands_of_cpu_fibers_cli.rs");
    assert!(
        cli_test.contains("fn d3_thousands_of_cpu_fibers_all_complete"),
        "tests/d3_thousands_of_cpu_fibers_cli.rs is missing d3_thousands_of_cpu_fibers_all_complete"
    );
    assert!(
        cli_test.contains("CHEZZI_THREADS"),
        "tests/d3_thousands_of_cpu_fibers_cli.rs must drive the built binary at a fixed CHEZZI_THREADS"
    );
}
