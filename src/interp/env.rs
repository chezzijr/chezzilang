//! Lexical environment.
//!
//! Two tiers, so functions get **lexical** (not dynamic) scoping:
//!   - `globals` — the *current module's* top-level declarations (fns, structs, enums, top-level
//!     `:=`). A [`ModEnv`] (shared `Rc<RefCell<…>>`) so a function carries its home module's
//!     globals and resolves against them even when imported elsewhere (see `Interp::call`, which
//!     swaps `globals` to the callee's home for the duration of the call).
//!   - `locals`  — a stack of block scopes for the *currently executing* function body. A call
//!     swaps in a fresh local stack, so a callee never sees the caller's locals — only its home
//!     globals and its own params.
//!
//! `define` writes to the innermost local scope, or to globals when no local scope is open
//! (i.e. at the top level). `assign` mutates the nearest existing binding, locals before globals.

use super::value::ModEnv;
use super::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct Env {
    globals: ModEnv,
    locals: Vec<HashMap<String, Value>>,
}

impl Env {
    pub fn new() -> Self {
        Env {
            globals: ModEnv::new(),
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

    /// The current module's globals handle — captured by a fn/closure so it can be restored when
    /// the callable runs (even after being imported into a different module).
    pub fn globals_rc(&self) -> ModEnv {
        self.globals.clone()
    }

    /// Replace the active module globals, returning the previous handle (enter/leave a call into a
    /// function whose home module differs from the caller's).
    pub fn swap_globals(&mut self, new_globals: ModEnv) -> ModEnv {
        std::mem::replace(&mut self.globals, new_globals)
    }

    /// Declare a name. Writes to the innermost local scope, or to globals at the top level.
    pub fn define(&mut self, name: &str, value: Value) {
        match self.locals.last_mut() {
            Some(scope) => {
                scope.insert(name.to_string(), value);
            }
            None => {
                self.globals.0.borrow_mut().insert(name.to_string(), value);
            }
        }
    }

    /// Look up a name: innermost local → outer locals → globals.
    pub fn get(&self, name: &str) -> Option<Value> {
        self.locals
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
            .or_else(|| self.globals.0.borrow().get(name).cloned())
    }

    /// Look up a name in the LOCAL scopes only (skipping globals/functions). Used to decide whether a
    /// real lexical binding shadows a same-named enum in qualified-variant access `Enum.Variant` —
    /// matching the VM's locals/captures-only gate (`resolve_local`), so the two engines agree.
    pub fn get_local(&self, name: &str) -> Option<Value> {
        self.locals.iter().rev().find_map(|scope| scope.get(name).cloned())
    }

    /// Mutate an existing binding (`=`, `+=`, `-=`). Returns `false` if undefined.
    pub fn assign(&mut self, name: &str, value: Value) -> bool {
        for scope in self.locals.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        let mut g = self.globals.0.borrow_mut();
        if g.contains_key(name) {
            g.insert(name.to_string(), value);
            return true;
        }
        false
    }
}
