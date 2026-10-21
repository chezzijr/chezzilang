//! The stdlib, baked into the binary.
//!
//! `std/*.chz` is BUILD-TIME source, not an installed asset: an installed `chezzi` (`cargo install
//! --path .`) must not depend on the source checkout still existing at [`checkout_root`].
//! So every `std/**/*.chz` is `include_str!`'d here — the same pattern the CLI already uses for the
//! `docs/*.md` topics — and [`lookup`] is the fallback source the resolver reads a `std.*` module
//! from when `$CHEZZI_STD` is unset. See [`super::std_source`] for the priority chain.
//!
//! The table is not hand-written: `crates/chezzi-native/build.rs` walks `std/` at build time, so a
//! new or edited `std/foo.chz` reaches the next `cargo build` with no manual step. The same script
//! parses each file with `parser::parse_module_source` and embeds the encoded AST ([`decode_ast`]),
//! so a run decodes a std module instead of lexing and parsing its text.
//! `embedded_std_table_matches_disk` still walks the on-disk tree and asserts set-equality AND
//! content-equality; `every_std_artifact_equals_a_parse_of_its_text` fails a stale artifact.

/// The source checkout this binary was built from: the workspace root, two levels above this
/// crate (`crates/chezzi-native`). The std directory PATH (`resolver::std_root`) and every test
/// fixture path derive from it; a `std.*` module's TEXT comes from [`STD_FILES`] instead.
pub fn checkout_root() -> std::path::PathBuf {
    let mut root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    root
}

// `STD_FILES`, `STD_ASTS` and `STD_PROTOCOLS`, written by `crates/chezzi-native/build.rs` from `std/`.
include!(concat!(env!("OUT_DIR"), "/std_files.rs"));

/// The decoded AST and identifier words of `STD_FILES[index]`, or `None` when the file did not parse
/// at build time. Every call decodes anew, with node ids this process has not used.
pub fn decode_ast(index: usize) -> Option<(crate::ast::Module, crate::lexer::IdentWords)> {
    crate::ast::codec::decode(STD_ASTS[index]?)
}

/// The embedded source of `rel` (e.g. `"math.chz"`, `"concurrency/collection.chz"`), if it exists.
pub fn lookup(rel: &str) -> Option<&'static str> {
    STD_FILES
        .iter()
        .find(|(k, _)| *k == rel)
        .map(|(_, src)| *src)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn collect_chz(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("std dir readable").flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_chz(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("chz") {
                out.push(p);
            }
        }
    }

    /// ANTI-ROT GUARD. The build script writes the table from `std/`, so a build that missed a new
    /// `std/foo.chz` would ship an INSTALLED binary without that module while dev/CI stays green
    /// (the checkout is on disk). Assert the embedded key set == the on-disk `**/*.chz` set, and
    /// that each embedded string is the live file content.
    #[test]
    fn embedded_std_table_matches_disk() {
        let root = checkout_root().join("std");
        let mut files = Vec::new();
        collect_chz(&root, &mut files);

        let mut on_disk: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        on_disk.sort();

        let mut embedded: Vec<String> = STD_FILES.iter().map(|(k, _)| k.to_string()).collect();
        embedded.sort();

        assert_eq!(
            embedded, on_disk,
            "STD_FILES drifted from the std/ tree — the build script missed a file, so an \
             installed chezzi ships a stdlib missing that module"
        );

        for rel in &on_disk {
            let disk = std::fs::read_to_string(root.join(rel)).unwrap();
            assert_eq!(
                lookup(rel).unwrap_or_else(|| panic!("no embedded entry for {rel}")),
                disk,
                "embedded {rel} differs from the file on disk"
            );
        }
    }

    /// A stale or mis-shifted artifact fails here: each decoded AST equals a parse of its text.
    #[test]
    fn every_std_artifact_equals_a_parse_of_its_text() {
        for (index, (rel, src)) in STD_FILES.iter().enumerate() {
            let decoded = decode_ast(index).unwrap_or_else(|| panic!("std/{rel}: no artifact"));
            let parsed =
                crate::parser::parse_module_source(src, index as u32 + 1, &|_: &str| false)
                    .unwrap_or_else(|_| panic!("std/{rel} does not parse"));
            assert!(
                decoded == parsed,
                "std/{rel}: artifact differs from a parse of its text"
            );
        }
    }

    /// Every id of a decoded module moves, binding targets included (`ids_in_block` skips those).
    #[test]
    fn two_decodes_of_a_std_artifact_share_no_node_id() {
        fn ids(m: &crate::ast::Module) -> std::collections::HashSet<u32> {
            let text = format!("{m:?}");
            let mut out = std::collections::HashSet::new();
            for part in text.split("NodeId(").skip(1) {
                let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
                let n: u32 = digits.parse().expect("a NodeId prints its number");
                if n != 0 {
                    out.insert(n);
                }
            }
            out
        }
        let index = STD_FILES
            .iter()
            .position(|(k, _)| *k == "prelude.chz")
            .expect("prelude.chz is embedded");
        let (first, _) = decode_ast(index).expect("prelude decodes");
        let (second, _) = decode_ast(index).expect("prelude decodes again");
        let (a, b) = (ids(&first), ids(&second));
        assert!(!a.is_empty(), "the prelude has node ids");
        let shared: Vec<_> = a.intersection(&b).collect();
        assert!(shared.is_empty(), "two decodes share node ids: {shared:?}");
    }
}
