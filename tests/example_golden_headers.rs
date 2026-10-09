//! TICKET-233: a golden whose spawned prints are not ordered by a channel hand-off must declare
//! `# golden: unordered`. Both examples below send on an unbounded channel, so no hand-off orders
//! the two tasks' prints, and both diverged from their `.expected` on the CLI (measured 2026-10-08,
//! release, 40 runs each: `channel_block.chz` 3 at the default worker count and 2 at
//! `CHEZZI_THREADS=2`; `parallel_cross_nursery_ok.chz` 2 at the default worker count).
use std::path::Path;

/// Whether `examples/<name>` declares `# golden: unordered` in its leading comment block, read the
/// way `tests/examples_golden.rs` reads it.
fn declares_unordered(name: &str) -> bool {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name);
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    src.lines()
        .take_while(|l| l.starts_with('#'))
        .any(|l| l.starts_with("# golden: unordered"))
}

#[test]
fn channel_block_declares_its_unforced_print_order_unordered() {
    assert!(
        declares_unordered("channel_block.chz"),
        "channel_block.chz header lacks `# golden: unordered` but its spawned prints race"
    );
}

#[test]
fn parallel_cross_nursery_ok_declares_its_unforced_print_order_unordered() {
    assert!(
        declares_unordered("parallel_cross_nursery_ok.chz"),
        "parallel_cross_nursery_ok.chz header lacks `# golden: unordered` but its spawned prints race"
    );
}
