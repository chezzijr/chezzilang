//! The checker's internal type lattice (`Ty`) — distinct from the AST's `Type` annotation node.
//!
//! Pragmatic, no unification: `list`/`Result`/`Option` carry exactly one inner type, and
//! [`Ty::Unknown`] is a top/bottom element that is compatible with everything so a single error
//! doesn't cascade into a storm of follow-on errors.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Nil,
    List(Box<Ty>),
    Func { params: Vec<Ty>, ret: Box<Ty> },
    Struct(String),
    Enum(String),
    Result(Box<Ty>),
    Option(Box<Ty>),
    /// Un-inferable, or "an error was already reported here". Compatible with everything.
    Unknown,
}

impl Ty {
    pub fn list(inner: Ty) -> Ty {
        Ty::List(Box::new(inner))
    }
    pub fn result(inner: Ty) -> Ty {
        Ty::Result(Box::new(inner))
    }
    pub fn option(inner: Ty) -> Ty {
        Ty::Option(Box::new(inner))
    }

    /// Is this a number (`int` or `float`)?
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
    }
}

/// Structural compatibility for assignment / argument passing. [`Ty::Unknown`] on either side
/// (at any depth) matches anything, which is what keeps one error from cascading.
pub fn compatible(expected: &Ty, actual: &Ty) -> bool {
    use Ty::*;
    match (expected, actual) {
        (Unknown, _) | (_, Unknown) => true,
        (Int, Int) | (Float, Float) | (Bool, Bool) | (Str, Str) | (Nil, Nil) => true,
        (List(a), List(b)) | (Result(a), Result(b)) | (Option(a), Option(b)) => compatible(a, b),
        (Struct(a), Struct(b)) | (Enum(a), Enum(b)) => a == b,
        (Func { params: p1, ret: r1 }, Func { params: p2, ret: r2 }) => {
            p1.len() == p2.len()
                && p1.iter().zip(p2).all(|(a, b)| compatible(a, b))
                && compatible(r1, r2)
        }
        _ => false,
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::Nil => write!(f, "nil"),
            Ty::List(t) => write!(f, "list[{t}]"),
            Ty::Result(t) => write!(f, "Result[{t}]"),
            Ty::Option(t) => write!(f, "Option[{t}]"),
            Ty::Struct(n) | Ty::Enum(n) => write!(f, "{n}"),
            Ty::Func { params, ret } => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}")
            }
            Ty::Unknown => write!(f, "?"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_compatible_with_anything() {
        assert!(compatible(&Ty::Unknown, &Ty::Int));
        assert!(compatible(&Ty::Int, &Ty::Unknown));
        // and at depth: Result[?] accepts Result[int]
        assert!(compatible(&Ty::result(Ty::Unknown), &Ty::result(Ty::Int)));
    }

    #[test]
    fn primitives_must_match_exactly() {
        assert!(compatible(&Ty::Int, &Ty::Int));
        assert!(!compatible(&Ty::Int, &Ty::Float)); // no implicit int->float
        assert!(!compatible(&Ty::Str, &Ty::Int));
    }

    #[test]
    fn nominal_types_compare_by_name() {
        assert!(compatible(&Ty::Struct("Point".into()), &Ty::Struct("Point".into())));
        assert!(!compatible(&Ty::Struct("Point".into()), &Ty::Struct("Vec".into())));
        assert!(!compatible(&Ty::Struct("Point".into()), &Ty::Enum("Point".into())));
    }

    #[test]
    fn display_renders_source_forms() {
        assert_eq!(Ty::list(Ty::Int).to_string(), "list[int]");
        assert_eq!(Ty::result(Ty::Int).to_string(), "Result[int]");
        assert_eq!(Ty::Struct("Point".into()).to_string(), "Point");
        assert_eq!(
            Ty::Func { params: vec![Ty::Int, Ty::Str], ret: Box::new(Ty::Bool) }.to_string(),
            "fn(int, str) -> bool"
        );
    }
}
