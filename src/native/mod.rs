//! The native FFI seam (M6c): the mechanism by which a Rust function is exposed as a callable
//! Chezzi value, so `std.math`/`std.io`/`std.os` can reach things pure Chezzi cannot (file I/O,
//! the OS, `f64` intrinsics).
//!
//! This module deliberately knows **nothing** about the VM's value representation. A native
//! binding is written once against the [`Host`] trait and runs unchanged on the bytecode VM
//! (`Copy` `Value` + GC handles). The VM
//! implements [`Host`] over its own argument stack and lowers the returned [`NativeRet`] into its
//! own value type *after* the call returns — so native code never touches a `GcRef`, and
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
pub mod crypto;
pub mod encoding;
pub mod ffi;
pub mod fs;
pub mod io;
pub mod math;
pub mod net;
pub mod os;
pub mod process;
pub mod rand;
pub mod regex;
pub mod request;
pub mod time;
pub mod uuid;

use std::collections::HashMap;
use std::io::Read as _;

/// The source of lines for `std.io.read_line`. Tests inject a fixed buffer for determinism; the CLI
/// uses the real process stdin (read lazily, one line at a time, so it never blocks until needed).
///
/// **Every task shares ONE stdin** (Go's `os.Stdin` / Python's `sys.stdin`), so `Clone` SHARES the
/// source, never copies the data: a line goes to exactly one reader — never duplicated, never
/// dropped. Which task gets a given line is nondeterministic; `None` means genuinely exhausted.
#[derive(Debug, Default, Clone)]
pub enum Stdin {
    /// No input — `read_line` immediately reports EOF. The host config for an embedder with no
    /// stdin (and the deterministic default for tests / `run_file`).
    #[default]
    Empty,
    /// A fixed list of lines (used by tests and embedders to inject stdin). Behind a shared
    /// `Arc<Mutex<..>>`: a clone reads the SAME queue, so a line consumed by one task is gone for
    /// all — cloning the `VecDeque` by value would hand every worker its own copy of every line.
    Lines(std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>),
    /// The real process stdin. A UNIT variant on purpose: `std::io::stdin()` is the process-global,
    /// internally-locked handle, and one `read_line` call takes that lock for the whole call — so
    /// concurrent readers get whole lines, never interleaved bytes. Never hold a `StdinLock` or wrap
    /// it in a per-worker `BufReader`: the former deadlocks across tasks, the latter steals bytes
    /// into a private buffer and drops lines.
    Real,
}

impl Stdin {
    /// A shared injected-lines stdin (the test/embedder source).
    pub fn lines(lines: impl IntoIterator<Item = String>) -> Stdin {
        Stdin::Lines(std::sync::Arc::new(std::sync::Mutex::new(
            lines.into_iter().collect(),
        )))
    }

    /// Read the next line (trailing `\n`/`\r\n` stripped); `None` at EOF.
    pub fn read_line(&mut self) -> Result<Option<String>, HostError> {
        match self {
            Stdin::Empty => Ok(None),
            Stdin::Lines(q) => Ok(q.lock().unwrap().pop_front()),
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

    /// Read ALL remaining stdin to EOF as one `str` (Python's `sys.stdin.read()`); `""` at a clean
    /// EOF. Drains the shared source, so a later read in ANY task then sees EOF. Non-UTF-8 real stdin
    /// is a fault (there is no stdin `read_bytes` hatch). Over the injected `Lines` source it
    /// reconstructs each line + `\n` (the queue is newline-stripped) — byte-exactness of the real
    /// stream is only observable via `Stdin::Real` (see `tests/interactive.rs`).
    pub fn read_all(&mut self) -> Result<String, HostError> {
        match self {
            Stdin::Empty => Ok(String::new()),
            Stdin::Lines(q) => {
                let mut q = q.lock().unwrap();
                let mut out = String::new();
                while let Some(line) = q.pop_front() {
                    out.push_str(&line);
                    out.push('\n');
                }
                Ok(out)
            }
            Stdin::Real => {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        HostError {
                            message: "stdin: stream is not valid UTF-8".into(),
                        }
                    } else {
                        HostError {
                            message: e.to_string(),
                        }
                    }
                })?;
                Ok(buf)
            }
        }
    }

    /// Read ONE Unicode scalar (a 1-char `str` — Chezzi has no `char`/`rune` scalar); `None` at a
    /// clean EOF. Reads exactly the bytes of one UTF-8 scalar. A partial (truncated at EOF) or
    /// invalid sequence is a fault, distinct from the clean `None`. Over the injected `Lines` source
    /// the virtual stream is line0 chars + a reconstructed `\n` + line1 chars…, matching `read_all`.
    pub fn read_char(&mut self) -> Result<Option<String>, HostError> {
        match self {
            Stdin::Empty => Ok(None),
            Stdin::Lines(q) => {
                let mut q = q.lock().unwrap();
                match q.front_mut() {
                    None => Ok(None),
                    // A fully-drained front line stands for its terminating newline.
                    Some(ln) if ln.is_empty() => {
                        q.pop_front();
                        Ok(Some("\n".to_string()))
                    }
                    Some(ln) => {
                        let c = ln.chars().next().unwrap();
                        ln.drain(..c.len_utf8());
                        Ok(Some(c.to_string()))
                    }
                }
            }
            Stdin::Real => {
                // Take the process-stdin lock for the WHOLE scalar (as `read_line` does for a whole
                // line): the lead byte and its continuation bytes MUST go to one reader atomically. A
                // per-byte `std::io::stdin().read()` releases the lock between bytes, so a concurrent
                // reader steals a continuation byte → a torn scalar / spurious not-UTF-8 fault under
                // the M:N engine. The buffered bytes live on the global `Stdin`, shared with
                // `read_line`, so unlocking at end of call loses nothing.
                // Read exactly one byte, transparently retrying a signal-interrupted (EINTR) read the
                // way std's buffered `read_line`/`read_to_string` do — `read_char` MUST NOT fault
                // where its siblings silently retry (the anti-drift contract). `Ok(None)` = EOF.
                fn read_one(r: &mut impl std::io::Read) -> std::io::Result<Option<u8>> {
                    let mut b = [0u8; 1];
                    loop {
                        match r.read(&mut b) {
                            Ok(0) => return Ok(None),
                            Ok(_) => return Ok(Some(b[0])),
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                    }
                }
                let stdin = std::io::stdin();
                let mut lock = stdin.lock();
                let first = match read_one(&mut lock).map_err(io_err)? {
                    None => return Ok(None), // clean EOF
                    Some(b) => b,
                };
                // Classify the leading byte → total scalar length (1..=4); a continuation/invalid
                // byte at the START is undecodable.
                let len = match first {
                    0x00..=0x7F => 1,
                    0xC0..=0xDF => 2,
                    0xE0..=0xEF => 3,
                    0xF0..=0xF7 => 4,
                    _ => return Err(not_utf8()),
                };
                let mut bytes = Vec::with_capacity(len);
                bytes.push(first);
                for _ in 1..len {
                    match read_one(&mut lock).map_err(io_err)? {
                        None => return Err(not_utf8()), // truncated mid-scalar
                        Some(b) => bytes.push(b),
                    }
                }
                match std::str::from_utf8(&bytes) {
                    Ok(s) => Ok(Some(s.to_string())),
                    Err(_) => Err(not_utf8()),
                }
            }
        }
    }
}

