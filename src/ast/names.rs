//! AST: the spelling facts of a name -- the dotted spelling of a path and the private (`$`) names.

use super::{Expr, ExprKind};

/// The dotted spelling of a Field chain over an Ident head (`a.b.K` -> `"a.b.K"`); `None` for any
/// other shape, and for an Ident alone.
pub fn dotted_path(e: &Expr) -> Option<String> {
    fn spelling(e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Ident(h) => Some(h.clone()),
            ExprKind::Field { obj, name, .. } => Some(format!("{}.{name}", spelling(obj)?)),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => None,
        }
    }
    match &e.kind {
        ExprKind::Field { .. } => spelling(e),
        ExprKind::Ident(_)
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Unary { .. }
        | ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Range { .. }
        | ExprKind::Call { .. }
        | ExprKind::Index { .. }
        | ExprKind::Try(_)
        | ExprKind::OptChain { .. }
        | ExprKind::NullCoalesce { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. }
        | ExprKind::Recover(..)
        | ExprKind::ElseGuard { .. } => None,
    }
}

/// The sigil of a private spelling; `$` does not lex.
const PRIVATE_SIGIL: char = '$';

/// The private spelling of a prelude name: `$` + `name`. No source can spell it (`$` does not lex),
/// so only code the compiler writes (desugar) reaches a name bound this way. In a user module the
/// carriers' variants are bound only so (`Checker::seed_carrier_enums_from`): `Some`, `Ok` and `Err`
/// are removed from the language, and a user may declare those names for their own use.
pub fn private_name(name: &str) -> String {
    format!("{PRIVATE_SIGIL}{name}")
}

/// The name a private spelling stands for (`$Option` → `Option`); any other name unchanged.
pub fn unprivate(name: &str) -> &str {
    name.strip_prefix(PRIVATE_SIGIL).unwrap_or(name)
}

/// Whether `name` is a private spelling (`$opt12`, `$Option`): only code the compiler writes
/// binds one.
pub fn is_private_name(name: &str) -> bool {
    name.starts_with(PRIVATE_SIGIL)
}
