//! base: operator hooks. One HOOKS row per hook: its protocol and its operator. The method a hook
//! calls is its protocol's std decl's (`PROTOCOL_METHODS`, from `std/*.chz`).

use crate::native::std_embed::PROTOCOL_METHODS;

/// TICKET-250 — an operator hook: a method an operator calls for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hook {
    Iter,
    Next,
    Index,
    SetIndex,
    Slice,
    Str,
    Eq,
    Hash,
    Compare,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Contains,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

/// One row per hook. `Checker::hook_verdict` decides each row for a declared type; nothing else
/// decides hook-ness.
pub struct HookRow {
    pub hook: Hook,
    /// The reserved protocol whose requirement the method must satisfy.
    pub protocol: &'static str,
    /// The user-facing spelling of the operator that calls the hook.
    pub operator: &'static str,
    /// The operator accepts every value, hook or not (`print` falls back to the default display).
    pub total: bool,
}

pub const HOOKS: &[HookRow] = &[
    HookRow {
        hook: Hook::Iter,
        protocol: "Iterable",
        operator: "for",
        total: false,
    },
    HookRow {
        hook: Hook::Next,
        protocol: "Iterator",
        operator: "for",
        total: false,
    },
    HookRow {
        hook: Hook::Index,
        protocol: "Index",
        operator: "x[k]",
        total: false,
    },
    HookRow {
        hook: Hook::SetIndex,
        protocol: "IndexSet",
        operator: "x[k] = v",
        total: false,
    },
    HookRow {
        hook: Hook::Slice,
        protocol: "Slice",
        operator: "x[a..b]",
        total: false,
    },
    HookRow {
        hook: Hook::Str,
        protocol: "Stringable",
        operator: "print",
        total: true,
    },
    HookRow {
        hook: Hook::Eq,
        protocol: "Eq",
        operator: "==",
        total: false,
    },
    HookRow {
        hook: Hook::Hash,
        protocol: "Hashable",
        operator: "a Set or Map key",
        total: false,
    },
    HookRow {
        hook: Hook::Compare,
        protocol: "Comparable",
        operator: "<",
        total: false,
    },
    HookRow {
        hook: Hook::Add,
        protocol: "Add",
        operator: "+",
        total: false,
    },
    HookRow {
        hook: Hook::Sub,
        protocol: "Sub",
        operator: "-",
        total: false,
    },
    HookRow {
        hook: Hook::Mul,
        protocol: "Mul",
        operator: "*",
        total: false,
    },
    HookRow {
        hook: Hook::Div,
        protocol: "Div",
        operator: "/",
        total: false,
    },
    HookRow {
        hook: Hook::Mod,
        protocol: "Mod",
        operator: "%",
        total: false,
    },
    HookRow {
        hook: Hook::Neg,
        protocol: "Neg",
        operator: "unary -",
        total: false,
    },
    HookRow {
        hook: Hook::Contains,
        protocol: "Contains",
        operator: "in",
        total: false,
    },
    HookRow {
        hook: Hook::BitAnd,
        protocol: "BitAnd",
        operator: "&",
        total: false,
    },
    HookRow {
        hook: Hook::BitOr,
        protocol: "BitOr",
        operator: "|",
        total: false,
    },
    HookRow {
        hook: Hook::BitXor,
        protocol: "BitXor",
        operator: "^",
        total: false,
    },
    HookRow {
        hook: Hook::Shl,
        protocol: "Shl",
        operator: "<<",
        total: false,
    },
    HookRow {
        hook: Hook::Shr,
        protocol: "Shr",
        operator: ">>",
        total: false,
    },
];

/// `a == b`, in a const context.
const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// The one method `protocol`'s std decl declares itself. The build fails when the protocol has no
/// std decl or declares more or fewer than one own method (`IndexSet` declares `set_index` and
/// embeds `Index`).
const fn own_method(protocol: &str) -> &'static str {
    let mut found = "";
    let mut count = 0;
    let mut j = 0;
    while j < PROTOCOL_METHODS.len() {
        if str_eq(PROTOCOL_METHODS[j].0, protocol) {
            found = PROTOCOL_METHODS[j].1;
            count += 1;
        }
        j += 1;
    }
    assert!(
        count == 1,
        "a protocol with a hook or an intrinsic method declares exactly one own method in its std decl"
    );
    found
}

/// The method of each `HOOKS` row, in row order. The build fails when a row is out of `Hook` order.
const HOOK_METHODS: [&str; HOOKS.len()] = {
    let mut out = [""; HOOKS.len()];
    let mut i = 0;
    while i < HOOKS.len() {
        assert!(
            HOOKS[i].hook as usize == i,
            "HOOKS rows follow the Hook order"
        );
        out[i] = own_method(HOOKS[i].protocol);
        i += 1;
    }
    out
};