fn io_err(e: std::io::Error) -> HostError {
    HostError {
        message: e.to_string(),
    }
}

fn not_utf8() -> HostError {
    HostError {
        message: "stdin: stream is not valid UTF-8".into(),
    }
}

/// Engine-neutral runtime configuration the [`Host`] exposes to native std modules: program args
/// (`std.os.args`), environment (`std.os.env`), and stdin (`std.io.read_line`). [`Default`] is the
/// deterministic, inert config used by tests and by `run_file` (empty args/env, EOF stdin); the CLI
/// builds one from the real process via [`HostConfig::from_process`].
#[derive(Debug, Default)]
pub struct HostConfig {
    pub args: Vec<String>,
    /// The process environment. SHARED (`Arc<Mutex<…>>`), not deep-cloned, when an M:N worker is
    /// spawned (`sched.rs`) — so `std.os.setenv` from inside a task is visible to the parent and
    /// siblings, matching the serial engine (one Vm, one map) and process-global env semantics
    /// (Python `os.environ` / Go `os.Setenv` are visible across threads). The `Mutex` guards the
    /// concurrent access from real OS-thread workers.
    pub env: std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>,
    pub stdin: Stdin,
    /// STREAM the program's stdout/stderr straight to the process's real streams (one locked write
    /// per `print` → line-atomic), instead of accumulating into the VM's captured buffers. Only the
    /// `chezzi run` CLI sets this; `Default` = `false` = the BUFFERED sink, which is what every test
    /// helper and every embedder gets — and what keeps the serial-vs-M:N parity oracle byte-identical.
    pub stream: bool,
}

