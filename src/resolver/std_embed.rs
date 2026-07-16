//! The stdlib, baked into the binary.
//!
//! `std/*.chz` is BUILD-TIME source, not an installed asset: an installed `chezzi` (`cargo install
//! --path .`) must not depend on the source checkout still existing at `env!("CARGO_MANIFEST_DIR")`.
//! So every `std/**/*.chz` is `include_str!`'d here — the same pattern the CLI already uses for the
//! `docs/*.md` topics — and [`lookup`] is the fallback source the resolver reads a `std.*` module
//! from when `$CHEZZI_STD` is unset. See [`super::std_source`] for the priority chain.
//!
//! The table is hand-written and would silently rot when a new `std/foo.chz` lands (an absent entry
//! is invisible in a dev/CI run, where the checkout is right there on disk) — `embedded_std_table_matches_disk`
//! below is the guard: it walks the on-disk tree and asserts set-equality AND content-equality.

/// Every `std/**/*.chz`, keyed by its path relative to `std/` (forward slashes, `.chz` included).
pub const STD_FILES: &[(&str, &str)] = &[
    ("bisect.chz", include_str!("../../std/bisect.chz")),
    ("cancel.chz", include_str!("../../std/cancel.chz")),
    ("cmp.chz", include_str!("../../std/cmp.chz")),
    ("collections.chz", include_str!("../../std/collections.chz")),
    ("concurrency.chz", include_str!("../../std/concurrency.chz")),
    (
        "concurrency/collection.chz",
        include_str!("../../std/concurrency/collection.chz"),
    ),
    ("crypto.chz", include_str!("../../std/crypto.chz")),
    ("csv.chz", include_str!("../../std/csv.chz")),
    ("datetime.chz", include_str!("../../std/datetime.chz")),
    ("encoding.chz", include_str!("../../std/encoding.chz")),
    ("ffi.chz", include_str!("../../std/ffi.chz")),
    ("flag.chz", include_str!("../../std/flag.chz")),
    ("fs.chz", include_str!("../../std/fs.chz")),
    ("io.chz", include_str!("../../std/io.chz")),
    ("iter.chz", include_str!("../../std/iter.chz")),
    ("json.chz", include_str!("../../std/json.chz")),
    ("log.chz", include_str!("../../std/log.chz")),
    ("math.chz", include_str!("../../std/math.chz")),
    ("memoize.chz", include_str!("../../std/memoize.chz")),
    ("net.chz", include_str!("../../std/net.chz")),
    ("os.chz", include_str!("../../std/os.chz")),
    ("path.chz", include_str!("../../std/path.chz")),
    ("prelude.chz", include_str!("../../std/prelude.chz")),
    ("process.chz", include_str!("../../std/process.chz")),
    ("rand.chz", include_str!("../../std/rand.chz")),
    ("ref.chz", include_str!("../../std/ref.chz")),
    ("regex.chz", include_str!("../../std/regex.chz")),
    ("request.chz", include_str!("../../std/request.chz")),
    ("string.chz", include_str!("../../std/string.chz")),
    ("time.chz", include_str!("../../std/time.chz")),
    ("uuid.chz", include_str!("../../std/uuid.chz")),
];

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

    /// ANTI-ROT GUARD. The `include_str!` table is hand-written: a new `std/foo.chz` that nobody adds
    /// here is absent from every INSTALLED binary while dev/CI stays green (the checkout is on disk).
    /// Assert the embedded key set == the on-disk `**/*.chz` set, and that each embedded string is the
    /// live file content.
    #[test]
    fn embedded_std_table_matches_disk() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("std");
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
            "src/resolver/std_embed.rs::STD_FILES drifted from the std/ tree — add/remove the \
             include_str! entry, or an installed chezzi ships a stdlib missing that module"
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
}
