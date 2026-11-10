//! AST: the names of the binders the compiler writes for a carrier (`?.`, `??`).

/// THE one owner of the binders the compiler writes for a carrier (`?.`, `??`): their names and
/// the test "is this name one of them". A maker calls [`CarrierTemp::name`], a reader
/// [`CarrierTemp::of`]; neither spells a prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierTemp {
    /// The payload binder of a desugared carrier `match` (`$opt{id}`).
    Payload,
    /// The checker's pre-typed stand-in for a carrier's operand (`$optrecv{id}`).
    Operand,
}

impl CarrierTemp {
    /// Every kind of carrier binder.
    const ALL: [CarrierTemp; 2] = [CarrierTemp::Operand, CarrierTemp::Payload];

    /// The spelling every binder of this kind starts with (a test asks a message for it).
    pub fn prefix(self) -> &'static str {
        match self {
            CarrierTemp::Payload => "$opt",
            CarrierTemp::Operand => "$optrecv",
        }
    }

    /// The binder of this kind for the carrier whose node is `id`. The number is the carrier's
    /// node id, so the checker and the compiler name one carrier alike with no counter. The
    /// spelling starts with `$`, which does not lex, so no user binder can take it.
    pub fn name(self, id: crate::ast::NodeId) -> String {
        format!("{}{}", self.prefix(), id.0)
    }

    /// The kind of carrier binder `name` is, or `None` for any other name. Only the exact shape
    /// [`CarrierTemp::name`] builds (prefix + digits) is one: a user's `$options` is not.
    pub fn of(name: &str) -> Option<CarrierTemp> {
        Self::ALL.into_iter().find(|k| {
            name.strip_prefix(k.prefix())
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the exact shape [`CarrierTemp::name`] builds (prefix + digits) is a compiler temp.
    #[test]
    fn carrier_temp_of_matches_only_built_names() {
        let rows = [
            ("$opt12", Some(CarrierTemp::Payload)),
            ("$optrecv3", Some(CarrierTemp::Operand)),
            ("__opt12", None),
            ("__optrecv3", None),
            ("$options", None),
            ("$opt", None),
            ("$optrecv", None),
            ("$optrecvx", None),
            ("x", None),
        ];
        for (name, want) in rows {
            assert_eq!(CarrierTemp::of(name), want, "{name}");
        }
        for k in CarrierTemp::ALL {
            assert_eq!(CarrierTemp::of(&k.name(crate::ast::NodeId(7))), Some(k));
        }
    }
}
