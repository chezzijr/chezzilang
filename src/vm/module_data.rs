//! A module namespace's payload: its global slots and its name table behind a lock.

use super::rvec::RVec;
use super::value::Value;
use std::collections::HashMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A module namespace's payload, boxed behind `Obj::Module` so the rare, cold `Module` variant
/// does not set the `Obj` size cap (as `Obj::Generator` boxes its core). Its slot-indexed
/// top-level bindings (`slots[i]` for compile-time slot `i`) and its [`ModuleNames`].
#[derive(Debug)]
pub struct ModuleData {
    pub slots: RVec<Value>,
    /// M3.0b — behind a lock: a define or a view re-seat rewrites the map while another thread
    /// can read the module (a `HashMap` that grows frees its old table).
    names: RwLock<ModuleNames>,
}

/// A module's name and the `index` (name → slot) that backs `module.member` reads, imports,
/// native population, and errors. A slot append takes the write lock with the index insert, so a
/// reader under the read lock sees `slots` and `index` in lockstep.
#[derive(Debug, Clone, Default)]
pub struct ModuleNames {
    pub name: Box<str>,
    pub index: HashMap<Box<str>, u32>,
}

impl ModuleData {
    pub fn new(name: Box<str>, slots: RVec<Value>, index: HashMap<Box<str>, u32>) -> Self {
        ModuleData {
            slots,
            names: RwLock::new(ModuleNames { name, index }),
        }
    }

    /// The names, read-locked. A panic under the write lock leaves whole entries (each write is
    /// one `HashMap` call), so a poisoned lock is read as it stands.
    pub fn names(&self) -> RwLockReadGuard<'_, ModuleNames> {
        self.names.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The names, write-locked (see [`names`](ModuleData::names) for poisoning).
    pub fn names_mut(&self) -> RwLockWriteGuard<'_, ModuleNames> {
        self.names.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl Clone for ModuleData {
    fn clone(&self) -> Self {
        let names = self.names().clone();
        ModuleData {
            slots: self.slots.clone(),
            names: RwLock::new(names),
        }
    }
}