impl HostConfig {
    /// Build a config from the real process: program args (everything after the script path is the
    /// caller's responsibility to pass in), the full environment, and real stdin.
    pub fn from_process(args: Vec<String>) -> Self {
        HostConfig {
            args,
            // `vars_os`, not `vars`: `std::env::vars()` PANICS on a non-UTF-8 key or value, so one
            // hostile variable anywhere in the environment aborted startup with rc=101 — even for a
            // program that never touches `std.os`. Decoding is LOSSY (invalid bytes → U+FFFD, so two
            // raw keys can collide, last wins); documented in docs/stdlib.md under std.os.
            // Collection ORDER is irrelevant here — `os.environ` sorts by key before lowering.
            env: std::sync::Arc::new(std::sync::Mutex::new(
                std::env::vars_os()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.to_string_lossy().into_owned(),
                        )
                    })
                    .collect(),
            )),
            stdin: Stdin::Real,
            stream: false,
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
    /// R1 — raw bytes. Lowers to the IMMUTABLE `bytes` value (`Obj::Bytes`, Python's
    /// `open(path,'rb').read()` model); a caller wanting mutation writes `bytearray(b)`. There is
    /// deliberately no `ByteArray` variant — one seam variant, one lowering.
    Bytes(Vec<u8>),
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
/// blocking fns take int / str / bytes args; float / bool are carried for completeness.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeArg {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// R1 — raw bytes (a `bytes` arg, copied out at the boundary), pre-extracted so
    /// a BLOCKING native taking binary data (`std.io.write_bytes`) can still offload to the dirty
    /// pool. Without this variant extraction returns `None` and the call quietly runs INLINE on a
    /// core worker (right answer, pinned worker — the D5 invariant it exists to protect), so it is
    /// unprovable black-box: it is pinned by direct unit tests on `Vm::extract_native_args` +
    /// `OffloadHost::arg_bytes`.
    Bytes(Vec<u8>),
    /// An insertion-ordered `map[str, str]`, pre-extracted so a blocking native that reads a map arg
    /// (today only `std.request.request`'s custom headers) can still offload to the dirty pool: the
    /// off-heap host has no `Vm`/heap, so the map is snapshotted into owned `(String, String)` pairs
    /// before the handoff and served back via [`Host::arg_str_map`]. Order is preserved (the engine
    /// reads `MapData.entries`), so header order is deterministic across engines.
    Map(Vec<(String, String)>),
    /// An ordered `list[str]`, pre-extracted so a blocking native that reads a list-of-strings arg
    /// (today only `std.process.run_args`'s argv) can still offload to the dirty pool: the off-heap
    /// host has no `Vm`/heap, so the list is snapshotted into owned `String`s before the handoff and
    /// served back via [`Host::arg_str_list`]. Order is preserved (it IS the argv), so the spawned
    /// process sees the same argument vector across engines.
    List(Vec<String>),
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
    /// Synchronously RE-ENTER the engine to invoke the closure passed as extern arg `arg_index`,
    /// with `args` the C scalars the C library handed back to the callback trampoline. The engine
    /// fetches its own closure value at that arg index (the cffi layer never sees an engine `Value`),
    /// runs it, and lowers the result back to an engine-neutral [`NativeRet`]. This is the one seam
    /// that lets a `fn(...)`-typed extern param work on BOTH engines (the cffi layer builds a libffi
    /// trampoline whose userdata holds `*mut dyn Host` + `arg_index`; when C calls it the trampoline
    /// routes here). Sync, same-thread, scalar-only (callbacks #4): the closure fires inside the
    /// extern call on the calling thread (no GC rooting, no cross-thread hand-off). The default errors
    /// so a host that never passes callbacks (the std-module / off-heap fixtures) needn't implement it.
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[NativeRet],
    ) -> Result<NativeRet, HostError> {
        let _ = (arg_index, args);
        Err(HostError {
            message: "this host does not support callbacks".into(),
        })
    }
    /// `args[i]` as an owned string; errors if it is not a str.
    fn arg_str(&mut self, i: usize) -> Result<String, HostError>;
    /// `args[i]` as an insertion-ordered `map[str, str]`, returned as owned `(key, value)` pairs in
    /// map insertion order. Errors if the arg is not a map or any key/value is not a str. Used by
    /// `std.request.request` to read custom request headers.
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, HostError>;
    /// `args[i]` as an ordered `list[str]`, returned as owned `String`s in list order. Errors if the
    /// arg is not a list or any element is not a str. Used by `std.process.run_args` to read the argv
    /// it spawns without a shell. The default returns a "no list args" error so a host that never
    /// passes lists (the std-module test fixtures, the FFI callback host) needn't implement it.
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, HostError> {
        let _ = i;
        Err(HostError {
            message: "this host does not support list arguments".into(),
        })
    }
    /// R1 — `args[i]` as raw bytes, copied out at the boundary (no heap aliasing). A `bytes` only:
    /// every seam param is typed `bytes`, and a `bytearray` is NOT assignable to a `bytes` sink
    /// (commit 7b29552 — a mutable buffer aliased as immutable `bytes` is the hole that rule closes);
    /// a caller converts with `bytes(ba)`, exactly as in CPython. Anything else is an arg-type error.
    /// The default returns a "no bytes args" error so a host that never passes binary data (the
    /// std-module test fixtures, the FFI callback host) needn't implement it.
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, HostError> {
        let _ = i;
        Err(HostError {
            message: "this host does not support bytes arguments".into(),
        })
    }

    /// Append to the program's captured stdout buffer.
    fn write_stdout(&mut self, s: &str);
    /// Append to the program's captured stderr buffer.
    fn write_stderr(&mut self, s: &str);
    /// Read one line from the injected stdin source; `None` at EOF. The trailing newline is
    /// stripped.
    fn read_line(&mut self) -> Result<Option<String>, HostError>;
    /// Read ALL remaining stdin to EOF as one `str`; `""` at a clean EOF. DEFAULTED to EOF-equivalent
    /// (`""`) so a stdin-less test/embedder host needs no override; the real `VmHost` delegates to its
    /// shared `Stdin` source (same seam as `read_line`, so it inherits shared-stdin behavior).
    fn read_all(&mut self) -> Result<String, HostError> {
        Ok(String::new())
    }
    /// Read ONE Unicode scalar as a 1-char `str`; `None` at a clean EOF, a fault on a partial/invalid
    /// UTF-8 sequence. DEFAULTED to EOF (`None`) — same override story as `read_all`.
    fn read_char(&mut self) -> Result<Option<String>, HostError> {
        Ok(None)
    }
    /// Flush this host's stdout. DEFAULTED to a no-op, and that default is what every host in-tree
    /// uses: the captured/buffered sink has nothing to flush, and the streaming CLI's stdout is
    /// UNBUFFERED (its writer thread `flush`es every message — see `vm::stream`), so there is nothing
    /// left in a buffer to push. It stays on the trait as the seam a buffering embedder would want,
    /// and as what `io.flush()` calls. It must NEVER wait on stdout's consumer: a fiber blocked on a
    /// stalled reader pins a core worker (the D5 invariant).
    fn flush_stdout(&mut self) {}

    /// The program arguments (injected; defaults to empty).
    fn os_args(&self) -> Vec<String>;
    /// An environment variable from the injected environment.
    fn os_env(&self, key: &str) -> Option<String>;
    /// The current working directory, as RAW OS bytes (W7-8). NOT a `String`: the cwd can be any byte
    /// sequence the OS allows, and decoding it here (`display().to_string()`, lossy) was the last
    /// member of the W7-8 lossy-path family — `os.getcwd()` handed back a `str` naming nothing.
    fn os_getcwd(&self) -> Result<Vec<u8>, HostError>;
    /// ALL environment variables (from the same injected env `os_env` reads), sorted by key so the
    /// map is deterministic on both engines (the backing store is a `HashMap` with per-instance
    /// random iteration order). DEFAULTED to empty so only the real `VmHost` overrides — test/off-heap
    /// hosts inherit the inert default.
    fn os_environ(&self) -> Vec<(String, String)> {
        vec![]
    }
    /// Set an environment variable in the injected env (the SAME map `os_env`/`os_environ` read), so a
    /// `std.os.setenv` is observed by both. DEFAULTED to a no-op (test/off-heap hosts).
    fn os_setenv(&mut self, _key: String, _value: String) {}

    /// Record a cooperative-exit request (`std.os.exit(code)`). The engine stores the code and the
    /// native fn returns an error sentinel that unwinds past any `recover:` to the top level, where
    /// the driver reports the code as the process exit status. The status is the LOW 8 BITS of the
    /// code (`code & 0xff`), like POSIX `exit(3)`/bash/Python/Go: `-1` → 255, `300` → 44, `0` → 0.
    /// Default: no-op (test hosts).
    fn request_exit(&mut self, _code: i64) {}
}

