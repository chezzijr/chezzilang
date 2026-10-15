//! Shared helpers for the lib's unit tests. The one owner of the fixture temp directory: a
//! single process-wide counter names every directory, so two live fixtures never share a path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh temp directory, removed with everything in it on drop.
pub(crate) struct TmpDir(pub(crate) PathBuf);

impl TmpDir {
    pub(crate) fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_unit_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }

    /// The path of `rel` inside the directory. Creates nothing.
    pub(crate) fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Write `contents` to `rel` inside the directory, creating parent directories.
    pub(crate) fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
