//! base: operator hooks. One HOOKS row per hook: its protocol, its method and its operator.

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
    pub method: &'static str,
    /// The user-facing spelling of the operator that calls the hook.
    pub operator: &'static str,
    /// The operator accepts every value, hook or not (`print` falls back to the default display).
    pub total: bool,
}

pub const HOOKS: &[HookRow] = &[
    HookRow {
        hook: Hook::Iter,
        protocol: "Iterable",
        method: "iter",
        operator: "for",
        total: false,
    },
    HookRow {
        hook: Hook::Next,
        protocol: "Iterator",
        method: "next",
        operator: "for",
        total: false,
    },
    HookRow {
        hook: Hook::Index,
        protocol: "Index",
        method: "index",
        operator: "x[k]",
        total: false,
    },
    HookRow {
        hook: Hook::SetIndex,
        protocol: "IndexSet",
        method: "set_index",
        operator: "x[k] = v",
        total: false,
    },
    HookRow {
        hook: Hook::Slice,
        protocol: "Slice",
        method: "slice",
        operator: "x[a..b]",
        total: false,
    },
    HookRow {
        hook: Hook::Str,
        protocol: "Stringable",
        method: "str",
        operator: "print",
        total: true,
    },
    HookRow {
        hook: Hook::Eq,
        protocol: "Eq",
        method: "eq",
        operator: "==",
        total: false,
    },
    HookRow {
        hook: Hook::Hash,
        protocol: "Hashable",
        method: "hash",
        operator: "a Set or Map key",
        total: false,
    },
    HookRow {
        hook: Hook::Compare,
        protocol: "Comparable",
        method: "compare",
        operator: "<",
        total: false,
    },
    HookRow {
        hook: Hook::Add,
        protocol: "Add",
        method: "add",
        operator: "+",
        total: false,
    },
    HookRow {
        hook: Hook::Sub,
        protocol: "Sub",
        method: "sub",
        operator: "-",
        total: false,
    },
    HookRow {
        hook: Hook::Mul,
        protocol: "Mul",
        method: "mul",
        operator: "*",
        total: false,
    },
    HookRow {
        hook: Hook::Div,
        protocol: "Div",
        method: "div",
        operator: "/",
        total: false,
    },
    HookRow {
        hook: Hook::Mod,
        protocol: "Mod",
        method: "mod",
        operator: "%",
        total: false,
    },
    HookRow {
        hook: Hook::Neg,
        protocol: "Neg",
        method: "neg",
        operator: "unary -",
        total: false,
    },
    HookRow {
        hook: Hook::Contains,
        protocol: "Contains",
        method: "contains",
        operator: "in",
        total: false,
    },
    HookRow {
        hook: Hook::BitAnd,
        protocol: "BitAnd",
        method: "bitand",
        operator: "&",
        total: false,
    },
    HookRow {
        hook: Hook::BitOr,
        protocol: "BitOr",
        method: "bitor",
        operator: "|",
        total: false,
    },
    HookRow {
        hook: Hook::BitXor,
        protocol: "BitXor",
        method: "bitxor",
        operator: "^",
        total: false,
    },
    HookRow {
        hook: Hook::Shl,
        protocol: "Shl",
        method: "shl",
        operator: "<<",
        total: false,
    },
    HookRow {
        hook: Hook::Shr,
        protocol: "Shr",
        method: "shr",
        operator: ">>",
        total: false,
    },
];

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

    /// The method name of this hook's `HOOKS` row.
    pub fn method(self) -> &'static str {
        self.row().method
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
