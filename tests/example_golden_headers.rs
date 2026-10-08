//! TICKET-233: a golden whose spawned prints are not ordered by a channel hand-off must declare
//! `# golden: unordered`; `examples/channel_block.chz` printed in a different order in 4/40 CLI runs.
use std::path::Path;

#[test]
fn channel_block_declares_its_unforced_print_order_unordered() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/channel_block.chz");
    let src = std::fs::read_to_string(path).expect("read channel_block.chz");
    let unordered = src
        .lines()
        .take_while(|l| l.starts_with('#'))
        .any(|l| l.starts_with("# golden: unordered"));
    assert!(
        unordered,
        "channel_block.chz header lacks `# golden: unordered` but its spawned prints race"
    );
}
