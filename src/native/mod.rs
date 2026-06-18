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

// Dynamic C-ABI FFI is unix-only: it builds on `dlopen`/libffi, and `int` marshals as C `long`
// (64-bit on every supported LP64 unix target; on a non-unix LLP64 target like Windows x64 C `long`
// is 32-bit, which would silently truncate). The checker rejects `extern` on non-unix (see
// `checker::hoist`), so this module + its `Op::MakeCffi`/`Obj::Cffi`/`Value::Cffi` consumers are
// only reached on unix. All supported Chezzi targets are unix; non-unix is unsupported by design.
#[cfg(unix)]
pub mod cffi;
pub mod ffi;
pub mod fs;
pub mod io;
pub mod math;
pub mod net;
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
                    .map_err(|e| HostError {
                        message: e.to_string(),
                    })?;
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
    /// An opaque C-ABI handle: a raw pointer address (`void*`), carried as a `usize` so it stays
    /// `Send` and never touches the GC. Produced by an `extern "lib":` fn declared `-> ptr`
    /// (`src/native/cffi.rs`) and by `std.ffi.null()`. A NULL return lowers to `Ptr(0)` — it does
    /// **not** fault (unlike a `str` return), since NULL is a legitimate "creation failed" signal for
    /// handle APIs. Each engine lowers this to its own opaque pointer value (`Obj::Ptr`/`Value::Ptr`).
    /// Untyped (no `FILE*` vs `sqlite3*` distinction) and never auto-freed — the author calls the
    /// library's own destroy (e.g. `fclose`); see `docs/spec.md` §Level-3 FFI limits.
    Ptr(usize),
}

/// D5 — an offloaded blocking native's already-extracted argument, in `Send` primitive form (no heap
/// `GcRef`). The engine materializes these on the worker (heap live) before handing a blocking call
/// to the dirty pool; the off-heap host serves them back to the native fn off-thread. The scoped
/// blocking fns take only int / str args; float / bool are carried for completeness.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeArg {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// An insertion-ordered `map[str, str]`, pre-extracted so a blocking native that reads a map arg
    /// (today only `std.request.request`'s custom headers) can still offload to the dirty pool: the
    /// off-heap host has no `Vm`/heap, so the map is snapshotted into owned `(String, String)` pairs
    /// before the handoff and served back via [`Host::arg_str_map`]. Order is preserved (the engine
    /// reads `MapData.entries`), so header order is deterministic across engines.
    Map(Vec<(String, String)>),
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
    /// `args[i]` as a bool; errors if it is not a bool. Used by the C-ABI FFI (`extern`) to marshal a
    /// Chezzi `bool` into a C `int`. The default returns a "no bool args" error so a host that never
    /// passes bools (the std-module test fixtures / off-heap host) needn't implement it.
    fn arg_bool(&mut self, i: usize) -> Result<bool, HostError> {
        let _ = i;
        Err(HostError {
            message: "this host does not support bool arguments".into(),
        })
    }
    /// `args[i]` as an opaque C-ABI pointer handle (a raw address). Used by the C-ABI FFI (`extern`)
    /// to marshal a Chezzi `ptr` into a C `void*`, and by `std.ffi.is_null`. The default returns a
    /// "no ptr args" error so a host that never passes handles (the std-module test fixtures /
    /// off-heap host) needn't implement it.
    fn arg_ptr(&mut self, i: usize) -> Result<usize, HostError> {
        let _ = i;
        Err(HostError {
            message: "this host does not support pointer arguments".into(),
        })
    }
    /// `args[i]` as a by-value C struct: its fields as engine-neutral [`NativeRet`] scalars in
    /// declaration order. Used by the C-ABI FFI (`extern`) to marshal a Chezzi struct into a C struct
    /// passed by value (v1: flat scalar fields only — `int`/`float`/`bool`/`ptr`/`int8`..`uint64`). Each
    /// engine surfaces its already-ordered field values; the cffi layer casts each to its C field width.
    /// The default returns a "no struct args" error so a host that never passes structs (the std-module
    /// test fixtures / off-heap host) needn't implement it.
    fn arg_struct_fields(&mut self, i: usize) -> Result<Vec<NativeRet>, HostError> {
        let _ = i;
        Err(HostError {
            message: "this host does not support struct arguments".into(),
        })
    }
    /// `args[i]` as an owned string; errors if it is not a str.
    fn arg_str(&mut self, i: usize) -> Result<String, HostError>;
    /// `args[i]` as an insertion-ordered `map[str, str]`, returned as owned `(key, value)` pairs in
    /// map insertion order. Errors if the arg is not a map or any key/value is not a str. Used by
    /// `std.request.request` to read custom request headers.
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, HostError>;

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
        HostError {
            message: format!("missing argument {i}"),
        }
    }
    /// A wrong-typed argument at index `i`.
    pub fn arg_type(i: usize, want: &str, got: &str) -> Self {
        HostError {
            message: format!("argument {i} must be {want}, got {got}"),
        }
    }
}