/// The one method the std decl of `protocol` declares itself, or `None` when it declares none or
/// several (a bundle such as `Arithmetic`).
pub fn protocol_method(protocol: &str) -> Option<&'static str> {
    let mut own = PROTOCOL_METHODS.iter().filter(|(p, _)| *p == protocol);
    match (own.next(), own.next()) {
        (Some((_, m)), None) => Some(*m),
        (None, _) | (Some(_), Some(_)) => None,
    }
}

/// A std protocol that a builtin satisfies intrinsically with no operator hook. The checker's
/// `INTRINSIC_PROTO_METHODS` keys its rows for these on `Unhooked::protocol`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unhooked {
    Error,
    PathLike,
}

/// The method of each `Unhooked` protocol, in `Unhooked::ALL` order, from its std decl.
const UNHOOKED_METHODS: [&str; Unhooked::ALL.len()] = {
    let mut out = [""; Unhooked::ALL.len()];
    let mut i = 0;
    while i < Unhooked::ALL.len() {
        assert!(
            Unhooked::ALL[i] as usize == i,
            "Unhooked::ALL follows the variant order"
        );
        out[i] = own_method(Unhooked::ALL[i].protocol());
        i += 1;
    }
    out
};

impl Unhooked {
    pub const ALL: [Unhooked; 2] = [Unhooked::Error, Unhooked::PathLike];

    pub const fn protocol(self) -> &'static str {
        match self {
            Unhooked::Error => "Error",
            Unhooked::PathLike => "PathLike",
        }
    }

    /// The method a builtin answers for this protocol, from its std decl.
    pub const fn method(self) -> &'static str {
        UNHOOKED_METHODS[self as usize]
    }
}

impl Hook {
    pub fn bit(self) -> u32 {
        1 << (self as u32)
    }

    /// This hook's `HOOKS` row. Every `Hook` has exactly one (`every_hook_has_one_row`).
    pub fn row(self) -> &'static HookRow {
        HOOKS
            .iter()
            .find(|row| row.hook == self)
            .expect("every Hook has a HOOKS row")
    }

    /// The method this hook calls, from its protocol's std decl.
    pub const fn method(self) -> &'static str {
        HOOK_METHODS[self as usize]
    }

    /// The hook whose method is `name`, if any.
    pub fn of_method(name: &str) -> Option<Hook> {
        HOOKS
            .iter()
            .map(|row| row.hook)
            .find(|hook| hook.method() == name)
    }

    /// The reserved protocol of this hook's `HOOKS` row.
    pub fn protocol(self) -> &'static str {
        self.row().protocol
    }

    /// The hook a binary operator calls, `None` for `and`/`or` (no hook). THE one map from a
    /// binary operator to its protocol and method: `binary_protocol` and the checker's operator
    /// typing read it.
    pub fn of_binary(op: crate::ast::BinaryOp) -> Option<Hook> {
        use crate::ast::BinaryOp as B;
        match op {
            B::Add => Some(Hook::Add),
            B::Sub => Some(Hook::Sub),
            B::Mul => Some(Hook::Mul),
            B::Div => Some(Hook::Div),
            B::Mod => Some(Hook::Mod),
            B::Lt | B::LtEq | B::Gt | B::GtEq => Some(Hook::Compare),
            B::Eq | B::NotEq => Some(Hook::Eq),
            B::In => Some(Hook::Contains),
            B::BitAnd => Some(Hook::BitAnd),
            B::BitOr => Some(Hook::BitOr),
            B::BitXor => Some(Hook::BitXor),
            B::Shl => Some(Hook::Shl),
            B::Shr => Some(Hook::Shr),
            B::And | B::Or => None,
        }
    }

    /// The arithmetic and bitwise operator hooks: the binary `+ - * / % & | ^ << >>` and unary
    /// `-`. Their protocols take `(self, other: Self) -> Self` (`neg` takes `self` only).
    pub fn is_arithmetic(self) -> bool {
        match self {
            Hook::Add
            | Hook::Sub
            | Hook::Mul
            | Hook::Div
            | Hook::Mod
            | Hook::Neg
            | Hook::BitAnd
            | Hook::BitOr
            | Hook::BitXor
            | Hook::Shl
            | Hook::Shr => true,
            Hook::Iter
            | Hook::Next
            | Hook::Index
            | Hook::SetIndex
            | Hook::Slice
            | Hook::Str
            | Hook::Eq
            | Hook::Hash
            | Hook::Compare
            | Hook::Contains => false,
        }
    }
}

/// The run-time text of an int fault: `IntFault::message`, with a binary op named by its hook's
/// protocol (`integer overflow in Add`). The checker and the VM print it.
pub fn int_fault_message(f: crate::ast::consteval::IntFault) -> String {
    f.message(|op| {
        Hook::of_binary(op)
            .expect("an int arithmetic op has a hook")
            .protocol()
    })
}

/// The hooks one declared type has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HookSet(pub u32);

impl HookSet {
    pub fn insert(&mut self, hook: Hook) {
        self.0 |= hook.bit();
    }

    pub fn has(self, hook: Hook) -> bool {
        self.0 & hook.bit() != 0
    }
}
