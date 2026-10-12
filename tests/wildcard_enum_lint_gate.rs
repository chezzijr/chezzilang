//! TICKET-251: a `_ =>` arm on a language enum gives a new variant the old behaviour silently.
//! The lint `clippy::wildcard_enum_match_arm` must be denied in ONE place, `Cargo.toml`.

use std::fs;

#[test]
fn wildcard_enum_match_arm_is_denied_in_cargo_toml() {
    let toml = fs::read_to_string("Cargo.toml").expect("read Cargo.toml");
    let denied = toml.lines().any(|l| {
        let l = l.trim();
        l.starts_with("wildcard_enum_match_arm")
            && (l.contains("\"deny\"") || l.contains("\"forbid\""))
    });
    assert!(
        denied,
        "Cargo.toml [lints.clippy] does not deny wildcard_enum_match_arm"
    );
}
