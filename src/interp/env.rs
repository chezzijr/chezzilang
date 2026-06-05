//! Lexical environment.
//!
//! Two tiers, so functions get **lexical** (not dynamic) scoping:
//!   - `globals` — top-level declarations (fns, structs, enums, top-level `:=`). Always visible.
//!   - `locals`  — a stack of block scopes for the *currently executing* function body. A call
//!     swaps in a fresh local stack (see `Interp::call`), so a callee never sees the caller's
//!     locals — only globals and its own params.
//!
//! `define` writes to the innermost local scope, or to globals when no local scope is open
//! (i.e. at the top level). `assign` mutates the nearest existing binding, locals before globals.

use super::Value;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone, Default)]
pub struct Env {
    globals: Rc<HashMap<String, Value>>,
    locals: Vec<HashMap<String, Value>>,
}

impl Env {
    pub fn new() -> Self {
        Env {
            globals: Rc::new(HashMap::new()),
            locals: Vec::new(),
        }
    }

    pub fn push(&mut self) {
        self.locals.push(HashMap::new());
    }

    pub fn pop(&mut self) {
        self.locals.pop();
    }

    /// A clone of the current local frames — captured by a closure at creation time.
    pub fn snapshot_locals(&self) -> Vec<HashMap<String, Value>> {
        self.locals.clone()
    }

    /// Replace the local-scope stack, returning the previous one (used to enter/leave a call).
    pub fn swap_locals(&mut self, new_locals: Vec<HashMap<String, Value>>) -> Vec<HashMap<String, Value>> {
        std::mem::replace(&mut self.locals, new_locals)
    }

    /// Declare a name. Writes to the innermost local scope, or to globals at the top level.
    pub fn define(&mut self, name: &str, value: Value) {
        match self.locals.last_mut() {
            Some(scope) => {
                scope.insert(name.to_string(), value);
            }
            None => {
                Rc::make_mut(&mut self.globals).insert(name.to_string(), value);
            }
        }
    }

    /// Look up a name: innermost local → outer locals → globals.
    pub fn get(&self, name: &str) -> Option<Value> {
        self.locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
            .or_else(|| self.globals.get(name).cloned())
    }

    /// Mutate an existing binding (`=`, `+=`, `-=`). Returns `false` if undefined.
    pub fn assign(&mut self, name: &str, value: Value) -> bool {
        for scope in self.locals.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        if self.globals.contains_key(name) {
            Rc::make_mut(&mut self.globals).insert(name.to_string(), value);
            return true;
        }
        false
    }
}