/// A Rust function callable from Chezzi. A bare `fn` pointer (no captured state, no generics) so it
/// can live behind an `Rc`/`GcRef` cheaply and compare/clone trivially.
pub type NativeFn = fn(&mut dyn Host) -> Result<NativeRet, HostError>;

/// **How the engine must RUN a native** — the one behavioural property of a native fn, carried on its
/// registry entry ([`native_members`]) rather than matched by name at the dispatch site.
///
/// This is `future.md` §3c: these properties used to live in string matches far from the entry — a
/// 40-name `is_blocking` list plus three `"sleep_ms"` arms in `vm/call.rs` plus a
/// `name == "connect" || name == "listen"` check — so **a new blocking native that forgot to join the
/// list failed SILENTLY**: nothing errored, no test went red, it just pinned an M:N core worker for
/// the syscall's duration (the D5 starvation the classification exists to prevent). As a field of
/// every `MEMBERS` tuple, omitting it is a **compile error** instead.
///
/// It rides the entry → [`crate::vm::heap::Obj::Native`] (bound in `vm/exec.rs`) → `Vm::invoke_native`, so
/// the dispatch site never compares a name: no lookup, and no bare-name ambiguity (`std.io::_append`
/// and `std.fs::_append` are distinct entries with different kinds — under the old name-keyed scheme
/// they collided and were kept apart only by check ORDER plus an exemption list in a test).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Run inline on the calling worker: pure CPU (math/crypto/encoding/regex/ffi) or a call that
    /// touches the host stdio/os state (`print`, `read_line`, `now`) and so *cannot* run off-heap.
    Inline,
    /// D5 — a blocking, **off-heap-safe** syscall the M:N engine offloads to the dirty/blocking pool
    /// instead of running inline, so it can't pin a core worker (the G3 starvation). "Off-heap-safe"
    /// is the contract that lets it run on a pool thread with no `Vm`: it reads only primitive args
    /// (`arg_int`/`arg_str`/`NativeArg::{Map,List,Bytes}`) and returns a primitive [`NativeRet`] — it
    /// never touches the heap, the stdout/stderr buffers, stdin, or os state during the blocking part
    /// (so [`Host`]'s I/O methods are `unreachable!` on the off-heap host). The set: `std.io`'s four
    /// file-seam members, all of `std.fs`, `std.request` (network) and `std.process` (subprocess).
    /// Also a cancellation checkpoint on BOTH engines (see `vm/call.rs`).
    Blocking,
    /// A wait whose **deadline we own** (`std.time.sleep_ms`): it rides the timer thread (park +
    /// deadline-wake) rather than a pool thread, and is a CONTINUOUS cancellation + `--timeout`
    /// checkpoint for its whole duration (W7-16/17/18) — not merely at entry like a `read(2)` in the
    /// kernel, which is not ours to cut short. Blocking in every other respect (offload gate, entry
    /// cancel checkpoint).
    TimedWait,
    /// R2 — a `std.io` Writer/Reader opener or handle (`_create`/`_append`/`stdout`/`stderr`/
    /// `buffered`/`_open`). It allocates a heap `Writer`/`Reader` over an `Arc`'d core, which a pure
    /// off-heap native cannot, so the engine runs it itself (`Vm::io_native`) and the registered fn
    /// (`io::intercepted`) never executes.
    InterceptIo,
    /// D6 — `std.net.connect`/`listen`. Same reason as [`Kind::InterceptIo`] (allocates a
    /// `Socket`/`Listener` handle over an `Arc`'d core); run by `Vm::net_connect_or_listen`, and the
    /// registered `net::intercepted` placeholder never executes.
    InterceptNet,
}