/// D5 — whether a native fn (by its bare member name) is a *blocking, off-heap-safe* call the M:N
/// engine offloads to the dirty/blocking pool instead of running inline on a core worker (so a
/// `sleep_ms`/`read_file` can't pin a worker → the live G3 starvation). "Off-heap-safe" is the
/// contract that lets it run on a pool thread with no `Vm`: it reads only primitive args
/// (`arg_int`/`arg_str`) and returns a primitive [`NativeRet`] — it never touches the heap, the
/// stdout/stderr buffers, stdin, or os state during the blocking part (so [`Host`]'s I/O methods are
/// `unreachable!` on the off-heap host). The scoped set: `std.io.read_file`/`write_file`, all of
/// `std.fs`, and `std.time.sleep_ms`. Classified by bare name (the engine has the member name at the
/// dispatch site); the set is distinctive across the native modules. D5 owe #1 added `std.request`
/// (`get`/`post`, HTTP via `ureq`) and `std.process` (`cmd`, subprocess): both verified off-heap-safe
/// (primitive `str` args, primitive `Struct`/`Ok`/`Err` returns, no heap/stdio touch during the call),
/// so they offload like the rest instead of pinning a core worker on network / subprocess I/O.
pub fn is_blocking(name: &str) -> bool {
    matches!(
        name,
        // std.io (file I/O only — print/eprint/read_line touch host stdio, run inline)
        "read_file" | "write_file"
        // std.fs (all members are filesystem syscalls)
        | "list_dir" | "exists" | "is_file" | "is_dir" | "size" | "glob"
        // std.time
        | "sleep_ms"
        // std.request (network I/O) + std.process (subprocess) — D5 owe #1.
        // `request`/`put`/`patch`/`delete`/`head` are the verb wrappers + the general header-carrying
        // call; `request` offloads its `map[str, str]` headers via `NativeArg::Map` (off-heap-safe).
        | "get" | "post" | "request" | "put" | "patch" | "delete" | "head" | "cmd"
    )
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
            "net" => Some("std.net"),
            "ffi" => Some("std.ffi"),
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
        "std.net" => net::MEMBERS,
        "std.ffi" => ffi::MEMBERS,
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
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError {
                message: "no map args".into(),
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
        assert_eq!(
            native_name(&["std".into(), "math".into()]),
            Some("std.math")
        );
        assert_eq!(native_name(&["std".into(), "io".into()]), Some("std.io"));
        assert_eq!(native_name(&["std".into(), "os".into()]), Some("std.os"));
        // str is a real Chezzi file, not virtual.
        assert_eq!(native_name(&["std".into(), "str".into()]), None);
        // user modules are never native.
        assert_eq!(native_name(&["foo".into(), "math".into()]), None);
        assert_eq!(native_name(&["std".into()]), None);
    }

    /// D5 — the blocking-fn classifier flags exactly the off-heap-safe blocking natives (the work the
    /// dirty pool offloads): `std.io.read_file`/`write_file`, all of `std.fs`, and `std.time.sleep_ms`.
    #[test]
    fn is_blocking_flags_the_offloadable_set() {
        for name in [
            "read_file",
            "write_file",
            "list_dir",
            "exists",
            "is_file",
            "is_dir",
            "size",
            "glob",
            "sleep_ms",
        ] {
            assert!(is_blocking(name), "{name} should be blocking");
        }
    }

    /// D5 owe #1 — `std.request` (HTTP via `ureq`) and `std.process` (subprocess) are blocking and
    /// off-heap-safe: primitive `str` args, primitive returns (`Struct`/`Ok(Str)`/`Err`), no heap /
    /// stdio touch during the blocking call. They satisfy the offload contract, so the M:N engine must
    /// route them through the dirty pool instead of pinning a core worker.
    #[test]
    fn is_blocking_flags_request_and_process() {
        for name in ["get", "post", "cmd"] {
            assert!(is_blocking(name), "{name} should be blocking");
        }
    }

    /// The new `std.request` verbs (`put`/`patch`/`delete`/`head`) and the general `request()` are
    /// network I/O — they must offload to the dirty pool under `--parallel`, same as `get`/`post`.
    #[test]
    fn is_blocking_flags_new_request_verbs() {
        for name in ["request", "put", "patch", "delete", "head"] {
            assert!(is_blocking(name), "{name} should be blocking");
        }
    }

    /// `NativeArg::Map` carries an insertion-ordered str/str map across the off-heap offload boundary
    /// so `request()`'s headers survive the handoff to the dirty pool.
    #[test]
    fn native_arg_map_constructs_and_compares() {
        let a = NativeArg::Map(vec![("a".into(), "b".into())]);
        let b = NativeArg::Map(vec![("a".into(), "b".into())]);
        assert_eq!(a, b);
        assert_ne!(a, NativeArg::Map(vec![]));
    }

    /// [`is_blocking`] classifies by *bare member name* (the engine has only the member name at the
    /// dispatch site), which is sound ONLY while member names are unique across the native modules —
    /// otherwise a non-blocking member sharing a name with a blocking one (e.g. a future `regex.get`)
    /// would be wrongly offloaded to the off-heap pool, where its host-I/O methods `unreachable!`.
    /// This guard turns any future name collision into a RED test instead of a production panic.
    #[test]
    fn native_member_names_are_unique_across_modules() {
        use std::collections::HashMap;
        let modules = [
            "std.math",
            "std.io",
            "std.os",
            "std.process",
            "std.fs",
            "std.time",
            "std.regex",
            "std.request",
            "std.ffi",
        ];
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for module in modules {
            for (name, _) in native_members(module) {
                if let Some(prev) = seen.insert(name, module) {
                    panic!(
                        "native member name `{name}` is defined in both `{prev}` and `{module}` — \
                         bare-name `is_blocking` classification is no longer sound (see is_blocking docs)"
                    );
                }
            }
        }
    }

    /// Fast / pure / host-I/O natives must NOT be offloaded: `print`/`eprint`/`read_line` touch the
    /// host stdio buffers (off-heap host would `unreachable!`), and `now`/`monotonic`/`format`/math are
    /// cheap. Mislabeling a CPU/pure fn as blocking would needlessly bounce it through the pool.
    #[test]
    fn is_blocking_excludes_fast_and_host_io_natives() {
        for name in [
            "print",
            "eprint",
            "read_line",
            "now",
            "monotonic",
            "format",
            "abs",
            "sqrt",
        ] {
            assert!(!is_blocking(name), "{name} should not be blocking");
        }
    }
}
