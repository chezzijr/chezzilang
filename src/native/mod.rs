//! The native FFI seam (M6c): the mechanism by which a Rust function is exposed as a callable
//! Chezzi value, so `std.math`/`std.io`/`std.os` can reach things pure Chezzi cannot (file I/O,
//! the OS, `f64` intrinsics).
//!
//! This module deliberately knows **nothing** about either engine's value representation. A native
//! binding is written once against the [`Host`] trait and runs unchanged on both the tree-walk
//! interpreter (Rc-based `Value`) and the bytecode VM (`Copy` `Value` + GC handles). Each engine
//! implements [`Host`] over its own argument stack and lowers the returned [`NativeRet`] into its
//! own value type *after* the call returns — so native code never touches an `Rc` or a `GcRef`, and
//! the VM's "allocate only at instruction boundaries" GC invariant holds by construction.
//!
//! This is the CPython-built-in-C-module model (compiled-in bindings), **not** dynamic `cdylib`/
//! C-ABI loading — that (Level-3) stays deferred per `docs/spec.md`.

pub mod fs;
pub mod io;
pub mod math;
pub mod os;
pub mod process;
pub mod regex;
pub mod request;
pub mod time;

use std::collections::HashMap;

/// The source of lines for `std.io.read_line`. Tests inject a fixed buffer for determinism; the CLI
/// uses the real process stdin (read lazily, one line at a time, so it never blocks until needed).
#[derive(Debug, Default)]
pub enum Stdin {
    /// No input — `read_line` immediately reports EOF.
    #[default]
    Empty,
    /// A fixed list of lines (deterministic; used by tests and embedders to inject stdin).
    #[allow(dead_code)]
    Lines(std::collections::VecDeque<String>),
    /// The real process stdin.
    Real,
}

impl Stdin {
    /// Read the next line (trailing `\n`/`\r\n` stripped); `None` at EOF.
    pub fn read_line(&mut self) -> Result<Option<String>, HostError> {
        match self {
            Stdin::Empty => Ok(None),
            Stdin::Lines(q) => Ok(q.pop_front()),
            Stdin::Real => {
                let mut buf = String::new();
                let n = std::io::stdin()
                    .read_line(&mut buf)
                    .map_err(|e| HostError { message: e.to_string() })?;
                if n == 0 {
                    return Ok(None);
                }
                let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
                Ok(Some(trimmed.to_string()))
            }
        }
    }
}

/// Engine-neutral runtime configuration the [`Host`] exposes to native std modules: program args
/// (`std.os.args`), environment (`std.os.env`), and stdin (`std.io.read_line`). [`Default`] is the
/// deterministic, inert config used by tests and by `run_file` (empty args/env, EOF stdin); the CLI
/// builds one from the real process via [`HostConfig::from_process`].
#[derive(Debug, Default)]
pub struct HostConfig {
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub stdin: Stdin,
}

impl HostConfig {
    /// Build a config from the real process: program args (everything after the script path is the
    /// caller's responsibility to pass in), the full environment, and real stdin.
    pub fn from_process(args: Vec<String>) -> Self {
        HostConfig {
            args,
            env: std::env::vars().collect(),
            stdin: Stdin::Real,
        }
    }
}

/// An error raised by a native function. Engine-agnostic: each engine maps it to its own
/// `RuntimeError` (attaching the call's source span).
#[derive(Debug, Clone, PartialEq)]
pub struct HostError {
    pub message: String,
}

/// A value produced by a native function, in an engine-neutral form. The calling engine lowers this
/// into its own `Value` once the native call returns — building `Rc`/`RefCell` lists (interp) or
/// allocating heap objects (VM) at that point. `Ok`/`Err`/`Some`/`None` lower to the built-in
/// `Result` / `Option` enums that both engines already register.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeRet {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    List(Vec<NativeRet>),
    /// A struct instance: type name + named fields in declaration order. The checker must know a
    /// struct of this `name` with matching field types (seeded for stdlib structs like
    /// `Response`/`Match`). Each engine lowers this to its own struct value.
    Struct {
        name: String,
        fields: Vec<(String, NativeRet)>,
    },
    /// A `{k: v, …}` map, insertion-ordered. Used by e.g. `std.request` response headers
    /// (`map[str, str]`). Keys MUST be unique: lowering pushes entries verbatim (it does not dedup),
    /// so the caller is responsible for upholding the language's map unique-key invariant.
    Map(Vec<(NativeRet, NativeRet)>),
    Ok(Box<NativeRet>),
    Err(String),
    Some(Box<NativeRet>),
    None,
    Nil,
}

/// The engine-agnostic context a native function operates through. Object-safe (`&mut dyn Host`):
/// arguments are read by index (already evaluated and bounds-checked by the engine before the
/// call), and side-effects (stdout/stderr, stdin, process/env) route through the host so they hit
/// the engine's captured output buffer and injected configuration — never real stdio/process state
/// directly (keeps runs deterministic and testable).
pub trait Host {
    fn arg_count(&self) -> usize;
    /// `args[i]` as an int; errors if it is not an int.
    fn arg_int(&mut self, i: usize) -> Result<i64, HostError>;
    /// Whether `args[i]` is an int (no promotion). Lets a numeric-polymorphic native fn
    /// (`abs`/`min`/`max`) pick an int vs float result without consuming the argument.
    fn arg_is_int(&self, i: usize) -> bool;
    /// `args[i]` as a float; an int argument is promoted (matches the builtin `sqrt`).
    fn arg_float(&mut self, i: usize) -> Result<f64, HostError>;
    /// `args[i]` as an owned string; errors if it is not a str.
    fn arg_str(&mut self, i: usize) -> Result<String, HostError>;