impl Kind {
    /// Does this native block its worker long enough to need the D5 treatment — the M:N offload gate
    /// and the entry cancellation checkpoint? True for [`Kind::Blocking`] and [`Kind::TimedWait`]
    /// (the two arms of the old `is_blocking` name list); false for everything the engine runs inline
    /// or intercepts.
    ///
    /// EXHAUSTIVE on purpose (no `_` arm): a future `Kind` must be classified here deliberately, or it
    /// does not compile. A catch-all would silently default a new variant to "does not block" — the
    /// same silent-omission failure this enum exists to abolish, one level up.
    pub fn blocks(self) -> bool {
        match self {
            Kind::Blocking | Kind::TimedWait => true,
            Kind::Inline | Kind::InterceptIo | Kind::InterceptNet => false,
        }
    }
}

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

/// Helper for native functions with an optional trailing arg: assert `min..=max` arguments. The
/// runtime mirror of the checker's `FnSig::optional_tail` arity range (used by `std.request`'s
/// optional `timeout_ms`). `min == max` reproduces [`expect_args`]' exact-arity message.
pub fn expect_args_range(
    h: &dyn Host,
    name: &str,
    min: usize,
    max: usize,
) -> Result<(), HostError> {
    let got = h.arg_count();
    if (min..=max).contains(&got) {
        Ok(())
    } else if min == max {
        Err(HostError {
            message: format!("{name}() expects {min} argument(s), got {got}"),
        })
    } else {
        Err(HostError {
            message: format!("{name}() expects {min}–{max} argument(s), got {got}"),
        })
    }
}

