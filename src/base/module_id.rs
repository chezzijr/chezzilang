//! base: a module's identity and one resolved import.

use crate::ast::{Import, Span};
use std::path::PathBuf;

/// A module's stable identity: its canonicalized absolute path. De-dupes diamond imports and never
/// aliases `std.io` with a same-named local file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleId(pub PathBuf);

/// One import statement resolved to a target module.
#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub target: ModuleId,
    pub import: Import,
    pub span: Span,
}