    /// Append to the program's captured stdout buffer.
    fn write_stdout(&mut self, s: &str);
    /// Append to the program's captured stderr buffer.
    fn write_stderr(&mut self, s: &str);
    /// Read one line from the injected stdin source; `None` at EOF. The trailing newline is
    /// stripped.
    fn read_line(&mut self) -> Result<Option<String>, HostError>;

    /// The program arguments (injected; defaults to empty).
    fn os_args(&self) -> Vec<String>;
    /// An environment variable from the injected environment.
    fn os_env(&self, key: &str) -> Option<String>;
    /// The current working directory.
    fn os_getcwd(&self) -> Result<String, HostError>;

    /// Record a cooperative-exit request (`std.os.exit(code)`). The engine stores the code and the
    /// native fn returns an error sentinel that unwinds past any `recover:` to the top level, where
    /// the driver reports the code as the process exit status. Default: no-op (test hosts).
    fn request_exit(&mut self, _code: i64) {}
}

/// A Rust function callable from Chezzi. A bare `fn` pointer (no captured state, no generics) so it
/// can live behind an `Rc`/`GcRef` cheaply and compare/clone trivially.
pub type NativeFn = fn(&mut dyn Host) -> Result<NativeRet, HostError>;

impl HostError {
    /// A missing positional argument (the engine's bounds check failed for index `i`).
    pub fn missing_arg(i: usize) -> Self {
        HostError { message: format!("missing argument {i}") }
    }
    /// A wrong-typed argument at index `i`.
    pub fn arg_type(i: usize, want: &str, got: &str) -> Self {
        HostError { message: format!("argument {i} must be {want}, got {got}") }
    }
}

/// Helper for native functions: assert an exact argument count, else a uniform error.
pub fn expect_args(h: &dyn Host, name: &str, n: usize) -> Result<(), HostError> {
    let got = h.arg_count();
    if got == n {
        Ok(())
    } else {
        Err(HostError {
            message: format!("{name}() expects {n} argument(s), got {got}"),
        })
    }
}

/// If this dotted import path names a native (virtual, no-file) std module, return its canonical
/// `'static` name. `std.str` is intentionally absent: it is a real Chezzi file under the stdlib dir.
pub fn native_name(path: &[String]) -> Option<&'static str> {
    match path {
        [a, b] if a == "std" => match b.as_str() {
            "math" => Some("std.math"),
            "io" => Some("std.io"),
            "os" => Some("std.os"),
            "process" => Some("std.process"),
            "fs" => Some("std.fs"),
            "time" => Some("std.time"),
            "regex" => Some("std.regex"),
            "request" => Some("std.request"),
            _ => None,
        },
        _ => None,
    }
}

/// The callable members of a native module, as `(name, fn)`. Single source of truth shared by both
/// engines (only the per-engine lowering and the checker's static signatures differ). Empty for an
/// unknown name.
pub fn native_members(module: &str) -> &'static [(&'static str, NativeFn)] {
    match module {
        "std.math" => math::MEMBERS,
        "std.io" => io::MEMBERS,
        "std.os" => os::MEMBERS,
        "std.process" => process::MEMBERS,
        "std.fs" => fs::MEMBERS,
        "std.time" => time::MEMBERS,
        "std.regex" => regex::MEMBERS,
        "std.request" => request::MEMBERS,
        _ => &[],
    }
}

/// The constant (non-callable) members of a native module, as `(name, value)`. Currently only
/// `std.math` exposes any (`pi`, `e`).
pub fn native_consts(module: &str) -> &'static [(&'static str, f64)] {
    match module {
        "std.math" => math::CONSTS,
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A standalone `Host` for unit-testing native fns in isolation from either engine.
    #[derive(Default)]
    struct MockHost {
        ints: Vec<i64>,
        stdout: String,
        stdin: VecDeque<String>,
    }

    impl Host for MockHost {
        fn arg_count(&self) -> usize {
            self.ints.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            self.ints.get(i).copied().ok_or(HostError {
                message: "missing arg".into(),
            })
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            Ok(self.arg_int(i)? as f64)
        }
        fn arg_is_int(&self, i: usize) -> bool {
            i < self.ints.len()
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError {
                message: "no str args".into(),
            })
        }
        fn write_stdout(&mut self, s: &str) {
            self.stdout.push_str(s);
        }
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(self.stdin.pop_front())
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<String, HostError> {
            Ok("/".into())
        }
    }

    /// A sample native fn exercising the seam: read two int args, return their sum.
    fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
        expect_args(h, "add", 2)?;
        Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
    }

    #[test]
    fn native_fn_reads_args_and_returns_through_host() {
        let mut host = MockHost {
            ints: vec![40, 2],
            ..Default::default()
        };
        let f: NativeFn = add;
        assert_eq!(f(&mut host), Ok(NativeRet::Int(42)));
    }

    #[test]
    fn expect_args_reports_arity() {
        let mut host = MockHost {
            ints: vec![1],
            ..Default::default()
        };
        let f: NativeFn = add;
        assert_eq!(
            f(&mut host),
            Err(HostError {
                message: "add() expects 2 argument(s), got 1".into()
            })
        );
    }

    #[test]
    fn native_name_recognizes_the_three_native_modules() {
        assert_eq!(native_name(&["std".into(), "math".into()]), Some("std.math"));
        assert_eq!(native_name(&["std".into(), "io".into()]), Some("std.io"));
        assert_eq!(native_name(&["std".into(), "os".into()]), Some("std.os"));
        // str is a real Chezzi file, not virtual.
        assert_eq!(native_name(&["std".into(), "str".into()]), None);
        // user modules are never native.
        assert_eq!(native_name(&["foo".into(), "math".into()]), None);
        assert_eq!(native_name(&["std".into()]), None);
    }
}