/// If this dotted import path names a native (virtual, no-file) std module, return its canonical
/// `'static` name. `std.string` is intentionally absent: it is a real Chezzi file under the stdlib dir.
pub fn native_name(path: &[String]) -> Option<&'static str> {
    match path {
        [a, b] if a == "std" => match b.as_str() {
            "math" => Some("std.math"),
            "io" => Some("std.io"),
            "os" => Some("std.os"),
            "process" => Some("std.process"),
            "rand" => Some("std.rand"),
            "fs" => Some("std.fs"),
            "time" => Some("std.time"),
            "regex" => Some("std.regex"),
            "request" => Some("std.request"),
            "net" => Some("std.net"),
            "ffi" => Some("std.ffi"),
            "encoding" => Some("std.encoding"),
            "crypto" => Some("std.crypto"),
            "uuid" => Some("std.uuid"),
            // `std.concurrency` is a FILE-BACKED native module (phase 4c-concurrency): it has NO
            // callable members — it only declares the four runtime concurrency TYPE/ctor names
            // (`Shared`/`RwShared`/`Atomic`/`Executor`) as `native struct`s in `std/concurrency.chz`,
            // harvested for their sigs + method tables (the ctors still lower via the compiler's
            // name→opcode dispatch, not a bound module member). Only the len-2 path is the native module
            // (loads `std/concurrency.chz`); `import std.concurrency.collection` (len-3) falls through
            // to load `std/concurrency/collection.chz` as a real (non-native) file.
            "concurrency" => Some("std.concurrency"),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a native std module is FILE-BACKED (its whole signature is declared in a real `std/<M>.chz`
/// with bodyless `native fn`/`native struct` decls, harvested by the checker) rather than hand-built in
/// `native_module_sig`. The resolver loads the real file (`visit_native_file`) and the checker harvests
/// its decls (`harvest_native_module`) — both gates MUST agree, so they share this one predicate to keep
/// the sig-source (file) and AST-source (file) provably in lockstep. Runtime member dispatch stays
/// name-keyed via `native_members` for these exactly as for the virtual ones (this is a front-end-only
/// distinction). `std.time` is file-backed for its 4 real fns but ALSO keeps a minimal
/// `native_module_sig` arm for its opcode-backed `timer` type-license (no runtime member value).
/// Phases: std.regex (4b); std.encoding/crypto/uuid/time (4e); std.process/request (4f);
/// std.math/io/os/rand/fs (4d); std.ffi (4c-ffi) — file-backed for its 59 real fns but ALSO keeps a
/// minimal `native_module_sig` arm for its opcode/type-license names (`ptr` + the fixed-width int
/// names — no runtime member value), like `std.time`'s `timer`; std.net (4c-net — native structs WITH
/// harvested method tables); std.concurrency (4c-concurrency — the four GENERIC native structs
/// Shared/RwShared/Atomic/Executor WITH harvested method tables, the LAST migration: after it every
/// native std module is file-backed, and its arm is DELETED entirely — no opcode/type-license residual
/// remains in `native_module_sig` for it since the four type names are harvested from the file).
pub fn is_file_backed_native(name: &str) -> bool {
    matches!(
        name,
        "std.regex"
            | "std.encoding"
            | "std.crypto"
            | "std.uuid"
            | "std.time"
            | "std.process"
            | "std.request"
            | "std.math"
            | "std.io"
            | "std.os"
            | "std.rand"
            | "std.fs"
            | "std.ffi"
            | "std.net"
            | "std.concurrency"
    )
}

/// The callable members of a native module, as `(name, fn, kind)`. Single source of truth shared by
/// both engines (only the per-engine lowering and the checker's static signatures differ) — and, since
/// `future.md` §3c, the single source of truth for HOW each one is run too ([`Kind`]). Empty for an
/// unknown name.
pub fn native_members(module: &str) -> &'static [(&'static str, NativeFn, Kind)] {
    match module {
        "std.math" => math::MEMBERS,
        "std.io" => io::MEMBERS,
        "std.os" => os::MEMBERS,
        "std.process" => process::MEMBERS,
        "std.rand" => rand::MEMBERS,
        "std.fs" => fs::MEMBERS,
        "std.time" => time::MEMBERS,
        "std.regex" => regex::MEMBERS,
        "std.request" => request::MEMBERS,
        "std.net" => net::MEMBERS,
        "std.ffi" => ffi::MEMBERS,
        "std.encoding" => encoding::MEMBERS,
        "std.crypto" => crypto::MEMBERS,
        "std.uuid" => uuid::MEMBERS,
        // A type-licensing-only native module: no callable members (it carries the four concurrency
        // ctor TYPE names, which have no runtime value — they lower via the compiler name→opcode path).
        "std.concurrency" => &[],
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

    /// Every native module with callable members — the domain the [`Kind`] assertions sweep.
    ///
    /// A hand-kept list is exactly the smell [`Kind`] exists to remove, so it is not hand-kept:
    /// `native_modules_matches_the_member_tables` below reads THIS FILE and derives the same set from
    /// [`native_members`]' match arms, both directions. Without that, adding a module and forgetting
    /// the list would leave every sweep below green while never looking at the new module — the
    /// silent-omission failure, relocated into the test harness.
    const NATIVE_MODULES: &[&str] = &[
        "std.math",
        "std.io",
        "std.os",
        "std.process",
        "std.rand",
        "std.fs",
        "std.time",
        "std.regex",
        "std.request",
        "std.net",
        "std.ffi",
        "std.encoding",
        "std.crypto",
        "std.uuid",
    ];

    /// Each name in [`NATIVE_MODULES`] really names a native module with a member table — so a typo or
    /// a module renamed out from under the list turns the kind sweeps into silent no-ops.
    #[test]
    fn native_modules_all_have_members() {
        for module in NATIVE_MODULES {
            let path: Vec<String> = module.split('.').map(str::to_string).collect();
            assert_eq!(native_name(&path), Some(*module), "not a native module");
            assert!(
                !native_members(module).is_empty(),
                "{module} has no callable members"
            );
        }
    }

    /// …and the OTHER direction: every `"std.x" => x::MEMBERS` arm of [`native_members`] is in
    /// [`NATIVE_MODULES`]. Source-derived (`include_str!` of this file + the arm pattern) because a
    /// second hand-maintained list of natives is precisely what [`Kind`] was introduced to abolish: a
    /// new module missing from the const would leave `std_time_sleep_is_the_only_timed_wait` and the
    /// intercept-exclusivity sweep GREEN while never examining it, and the engine would then route,
    /// say, a stray `Kind::InterceptNet` member into `net_connect_or_listen`'s name dispatch.
    #[test]
    fn native_modules_matches_the_member_tables() {
        let src = include_str!("mod.rs");
        let body = {
            let start = src
                .find("pub fn native_members(")
                .expect("native_members moved — update this fence");
            let rest = &src[start..];
            &rest[..rest.find("\n}\n").expect("unterminated native_members")]
        };
        let arms: std::collections::BTreeSet<&str> = body
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                // `"std.math" => math::MEMBERS,` — the empty `std.concurrency => &[]` arm has no
                // `::MEMBERS` and is skipped, matching the const.
                let name = l.strip_prefix('"')?.split('"').next()?;
                l.contains("::MEMBERS").then_some(name)
            })
            .collect();
        let listed: std::collections::BTreeSet<&str> = NATIVE_MODULES.iter().copied().collect();
        assert_eq!(
            arms, listed,
            "NATIVE_MODULES is out of step with native_members' match arms"
        );
    }

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
        fn os_getcwd(&self) -> Result<Vec<u8>, HostError> {
            Ok(b"/".to_vec())
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
    fn expect_args_range_accepts_optional_tail() {
        // min=1, max=2: arity 1 and 2 are both Ok; 0 and 3 are Err with a range message.
        let h1 = MockHost {
            ints: vec![1],
            ..Default::default()
        };
        assert!(expect_args_range(&h1, "get", 1, 2).is_ok());
        let h2 = MockHost {
            ints: vec![1, 2],
            ..Default::default()
        };
        assert!(expect_args_range(&h2, "get", 1, 2).is_ok());
        let h0 = MockHost {
            ints: vec![],
            ..Default::default()
        };
        assert_eq!(
            expect_args_range(&h0, "get", 1, 2),
            Err(HostError {
                message: "get() expects 1–2 argument(s), got 0".into()
            })
        );
        let h3 = MockHost {
            ints: vec![1, 2, 3],
            ..Default::default()
        };
        assert_eq!(
            expect_args_range(&h3, "get", 1, 2),
            Err(HostError {
                message: "get() expects 1–2 argument(s), got 3".into()
            })
        );
    }

    /// `std.rand` is a native (virtual, no-file) module: it resolves to a canonical name and exposes
    /// its four scalar members. (Their kinds are asserted by
    /// `every_pure_and_host_state_module_member_is_inline` — draws are inline CPU, not I/O.)
    #[test]
    fn native_rand_module_is_wired() {
        assert_eq!(
            native_name(&["std".into(), "rand".into()]),
            Some("std.rand")
        );
        assert_eq!(native_members("std.rand").len(), 4);
        let names: Vec<&str> = native_members("std.rand")
            .iter()
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(names, ["seed", "float", "int", "bool"]);
    }

    /// `std.encoding` / `std.crypto` / `std.uuid` are native (virtual) modules: each resolves to a
    /// canonical name and exposes exactly these members. (Their kinds — all inline, since they are pure
    /// CPU str transforms / RNG draws — are asserted by
    /// `every_pure_and_host_state_module_member_is_inline`.)
    #[test]
    fn native_encoding_crypto_uuid_wired() {
        assert_eq!(
            native_name(&["std".into(), "encoding".into()]),
            Some("std.encoding")
        );
        assert_eq!(
            native_name(&["std".into(), "crypto".into()]),
            Some("std.crypto")
        );
        assert_eq!(
            native_name(&["std".into(), "uuid".into()]),
            Some("std.uuid")
        );

        let enc: Vec<&str> = native_members("std.encoding")
            .iter()
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(
            enc,
            [
                "base64_encode",
                "base64_encode_url",
                "base64_decode",
                "base64_decode_url",
                "base64_encode_bytes",
                "base64_decode_bytes",
                "hex_encode",
                "hex_decode",
                "url_encode",
                "url_decode",
                "query_encode",
                "query_decode",
                "url_parse",
            ]
        );
        let cry: Vec<&str> = native_members("std.crypto")
            .iter()
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(
            cry,
            [
                "sha256",
                "sha256_bytes",
                "sha1",
                "sha1_bytes",
                "sha512",
                "sha512_bytes",
                "md5",
                "hmac_sha256",
                "secure_bytes",
                "token_hex"
            ]
        );
        let uid: Vec<&str> = native_members("std.uuid")
            .iter()
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(uid, ["v4", "uuid_seed"]);
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
        assert_eq!(native_name(&["std".into(), "string".into()]), None);
        // user modules are never native.
        assert_eq!(native_name(&["foo".into(), "math".into()]), None);
        assert_eq!(native_name(&["std".into()]), None);
        // `std.concurrency` is a file-less native (type-licensing-only) module at the len-2 path...
        assert_eq!(
            native_name(&["std".into(), "concurrency".into()]),
            Some("std.concurrency")
        );
        // ...but it has NO callable members (it only licenses the four concurrency ctor TYPE names).
        assert!(native_members("std.concurrency").is_empty());
        // The len-3 `std.concurrency.collection` is the REAL file — NOT native (no collision).
        assert_eq!(
            native_name(&["std".into(), "concurrency".into(), "collection".into()]),
            None
        );
    }

    /// Every member of a module, as `(name, kind)` — the shape the kind assertions below compare.
    fn kinds(module: &str) -> Vec<(&'static str, Kind)> {
        native_members(module)
            .iter()
            .map(|(n, _, k)| (*n, *k))
            .collect()
    }

    /// [`Kind::blocks`] — the predicate the D5 offload gate and the entry cancellation checkpoint use —
    /// is true for exactly the two waiting kinds. It replaced the old `is_blocking` name list, whose
    /// membership was the thing a new native could silently forget.
    #[test]
    fn only_the_waiting_kinds_block() {
        assert!(Kind::Blocking.blocks());
        assert!(Kind::TimedWait.blocks());
        assert!(!Kind::Inline.blocks());
        assert!(!Kind::InterceptIo.blocks());
        assert!(!Kind::InterceptNet.blocks());
    }

    /// D5 — every member of `std.fs` (filesystem syscalls), `std.request` (HTTP via `ureq`) and
    /// `std.process` (subprocess) is off-heap-safe blocking work: primitive args, primitive returns
    /// (`Struct`/`Ok(Str)`/`Err`), no heap/stdio touch during the blocking call. All must carry
    /// [`Kind::Blocking`] so the M:N engine routes them through the dirty pool instead of pinning a core
    /// worker. Iterating MEMBERS (not a hand-copied name list) is the point: a future verb added without
    /// a kind fails to COMPILE, and one added with the WRONG kind fails here. W7-19 — EXCEPTION-FREE
    /// since 2026-08-05: `fs._stat`/`fs._walk` were carved out here while they still ran inline.
    #[test]
    fn every_syscall_module_member_is_blocking() {
        for module in ["std.fs", "std.request", "std.process"] {
            for (name, kind) in kinds(module) {
                assert_eq!(kind, Kind::Blocking, "{module}.{name} must be blocking");
            }
        }
    }

    /// `std.process` exposes exactly `cmd`/`run`/`run_args` + the W6-4 bytes twins
    /// `run_bytes`/`run_args_bytes`.
    #[test]
    fn native_process_members_and_blocking() {
        let names: Vec<&str> = native_members("std.process")
            .iter()
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(
            names,
            vec!["cmd", "run", "run_args", "run_bytes", "run_args_bytes"]
        );
    }

    /// `NativeArg::List` carries an ordered `list[str]` across the off-heap offload boundary so
    /// `run_args`'s argv survives the handoff to the dirty pool. The off-heap host serves it back.
    #[test]
    fn native_arg_list_variant_carries_str_vec() {
        let a = NativeArg::List(vec!["a".into(), "b".into()]);
        let b = NativeArg::List(vec!["a".into(), "b".into()]);
        assert_eq!(a, b);
        assert_ne!(a, NativeArg::List(vec![]));
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

    /// Fast / pure / host-I/O natives must NOT be offloaded: pure CPU transforms (math, crypto,
    /// encoding, regex, ffi, the RNG draws) are cheap, and the `std.os` members touch process state the
    /// off-heap host cannot serve. Mislabeling one as blocking would bounce it through the pool for
    /// nothing — or, for a host-state member, reach an `unreachable!` on the off-heap host.
    #[test]
    fn every_pure_and_host_state_module_member_is_inline() {
        for module in [
            "std.math",
            "std.crypto",
            "std.encoding",
            "std.uuid",
            "std.rand",
            "std.regex",
            "std.os",
            "std.ffi",
        ] {
            for (name, kind) in kinds(module) {
                assert_eq!(kind, Kind::Inline, "{module}.{name} must run inline");
            }
        }
    }

    /// `std.time` — `sleep_ms` is the one native whose deadline the ENGINE owns, so it is a
    /// [`Kind::TimedWait`]: it rides the timer thread and stays a cancellation + `--timeout` checkpoint
    /// for its whole duration (W7-16/17/18). The clock reads beside it are plain inline calls.
    #[test]
    fn std_time_sleep_is_the_only_timed_wait() {
        assert_eq!(
            kinds("std.time"),
            vec![
                ("now", Kind::Inline),
                ("monotonic", Kind::Inline),
                ("sleep_ms", Kind::TimedWait),
                ("format", Kind::Inline),
            ]
        );
        // …and no OTHER module smuggles one in: the engine's timed-wait paths (offload timer, callback
        // demote, block-in-place) all key on this kind alone.
        for module in NATIVE_MODULES {
            for (name, kind) in kinds(module) {
                assert!(
                    kind != Kind::TimedWait || (*module == "std.time" && name == "sleep_ms"),
                    "{module}.{name} is a TimedWait — only std.time.sleep_ms may be"
                );
            }
        }
    }

    /// `std.io` splits three ways: the four file seams are dirty-pool [`Kind::Blocking`]; the six
    /// Writer/Reader openers are [`Kind::InterceptIo`] (the engine runs them — they allocate a heap
    /// handle a pure off-heap native cannot); the rest touch host stdio and run inline. `std.net`'s two
    /// members are [`Kind::InterceptNet`] for the same handle-allocating reason.
    ///
    /// This is also where the OLD name-keyed scheme was unsound: `std.io::_append` (an opener) and
    /// `std.fs::_append` (a syscall) share a bare name, and were told apart only by the ORDER of the
    /// checks in `invoke_native` plus a func-pointer identity test.
    #[test]
    fn io_and_net_members_carry_their_intercept_and_blocking_kinds() {
        assert_eq!(
            kinds("std.io"),
            vec![
                ("print", Kind::Inline),
                ("eprint", Kind::Inline),
                ("read_line", Kind::Inline),
                ("read_all", Kind::Inline),
                ("read_char", Kind::Inline),
                ("flush", Kind::Inline),
                ("isatty", Kind::Inline),
                ("isatty_stdin", Kind::Inline),
                ("isatty_stderr", Kind::Inline),
                ("input", Kind::Inline),
                ("_read_file", Kind::Blocking),
                ("_write_file", Kind::Blocking),
                // R1 — the binary whole-file twins (`write_bytes` offloads via `NativeArg::Bytes`).
                ("_read_bytes", Kind::Blocking),
                ("_write_bytes", Kind::Blocking),
                ("_create", Kind::InterceptIo),
                ("_append", Kind::InterceptIo),
                ("stdout", Kind::InterceptIo),
                ("stderr", Kind::InterceptIo),
                ("buffered", Kind::InterceptIo),
                ("_open", Kind::InterceptIo),
            ]
        );
        assert_eq!(
            kinds("std.net"),
            vec![
                ("connect", Kind::InterceptNet),
                ("listen", Kind::InterceptNet),
            ]
        );
        // The two intercept kinds are exclusive to those modules — `invoke_native` dispatches an
        // InterceptIo to `io_native` and an InterceptNet to `net_connect_or_listen` by kind alone, so a
        // stray one elsewhere would be routed to a handler that does not know its name.
        for module in NATIVE_MODULES {
            for (name, kind) in kinds(module) {
                match kind {
                    Kind::InterceptIo => assert_eq!(*module, "std.io", "{module}.{name}"),
                    Kind::InterceptNet => assert_eq!(*module, "std.net", "{module}.{name}"),
                    _ => {}
                }
            }
        }
    }
}
