//! base: the facts both the front end (resolver, checker, compiler) and the VM read — the bytecode,
//! the airlock's crossing marks, the operator hooks, module identity, the `json.decode` descriptor,
//! the provider names and the shared message words. It depends on the syntax layer and `native`
//! only, so neither the checker nor the VM depends on the other.

pub mod crossing;
pub mod hooks;
pub mod json_desc;
pub mod module_id;
pub mod op;
pub mod provider_name;
pub mod words;
