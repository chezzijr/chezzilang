//! TICKET-276: the test time budget. No `.config/nextest.toml` override may raise a test's cap.

use std::fs;

#[test]
fn nextest_config_raises_no_timeout_override() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.config/nextest.toml");
    let text = fs::read_to_string(path).expect("read .config/nextest.toml");
    let raised: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains("slow-timeout"))
        .filter(|l| !l.contains("terminate-after = 5 }"))
        .collect();
    assert!(
        raised.is_empty(),
        "a raised timeout override hides a test over budget: {raised:?}"
    );
}
