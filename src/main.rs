//! Chezzi — a fast, statically-typed, Python-feel scripting language.
//!
//! Design spec: docs/spec.md
//!
//! Pipeline (built incrementally — see roadmap in docs/spec.md):
//!   source.chz → lexer → parser → checker → compiler → bytecode VM
//!
//! Status: pre-M1 scaffold. Subcommands below are stubs.

// `src/main.rs` is a thin CLI shim over the `chezzi` **library** crate (`src/lib.rs`): the front-end
// modules live there as `pub mod`s and compile once, so this binary declares no modules of its own —
// it just imports the pieces the CLI body drives. (The grammar `conformance` suite and the two-engine
// serial-VM/M:N-VM parity tests now live + run once in the lib's test target, not in this bin.)
use chezzi::{checker, lexer, manifest, native, parser, resolver, test_runner, vm};

use std::process::ExitCode;

const USAGE: &str = "\
chezzi — the Chezzi language toolchain

USAGE:
    chezzi <command> [flags] <file.chz> [program args...]

COMMANDS:
    init    [dir]    Scaffold a new Chezzi project (manifest + src)
    run     [file]   Type-check, then run on the bytecode VM (no file → manifest entrypoint)
    test    [path]   Run every `test fn` in *_test.chz files
    check   <file>   Type-check only; report errors
    tokens  <file>   Print the token stream
    ast     <file>   Print the parsed AST
    docs    [topic]  Print language docs (no topic → full reference, for piping to an LLM)
    help             Show this message

FLAGS:
    --errors=json    Emit type errors as JSON (for `check` / `run`)
    --serial         Run on the cooperative single-thread VM (the frozen parity oracle)
    --parallel       Select the OS-thread engine (now the DEFAULT; flag kept as a no-op)
    --threads=N      Worker threads for the OS-thread engine (0 = all cores; env: CHEZZI_THREADS)
    --check-parity   Run the program on BOTH engines (serial oracle + M:N) and report whether their
                     output is byte-identical (exit 0 = parity OK; non-zero = divergence report)
    --max-heap=N     (`test`, M:N engine only) Hard-abort any test whose live heap exceeds N bytes — a
                     runaway-alloc guard, bucketed OVER-MEMORY (0/omitted = off; not with --serial)

NOTE: flags must come BEFORE the file path. Anything after the file is passed
      to the program as an argument, so `chezzi run prog.chz --serial` runs the
      default parallel VM and hands `--serial` to the program. Use `chezzi run --serial prog.chz`.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "tokens" => cmd_tokens(args.get(1)),
        "ast" => cmd_ast(args.get(1)),
        "check" => cmd_check(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "test" => cmd_test(&args[1..]),
        "init" => cmd_init(&args[1..]),
        "docs" => cmd_docs(&args[1..]),
        other => {
            eprintln!("chezzi: unknown command '{other}'\n");
            print!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

/// `chezzi tokens <file>` — read a source file, run the lexer, print the token stream.
///
/// This is plumbing (provided). It calls into YOUR `lexer` module. Until you implement the
/// lexer's `todo!()`s, running this will panic at the `todo!` — that's expected.
fn cmd_tokens(path: Option<&String>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("chezzi tokens: missing file argument\nusage: chezzi tokens <file.chz>");
        return ExitCode::FAILURE;
    };

    let Some(source) = read_source(path) else {
        return ExitCode::FAILURE;
    };

    match lexer::tokenize(&source) {
        Ok(tokens) => {
            for tok in &tokens {
                println!("{:?}", tok.kind);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `chezzi ast <file>` — lex, parse, and pretty-print the AST. (M2)
fn cmd_ast(path: Option<&String>) -> ExitCode {
    let Some(path) = path else {
        eprintln!("chezzi ast: missing file argument\nusage: chezzi ast <file.chz>");
        return ExitCode::FAILURE;
    };

    let Some(source) = read_source(path) else {
        return ExitCode::FAILURE;
    };

    let tokens = match lexer::tokenize(&source) {
        Ok(tokens) => tokens,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    match parser::parse(tokens) {
        Ok(module) => {
            println!("{module:#?}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// `chezzi check <file> [--errors=json]` — lex, parse, and type-check, reporting all errors. (M4)
fn cmd_check(args: &[String]) -> ExitCode {
    let Some((path, json)) = parse_file_and_flags("check", args) else {
        return ExitCode::FAILURE;
    };
    if read_source(&path).is_none() {
        return ExitCode::FAILURE;
    }

    // `chezzi check` is file-only (no bare/manifest mode), so the root is the nearest marker walking
    // up from the file — pass `None`.
    match type_check(&path, None) {
        CheckOutcome::Ok => {
            println!("{}", if json { "[]" } else { "ok: no type errors" });
            ExitCode::SUCCESS
        }
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, json);
            ExitCode::FAILURE
        }
        CheckOutcome::Fatal {
            text,
            message,
            line,
            col,
        } => {
            report_fatal(&text, &message, line, col, json);
            ExitCode::FAILURE
        }
    }
}

/// `chezzi run [file] [--errors=json] [--serial] [--parallel] [--threads=N] [--check-parity]` —
/// type-check first,
/// then execute on the bytecode VM. With NO file argument, the project's manifest entrypoint is run:
/// the project root is found by walking up from the cwd for `chezzi.toml`, and its
/// `[project] entrypoint` (a dotted module path, e.g. `"src.main"`) is resolved root-relatively and
/// run. The VM now runs the real OS-thread engine BY DEFAULT; `--serial` opts back into the
/// cooperative single-thread VM (the frozen byte-identical oracle). `--parallel` is kept as an
/// accepted no-op alias for the (now default) OS-thread engine. `--threads=N` (or the
/// `CHEZZI_THREADS` env var) sizes the OS-thread engine's worker pool — `0` (or omitted) = all
/// cores; the flag wins over the env var. `--check-parity` instead runs the program on BOTH engines
/// (serial oracle + M:N) into buffered sinks and diffs them — the in-tree serial==M:N parity oracle
/// as a one-command check; it is mutually exclusive with `--serial`/`--parallel`.
fn cmd_run(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut json = false;
    // The OS-thread engine (bounded pool + condvar `recv`) is now the DEFAULT VM engine.
    // `--serial` opts back into the cooperative single-thread VM (the frozen parity oracle).
    let mut parallel = true;
    // Track explicit `--parallel`/`--serial` so contradictory combos still error instead of
    // silently picking one.
    let mut saw_parallel = false;
    let mut saw_serial = false;
    // `--check-parity` runs the program on BOTH engines (serial oracle + M:N) and diffs their
    // captured output — the test-only serial==M:N parity oracle, exposed as a one-command check.
    let mut saw_check_parity = false;
    // `--threads=N` worker count for the M:N engine (orthogonal to which engine runs). `0` = all
    // cores. `None` = unset (fall through to `CHEZZI_THREADS`, then auto). The flag wins over env.
    let mut threads_flag: Option<usize> = None;
    // Positional args after the script path are the program's own args (std.os.args).
    // GOTCHA: this means flags MUST precede the file — `chezzi run prog.chz --serial`
    // treats `--serial` as a program arg (path is already set) and silently runs the
    // default parallel VM. Correct form: `chezzi run --serial prog.chz`.
    let mut prog_args: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            _ if path.is_some() => prog_args.push(arg.clone()),
            "--errors=json" => json = true,
            "--parallel" => saw_parallel = true,
            "--check-parity" => saw_check_parity = true,
            "--serial" => {
                saw_serial = true;
                parallel = false;
            }
            s if s.starts_with("--threads=") => {
                let v = &s["--threads=".len()..];
                match v.parse::<usize>() {
                    Ok(n) => threads_flag = Some(n),
                    Err(_) => {
                        eprintln!(
                            "chezzi run: --threads expects a non-negative integer (0 = all cores), got '{v}'"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("chezzi run: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
            other => path = Some(other.to_string()),
        }
    }
    if saw_parallel && saw_serial {
        eprintln!("chezzi run: --parallel and --serial are mutually exclusive");
        return ExitCode::FAILURE;
    }
    if saw_check_parity && (saw_parallel || saw_serial) {
        eprintln!(
            "chezzi run: --check-parity runs BOTH engines and is mutually exclusive with --serial/--parallel"
        );
        return ExitCode::FAILURE;
    }

    // No file argument → run the project manifest's entrypoint. Find the project root by walking up
    // from the cwd for `chezzi.toml`, parse it, and resolve `[project] entrypoint` (a dotted module
    // path) root-relatively. This keeps imports root-relative (build_graph walks up to the same
    // marker), so a bare `chezzi run` from anywhere in the project runs the configured entry.
    // An explicit `chezzi run <file>` is script-mode (run the file's top-level, no entry fn). The
    // bare `chezzi run` resolves the manifest entrypoint, which MAY name a function to call
    // (`module:function`) — `entry_fn` is `Some` only in that case.
    // `root_override` enforces the "one root per run" invariant. Bare `chezzi run` (no file) pins the
    // module-graph root to the manifest that declared the entrypoint (found by walking up from the
    // cwd) — computed ONCE here and reused for BOTH the pre-run type check AND the VM run, so every
    // `import` resolves against the SAME root that located the entry file (never a nested chezzi.toml
    // the entry sits under). An explicit `chezzi run <file>` keeps `None` → the graph builders derive
    // the root by walking up from the file (nearest marker, the conventional file-run behavior).
    let (path, entry_fn, root_override): (String, Option<String>, Option<std::path::PathBuf>) =
        match path {
            Some(p) => (p, None, None),
            None => match resolve_entrypoint() {
                Ok((p, f, root)) => (p, f, Some(root)),
                Err(msg) => {
                    eprintln!("{msg}");
                    return ExitCode::FAILURE;
                }
            },
        };

    // Resolve the M:N worker count. An explicit `--threads` wins and errors if the parallel engine
    // won't run (contradiction); otherwise `CHEZZI_THREADS` applies only when it actually will, so a
    // stray env var never breaks `--serial`. `0`/unset both mean auto (all cores).
    // `--check-parity` runs an M:N leg, so `--threads=N` legitimately sizes it (the serial leg
    // ignores the count) — don't let the "no effect with --serial" guard fire for it.
    let runs_parallel = parallel || saw_check_parity;
    if let Some(n) = threads_flag {
        if !runs_parallel {
            eprintln!(
                "chezzi run: --threads sizes the parallel engine and has no effect with --serial (the cooperative single-thread engine)"
            );
            return ExitCode::FAILURE;
        }
        vm::set_worker_count(n);
    } else if runs_parallel && let Ok(raw) = std::env::var("CHEZZI_THREADS") {
        let s = raw.trim();
        if !s.is_empty() {
            match s.parse::<usize>() {
                Ok(n) => vm::set_worker_count(n),
                Err(_) => eprintln!(
                    "chezzi run: ignoring invalid CHEZZI_THREADS='{s}' (expected a non-negative integer; 0 = all cores)"
                ),
            }
        }
    }

    if read_source(&path).is_none() {
        return ExitCode::FAILURE;
    }

    // Pre-run type check: type errors block execution (no partial output). Pins the SAME root the VM
    // will run (below), so the checker and the VM never disagree on which same-named module to load.
    match type_check(&path, root_override.as_deref()) {
        CheckOutcome::Ok => {}
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, json);
            return ExitCode::FAILURE;
        }
        CheckOutcome::Fatal {
            text,
            message,
            line,
            col,
        } => {
            report_fatal(&text, &message, line, col, json);
            return ExitCode::FAILURE;
        }
    }

    // `--check-parity`: run the SAME program on BOTH engines (serial oracle + M:N), buffer each
    // leg's output, and diff — the in-tree serial==M:N parity oracle as a one-command user check.
    if saw_check_parity {
        return run_check_parity(&path, prog_args, entry_fn.as_deref(), root_override);
    }

    // The CLI STREAMS: the VM writes each `print` straight to the real stdout as it happens (see
    // `HostConfig::stream`), so a prompt appears before its `read_line`, a long-running program is
    // not silent, and a spawned task's log is visible before its nursery joins. The lib helpers keep
    // the buffered sink (the parity oracle), so `out`/`err` come back empty here. The native std
    // modules read args/env/stdin from a process-backed config.
    let p = std::path::Path::new(&path);
    let mut cfg = native::HostConfig::from_process(prog_args);
    cfg.stream = true;
    let (errored, exit_code) = {
        let (_out, _err, result, code) =
            vm::run_file_with_entry(p, cfg, parallel, entry_fn.as_deref(), root_override.clone());
        (
            result
                .err()
                .map(|e| vm::format_trace(&e.message, e.span, &e.trace)),
            code,
        )
    };
    // Drain + flush the stream writers before ANY exit path, so a trailing `print(…, end="")`, an
    // `os.exit` mid-line or a fatal trace does not lose its bytes.
    vm::flush_stream();
    // The exit status is decided HERE — the writer threads only record (`vm::stream`).
    if let Some(msg) = &errored {
        eprintln!("{msg}");
    }
    // A stdout write that failed for anything but a closed reader (`> /dev/full`, a closed fd): the
    // output is truncated, so the run must not report success. A closed READER (`| head -1`) is a
    // clean end — the VM halted itself at its next `print` and its own status stands.
    if let Some(e) = vm::stream_error() {
        eprintln!("chezzi run: cannot write stdout: {e}");
        return ExitCode::FAILURE;
    }
    // A last `print` into a just-closed pipe drops bytes but has NO next print site to fault at, so
    // the in-VM `stream_halt` never fired and `errored` is None (gap N1). `flush_stream()` above
    // blocked on the writer ack, so OUT_DEAD is now FINAL: fail deterministically instead of the old
    // race (exit 0 if the VM outran the writer's EPIPE, 1 if it lost) — matching Python's
    // `BrokenPipeError`. Outranked by a non-broken-pipe `stream_error` (handled just above) and by
    // `os.exit` (the `exit_code.is_none()` guard + the block below); skipped when the VM already
    // faulted (`errored.is_none()`), so no double-report. Same phrase as the mid-program fault.
    if errored.is_none()
        && exit_code.is_none()
        && let Some(why) = vm::out_dead_reason()
    {
        eprintln!("chezzi run: {why}");
        return ExitCode::FAILURE;
    }
    // `std.os.exit(code)` takes precedence: a clean halt with the requested status.
    if let Some(code) = exit_code {
        return ExitCode::from(code as u8);
    }
    match errored {
        None => ExitCode::SUCCESS,
        Some(_) => ExitCode::FAILURE,
    }
}

/// `chezzi run --check-parity <file>` — run the SAME program on BOTH engines (the cooperative serial
/// oracle, then the M:N OS-thread engine), each into a BUFFERED sink, and diff the two captures the
/// way the in-tree oracle `assert_file_parity` does: byte-identical stdout, byte-identical stderr,
/// identical rendered terminal error (exit code ignored, matching the oracle). Identical → print the
/// captured output once + `parity OK` (exit 0, even if BOTH engines errored identically). Diverged →
/// a greppable side-by-side report + non-zero exit (a divergence is a signal to investigate:
/// concurrency order-dependence, an airlock/scheduler fault, or an accepted --parallel-only path).
///
/// NOTE: both legs share the real process stdin fd and run sequentially, so a stdin-reading program
/// diverges by construction (leg 2 sees EOF) — check-parity does not support stdin-reading programs.
fn run_check_parity(
    path: &str,
    prog_args: Vec<String>,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
) -> ExitCode {
    let p = std::path::Path::new(path);
    // `stream` stays false (the `from_process` default) so out/err come back FULLY BUFFERED. Each leg
    // needs its own config: `HostConfig` is not `Clone` and `from_process` consumes the Vec.
    let cfg1 = native::HostConfig::from_process(prog_args.clone());
    let cfg2 = native::HostConfig::from_process(prog_args);
    let (o1, e1, r1, _) = vm::run_file_with_entry(p, cfg1, false, entry_fn, root.clone());
    let (o2, e2, r2, _) = vm::run_file_with_entry(p, cfg2, true, entry_fn, root);
    // Compare terminal faults EXACTLY as the in-tree parity oracle does — `RunError`'s `Display`
    // (`to_string()` = "runtime error ({span}): {message}"), NOT the full stack trace. The trace frames
    // (`at <fn> (called at <site>)`) can legitimately differ between engines when M:N picks a different
    // first-fault winner among tasks that fault identically, and the oracle holds such a program equal;
    // diffing the trace here would false-report divergence. Mirror `assert_file_parity` (parity_tests.rs).
    let err1 = r1.err().map(|e| e.to_string());
    let err2 = r2.err().map(|e| e.to_string());

    if o1 == o2 && e1 == e2 && err1 == err2 {
        print!("{o1}");
        eprint!("{e1}");
        if let Some(m) = &err1 {
            eprintln!("{m}");
        }
        eprintln!("parity OK (serial == M:N)");
        return ExitCode::SUCCESS;
    }

    // First differing stream wins the report: stdout, then stderr, then the terminal error.
    eprintln!("parity DIVERGENCE (serial != M:N)");
    if o1 != o2 {
        report_stream_diff("stdout", &o1, &o2);
    } else if e1 != e2 {
        report_stream_diff("stderr", &e1, &e2);
    } else {
        let a = err1.as_deref().unwrap_or("<ok>");
        let b = err2.as_deref().unwrap_or("<ok>");
        eprintln!("  terminal error differs:");
        eprintln!("    serial: {a}");
        eprintln!("    M:N:    {b}");
    }
    ExitCode::FAILURE
}

/// Emit a greppable side-by-side report for a stream that differs between the two engines: the first
/// differing line index, then both sides. Handles differing line counts (a missing line shows as
/// `<none>`).
fn report_stream_diff(stream: &str, serial: &str, mn: &str) {
    let a: Vec<&str> = serial.lines().collect();
    let b: Vec<&str> = mn.lines().collect();
    let n = a.len().max(b.len());
    for i in 0..n {
        let la = a.get(i).copied();
        let lb = b.get(i).copied();
        if la != lb {
            eprintln!("  {stream} differs at line {}:", i + 1);
            eprintln!("    serial: {}", la.unwrap_or("<none>"));
            eprintln!("    M:N:    {}", lb.unwrap_or("<none>"));
            return;
        }
    }
    // Streams differ only in a trailing newline (no differing line) — report at the boundary.
    eprintln!("  {stream} differs (trailing content):");
    eprintln!("    serial: {serial:?}");
    eprintln!("    M:N:    {mn:?}");
}

/// Resolve what to run for a bare `chezzi run` (no file argument): find the project root by walking
/// up from the cwd for `chezzi.toml`, parse the manifest, require a `[project] entrypoint`, and map
/// it to a file root-relatively. The entrypoint is a dotted module path optionally suffixed with
/// `:function` (e.g. `"src.main:main"`) — when a function is named, the VM calls it after the
/// module's top-level runs. Returns `(resolved .chz path, Some(function), project_root)`, or a
/// ready-to-print error message. A bare `"src.main"` (no `:function`) yields `None` → run the module
/// top-level only. The `project_root` (the dir holding the manifest, found by walking up from the
/// cwd) is returned so the caller can pin it as the ONE module-graph root for the whole run — the
/// entry file was located relative to it, so every `import` must resolve relative to it too.
fn resolve_entrypoint() -> Result<(String, Option<String>, std::path::PathBuf), String> {
    let cwd = std::env::current_dir()
        .map_err(|e| format!("chezzi run: cannot read current directory: {e}"))?;
    let root = resolver::find_root_from_dir(&cwd).ok_or_else(|| {
        "chezzi run: no file given and no chezzi.toml found (run from inside a project, or pass a file: chezzi run <file.chz>)".to_string()
    })?;
    let manifest_path = root.join("chezzi.toml");
    let src = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("chezzi run: cannot read {}: {e}", manifest_path.display()))?;
    let manifest = manifest::parse(&src)
        .map_err(|e| format!("chezzi run: invalid {}: {e}", manifest_path.display()))?;
    let entrypoint = manifest.entrypoint.ok_or_else(|| {
        format!(
            "chezzi run: {} has no [project] entrypoint; add entrypoint = \"src.main:main\" or pass a file",
            manifest_path.display()
        )
    })?;
    let (module_path, entry_fn) = split_entrypoint(&entrypoint)
        .map_err(|e| format!("chezzi run: {} {e}", manifest_path.display()))?;
    let file = entrypoint_file(module_path, &root)
        .map_err(|e| format!("chezzi run: {} {e}", manifest_path.display()))?;
    Ok((
        file.to_string_lossy().into_owned(),
        entry_fn.map(str::to_string),
        root,
    ))
}

/// Split a manifest `entrypoint` value into its dotted module path and an optional `:function`
/// suffix. Splits on the FIRST `:` so the function name is taken verbatim; `"src.main"` →
/// `("src.main", None)`, `"src.main:main"` → `("src.main", Some("main"))`. A `:` with no function
/// after it (`"src.main:"`) is rejected — otherwise it reaches the VM as an empty name and produces a
/// baffling "function `` not found" error. Pure (no I/O) so it is unit-testable.
fn split_entrypoint(entrypoint: &str) -> Result<(&str, Option<&str>), String> {
    match entrypoint.split_once(':') {
        Some((_, "")) => Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; the ':' must be followed by a function name like \"src.main:main\""
        )),
        Some((module, func)) => Ok((module, Some(func))),
        None => Ok((entrypoint, None)),
    }
}

/// Map a manifest `[project] entrypoint` (a dotted module path) to its `.chz` file, root-relatively.
/// Validates the path FIRST: an empty / whitespace / leading- or trailing-dot / doubled-dot value
/// would otherwise feed empty path segments to [`resolver::module_file`], whose `push("")` +
/// `set_extension` rewrites the project-root dir's own extension and escapes the root (e.g.
/// `<root>.chz`), producing a baffling "cannot read" error. Pure (no cwd/env) so it is unit-testable.
fn entrypoint_file(entrypoint: &str, root: &std::path::Path) -> Result<std::path::PathBuf, String> {
    // Trim surrounding whitespace on EACH segment before building the path, so a padded value like
    // `" app "` resolves to `app.chz` rather than the baffling `<root>/ app .chz` ("cannot read").
    // An embedded path separator would resolve by accident via `PathBuf::push` instead of the
    // documented dotted form, so reject it up front.
    if entrypoint.contains('/') || entrypoint.contains('\\') {
        return Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; the module path must use '.' separators, not '/'"
        ));
    }
    let segs: Vec<String> = entrypoint
        .split('.')
        .map(|s| s.trim().to_string())
        .collect();
    if segs.iter().any(String::is_empty) {
        return Err(format!(
            "has an invalid [project] entrypoint {entrypoint:?}; expected a dotted module path like \"src.main\""
        ));
    }
    Ok(resolver::module_file(&segs, root, &resolver::std_root()))
}

/// `chezzi test [path]` — discover + run every `test fn` in `*_test.chz` files under `path` (default
/// cwd; a single `*_test.chz` file runs that file; a directory is walked recursively). Reports
/// `PASS/FAIL name (file:line) msg` per test, a summary, and a non-zero exit if anything failed.
fn cmd_test(args: &[String]) -> ExitCode {
    let mut path: Option<String> = None;
    let mut saw_serial = false;
    let mut saw_parallel = false;
    let mut max_heap: usize = 0; // 0 = cap OFF (the default)
    for arg in args {
        match arg.as_str() {
            "--serial" => saw_serial = true,
            "--parallel" => saw_parallel = true, // no-op alias: M:N is already the default
            other if other.starts_with("--max-heap=") => {
                let raw = &other["--max-heap=".len()..];
                match raw.parse::<usize>() {
                    Ok(n) => max_heap = n,
                    Err(_) => {
                        eprintln!(
                            "chezzi test: --max-heap expects a byte count (a non-negative integer), got '{raw}'"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("chezzi test: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
            other if path.is_none() => path = Some(other.to_string()),
            _ => {
                eprintln!("chezzi test: unexpected extra argument");
                return ExitCode::FAILURE;
            }
        }
    }
    if saw_serial && saw_parallel {
        eprintln!("chezzi test: --serial and --parallel are mutually exclusive");
        return ExitCode::FAILURE;
    }
    // `--max-heap` is the M:N engine ONLY. The cooperative `--serial` engine shares ONE heap across
    // the parent + every `spawn`/`parallel:` fiber, so a concurrent test's per-heap trip point differs
    // from M:N's isolated-per-worker heaps — rather than ship that serial≠M:N divergence, restrict the
    // flag to the default engine. (`--serial` is the parity oracle, slated for post-freeze removal.)
    if max_heap != 0 && saw_serial {
        eprintln!(
            "chezzi test: --max-heap requires the M:N engine and cannot be combined with --serial"
        );
        return ExitCode::FAILURE;
    }
    let root = path.unwrap_or_else(|| ".".to_string());
    // Default engine is the M:N OS-thread VM — matching `chezzi run`, and forward-compatible with the
    // post-JIT-freeze removal of the cooperative serial engine. `--serial` opts into it while it lives;
    // the dual-engine serial==M:N check itself lives in the `cargo test` gate.
    let report = test_runner::run_tests_capped(std::path::Path::new(&root), !saw_serial, max_heap);
    print!("{}", report.text);
    if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `chezzi init [dir]` — scaffold a new Chezzi project. `dir` defaults to the current directory;
/// it (and any parents) are created if missing. Refuses to clobber an existing `chezzi.toml`.
fn cmd_init(args: &[String]) -> ExitCode {
    let mut dir: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            other if other.starts_with("--") => {
                eprintln!("chezzi init: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
            other if dir.is_none() => dir = Some(other.to_string()),
            _ => {
                eprintln!("chezzi init: unexpected extra argument");
                return ExitCode::FAILURE;
            }
        }
    }
    let dir = dir.unwrap_or_else(|| ".".to_string());
    let path = std::path::Path::new(&dir);
    match scaffold_project(path) {
        Ok(()) => {
            println!("chezzi: scaffolded a new project in {}", path.display());
            println!(
                "  chezzi.toml          project manifest (entrypoint = \"src.main\" — drives bare `chezzi run`)"
            );
            println!(
                "  src/main.chz         entry script  — run with: chezzi run (from {dir}) or chezzi run {dir}/src/main.chz",
            );
            println!(
                "  src/main_test.chz    example test   — run with: chezzi test {}",
                dir
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("chezzi init: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Write the scaffold for a new project into `dir`: a `chezzi.toml` manifest, `src/main.chz`, and an
/// example `src/main_test.chz`. Creates `dir` (and parents) if needed; refuses to overwrite an
/// existing `chezzi.toml` (so it never clobbers a real project). Pure filesystem work — this is the
/// unit-testable core behind `cmd_init`. The manifest's `[project] entrypoint` is written ACTIVE
/// (`"src.main:main"` — module path + `:function`), so a freshly-init'd project runs with a bare
/// `chezzi run`, which calls `main` directly (no trailing call in the source needed; see
/// `manifest::parse` + `resolve_entrypoint`).
fn scaffold_project(dir: &std::path::Path) -> Result<(), String> {
    use std::fs;

    fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create directory '{}': {e}", dir.display()))?;

    let manifest = dir.join("chezzi.toml");
    if manifest.exists() {
        return Err(format!(
            "chezzi.toml already exists in {}; not overwriting (refusing to clobber an existing project)",
            dir.display()
        ));
    }

    // Project name: the target dir's basename (canonicalized, so "." resolves to the current
    // directory's own name). Falls back to "app" only when the path can't be canonicalized or has
    // no basename (e.g. the filesystem root). Control chars are stripped and TOML metacharacters
    // (`\` and `"`) escaped so an odd-but-legal dir name can never produce a malformed manifest.
    let raw = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "app".to_string());
    let escaped: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let name = if escaped.is_empty() {
        "app".to_string()
    } else {
        escaped
    };

    let manifest_body = format!(
        "# chezzi.toml — project manifest.\n\
         #\n\
         # This file is BOTH a project ROOT MARKER (module resolution walks up for it; see\n\
         # docs/spec.md \"Imports & module resolution\") AND a parsed manifest. The toolchain reads\n\
         # `[project]` keys: `entrypoint` is what a bare `chezzi run` executes — a dotted module\n\
         # path, optionally suffixed with `:function` to call that function directly after the\n\
         # module loads (swap which function runs by editing the part after the colon). Without a\n\
         # `:function` suffix the module's top-level runs instead. `name`/`version` are project\n\
         # metadata. Unknown keys/sections are ignored.\n\
         \n\
         [project]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         entrypoint = \"src.main:main\"   # → src/main.chz, calls `main`; run with a bare `chezzi run`\n",
    );
    fs::write(&manifest, manifest_body)
        .map_err(|e| format!("cannot write {}: {e}", manifest.display()))?;

    let src = dir.join("src");
    fs::create_dir_all(&src).map_err(|e| format!("cannot create {}: {e}", src.display()))?;

    let main_chz = "# src/main.chz — program entry.\n\
        #\n\
        # chezzi.toml's `entrypoint = \"src.main:main\"` calls `main` for you after this module\n\
        # loads, so no trailing `main()` call is needed (see docs/syntax.md \"9b. Program entry\").\n\
        # Swap the function a bare `chezzi run` invokes by editing the part after the `:`. Run a\n\
        # *file* directly (`chezzi run src/main.chz`) and it is scripting-style: top-level only.\n\
        \n\
        fn main():\n\
        \x20   print(\"Hello from Chezzi!\")\n";
    let main_path = src.join("main.chz");
    fs::write(&main_path, main_chz)
        .map_err(|e| format!("cannot write {}: {e}", main_path.display()))?;

    let test_chz = "# src/main_test.chz — example test.\n\
        #\n\
        # `chezzi test` discovers every `test fn` in *_test.chz files (see docs/syntax.md \"9c\").\n\
        # Run it with:  chezzi test .\n\
        \n\
        test fn arithmetic():\n\
        \x20   assert 1 + 1 == 2\n\
        \n\
        test fn strings():\n\
        \x20   assert \"a\" + \"b\" == \"ab\", \"string concat\"\n";
    let test_path = src.join("main_test.chz");
    fs::write(&test_path, test_chz)
        .map_err(|e| format!("cannot write {}: {e}", test_path.display()))?;

    Ok(())
}

/// Embedded language documentation, keyed by topic. `include_str!` paths are relative to this file
/// (`src/main.rs`), so the docs live at `../docs/...`. Bundling them into the binary means
/// `chezzi docs` works anywhere (no repo checkout), e.g. piping the full reference to an LLM.
const DOC_TOPICS: &[(&str, &str)] = &[
    ("spec", include_str!("../docs/spec.md")),
    ("syntax", include_str!("../docs/syntax.md")),
    ("stdlib", include_str!("../docs/stdlib.md")),
];

/// The topics, in order, that make up the `llms` bundle (`chezzi docs` with no topic): the essentials
/// an LLM needs to write correct Chezzi — language overview, full syntax, and the library surface.
const LLMS_BUNDLE: &[&str] = &["spec", "syntax", "stdlib"];

fn doc_topic(name: &str) -> Option<&'static str> {
    DOC_TOPICS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| *body)
}

/// Build the text `chezzi docs <topic>` prints, or a ready-to-print error message. Pure (no I/O) so
/// it is unit-testable. Topics:
/// - `llms` / `all` → the full reference bundle (spec + syntax + stdlib + grammar).
/// - `topics` / `list` → the available topic names.
/// - `<topic>` → that one embedded document. Unknown topic / any flag → `Err`.
fn render_docs(topic: &str) -> Result<String, String> {
    match topic {
        "llms" | "all" => {
            let mut out = format!(
                "# Chezzi language reference (generated by `chezzi docs`)\n\
                 # The complete language + library reference, concatenated for LLM consumption.\n\
                 # Sections: {}.\n\n",
                LLMS_BUNDLE.join(", ")
            );
            for name in LLMS_BUNDLE {
                let body = doc_topic(name).expect("bundle topic must be embedded");
                out.push_str(&format!("# ===== Chezzi reference: {name} =====\n\n"));
                out.push_str(body);
                out.push('\n');
            }
            Ok(out)
        }
        "topics" | "list" => {
            let mut out = String::from("available docs topics:\n");
            for (name, _) in DOC_TOPICS {
                out.push_str(&format!("    {name}\n"));
            }
            out.push_str(&format!(
                "    llms     full reference bundle ({})\n",
                LLMS_BUNDLE.join(" + ")
            ));
            Ok(out)
        }
        other if other.starts_with("--") => Err(format!("chezzi docs: unknown flag '{other}'")),
        other => doc_topic(other).map(str::to_string).ok_or_else(|| {
            let names: Vec<&str> = DOC_TOPICS.iter().map(|(n, _)| *n).collect();
            format!(
                "chezzi docs: unknown topic '{other}'\nvalid topics: {}, llms, topics",
                names.join(", ")
            )
        }),
    }
}

/// `chezzi docs [topic]` — print embedded language documentation (see [`render_docs`]). With no
/// topic, prints the full LLM reference bundle to stdout (pipe it: `chezzi docs > chezzi-llms.txt`).
fn cmd_docs(args: &[String]) -> ExitCode {
    let topic = args.first().map(String::as_str).unwrap_or("llms");
    match render_docs(topic) {
        Ok(text) => match write_stdout(&text) {
            Ok(()) => ExitCode::SUCCESS,
            // A reader that closed the pipe early (`chezzi docs | head`, an LLM tool reading a
            // prefix) is the expected case for a bulk dump, not an error — exit clean, don't panic.
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("chezzi docs: cannot write output: {e}");
                ExitCode::FAILURE
            }
        },
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

/// Write `s` to stdout, surfacing the I/O error (notably `BrokenPipe`) instead of panicking like
/// `print!`. Used by `chezzi docs`, whose bundle output is designed to be piped to a reader that may
/// close early.
fn write_stdout(s: &str) -> std::io::Result<()> {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(s.as_bytes())?;
    lock.flush()
}

enum CheckOutcome {
    Ok,
    Errors(Vec<checker::CheckError>),
    /// A resolve, lex, or parse error (the program never reaches the checker). `text` is the full
    /// rendered message (Display, with the `resolve error (...)` prefix) used for plain-text output;
    /// `message` is the clean message body (no Display prefix, with any `in module 'X':` attribution)
    /// used for `--errors=json` so its shape matches type-error JSON. `line`/`col` are carried so
    /// `--errors=json` still emits structured output.
    Fatal {
        text: String,
        message: String,
        line: usize,
        col: usize,
    },
}

/// Resolve the module graph, then type-check it, normalizing each failure mode. A resolve, lex, or
/// parse failure (in the entry or any imported module) is `Fatal`; type errors are `Errors`.
///
/// `root` pins the module-graph root (the "one root per run" invariant): the bare-`chezzi run`
/// manifest path passes `Some(root)` so the checker resolves imports against the SAME root the VM
/// will run against; `None` (explicit `chezzi run FILE`) derives it by walking up from the file.
fn type_check(path: &str, root: Option<&std::path::Path>) -> CheckOutcome {
    // Resolve + desugar + type-check on the dedicated front-end stack: the recursive AST walkers can
    // overflow the caller's (main-thread) stack on a deep-but-valid AST — see `chezzi::on_frontend_stack`.
    let path = path.to_string();
    let root = root.map(|r| r.to_path_buf());
    chezzi::on_frontend_stack(move || type_check_inner(&path, root.as_deref()))
}

fn type_check_inner(path: &str, root: Option<&std::path::Path>) -> CheckOutcome {
    let entry = std::path::Path::new(path);
    let build = match root {
        Some(r) => resolver::build_graph_with_root(entry, r.to_path_buf()),
        None => resolver::build_graph(entry),
    };
    let graph = match build {
        Ok(g) => g,
        Err(e) => {
            return CheckOutcome::Fatal {
                text: e.to_string(),
                message: e.message.clone(),
                line: e.span.line,
                col: e.span.col,
            };
        }
    };
    match checker::check_graph(&graph) {
        Ok(()) => CheckOutcome::Ok,
        Err(errs) => CheckOutcome::Errors(errs),
    }
}

/// Pull the file path (first non-flag arg) and `--errors=json` out of a command's args. Returns
/// `None` (after printing a diagnostic) on a missing file or an unknown flag — the caller fails.
fn parse_file_and_flags(cmd: &str, args: &[String]) -> Option<(String, bool)> {
    let mut path = None;
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--errors=json" => json = true,
            other if other.starts_with("--") => {
                eprintln!("chezzi {cmd}: unknown flag '{other}'");
                return None;
            }
            other => {
                if path.is_none() {
                    path = Some(other.to_string());
                }
            }
        }
    }
    match path {
        Some(p) => Some((p, json)),
        None => {
            eprintln!(
                "chezzi {cmd}: missing file argument\nusage: chezzi {cmd} <file.chz> [--errors=json]"
            );
            None
        }
    }
}

fn read_source(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("chezzi: cannot read '{path}': {e}");
            None
        }
    }
}

/// Print type errors as plain text (default) or a JSON array (`--errors=json`).
fn report_check_errors(errs: &[checker::CheckError], json: bool) {
    if json {
        let items: Vec<String> = errs
            .iter()
            .map(|e| {
                format!(
                    "{{\"line\":{},\"col\":{},\"message\":{}}}",
                    e.span.line,
                    e.span.col,
                    json_string(&e.message)
                )
            })
            .collect();
        println!("[{}]", items.join(","));
    } else {
        for e in errs {
            eprintln!("{e}");
        }
        eprintln!(
            "chezzi: {} type error{}",
            errs.len(),
            if errs.len() == 1 { "" } else { "s" }
        );
    }
}

/// Report a fatal resolve/lex/parse error, preserving the `--errors=json` contract (valid JSON on
/// stdout). Plain text uses `text` (the full Display rendering, with the `resolve error (...)`
/// prefix); JSON uses the clean `message` (no embedded Display prefix) so its shape matches
/// type-error JSON — the `in module 'X':` attribution rides along inside `message`.
fn report_fatal(text: &str, message: &str, line: usize, col: usize, json: bool) {
    if json {
        println!(
            "[{{\"line\":{line},\"col\":{col},\"message\":{}}}]",
            json_string(message)
        );
    } else {
        eprintln!("{text}");
    }
}

/// Encode a string as a JSON string literal (minimal, zero-dep — escapes what JSON requires).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod init_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("chezzi_init_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn init_scaffolds_expected_files() {
        let d = TmpDir::new();
        let dir = d.0.join("proj");
        scaffold_project(&dir).expect("scaffold should succeed");
        assert!(dir.join("chezzi.toml").is_file(), "chezzi.toml missing");
        assert!(dir.join("src/main.chz").is_file(), "src/main.chz missing");
        assert!(
            dir.join("src/main_test.chz").is_file(),
            "src/main_test.chz missing"
        );
    }

    #[test]
    fn init_refuses_when_manifest_exists() {
        let d = TmpDir::new();
        std::fs::write(d.0.join("chezzi.toml"), "# pre-existing\n").unwrap();
        let err = scaffold_project(&d.0).expect_err("should refuse to clobber");
        assert!(
            err.contains("already exists"),
            "error should mention already exists, got: {err}"
        );
        assert!(
            !d.0.join("src/main.chz").exists(),
            "must not write project files when refusing"
        );
    }

    #[test]
    fn scaffolded_main_runs() {
        let d = TmpDir::new();
        scaffold_project(&d.0).expect("scaffold should succeed");
        let main = d.0.join("src/main.chz");

        // The scaffolded main.chz no longer self-calls `main` — the manifest's `:main` entrypoint
        // does. Running the FILE directly is scripting-mode (top-level only): it defines `main` but
        // prints nothing.
        let (stdout, stderr, result, _exit) = vm::run_file_with_entry(
            &main,
            native::HostConfig::from_process(vec![]),
            false,
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "scaffolded main.chz must load; stderr:\n{stderr}"
        );
        assert!(
            !stdout.contains("Hello from Chezzi!"),
            "running the file directly must NOT call main; stdout:\n{stdout}"
        );

        // Invoking the entry function (what a bare `chezzi run` does) prints the greeting.
        let (stdout, stderr, result, _exit) = vm::run_file_with_entry(
            &main,
            native::HostConfig::from_process(vec![]),
            false,
            Some("main"),
            None,
        );
        assert!(
            result.is_ok(),
            "scaffolded entry `main` must run; stderr:\n{stderr}"
        );
        assert!(
            stdout.contains("Hello from Chezzi!"),
            "scaffolded entry `main` should print the greeting; stdout:\n{stdout}"
        );

        // And run the scaffolded test file through the real `chezzi test` runner: both pass.
        let report = test_runner::run_tests(&d.0, true);
        assert!(
            report.passed,
            "scaffolded tests must pass; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("2 test(s): 2 passed, 0 failed"),
            "expected 2 passing tests; report:\n{}",
            report.text
        );
    }

    #[test]
    fn scaffolded_manifest_entrypoint_resolves_and_runs() {
        // A freshly-init'd project must run with a bare `chezzi run`: the manifest's active
        // `entrypoint = "src.main:main"` parses, resolves the module root-relatively to src/main.chz,
        // and the `:main` suffix calls `main` directly.
        let d = TmpDir::new();
        scaffold_project(&d.0).expect("scaffold should succeed");

        // The scaffolded manifest parses to an active `module:function` entrypoint.
        let toml = std::fs::read_to_string(d.0.join("chezzi.toml")).unwrap();
        let m = manifest::parse(&toml).expect("scaffolded manifest must parse");
        assert_eq!(
            m.entrypoint.as_deref(),
            Some("src.main:main"),
            "scaffolded manifest must write an active module:function entrypoint"
        );

        // Splits into the module path and the function name.
        let (module_path, entry_fn) = split_entrypoint(m.entrypoint.as_deref().unwrap()).unwrap();
        assert_eq!(module_path, "src.main");
        assert_eq!(entry_fn, Some("main"));

        // find_root_from_dir(root) → the project root (the cwd case for bare `chezzi run`).
        let root = resolver::find_root_from_dir(&d.0).expect("chezzi.toml marks the root");

        // The module path resolves root-relatively to src/main.chz.
        let entry = entrypoint_file(module_path, &root).expect("module path resolves");
        assert!(
            entry.ends_with("src/main.chz"),
            "entry: {}",
            entry.display()
        );

        // Calling the named entry function prints the greeting.
        let (stdout, stderr, result, _exit) = vm::run_file_with_entry(
            &entry,
            native::HostConfig::from_process(vec![]),
            false,
            Some(entry_fn.unwrap()),
            None,
        );
        assert!(result.is_ok(), "entrypoint must run; stderr:\n{stderr}");
        assert!(
            stdout.contains("Hello from Chezzi!"),
            "entrypoint should print the greeting; stdout:\n{stdout}"
        );

        // A missing entry function is a clear error, not a silent no-op.
        let (_o, _e, result, _exit) = vm::run_file_with_entry(
            &entry,
            native::HostConfig::from_process(vec![]),
            false,
            Some("nope"),
            None,
        );
        let err = result.expect_err("missing entry function must error");
        let msg = vm::format_trace(&err.message, err.span, &err.trace);
        assert!(
            msg.contains("entrypoint function `nope` not found"),
            "expected a clear not-found error; got:\n{msg}"
        );
    }

    #[test]
    fn doc_topics_are_embedded_and_nonempty() {
        // Every advertised topic resolves to non-empty embedded content.
        for (name, _) in DOC_TOPICS {
            let body = doc_topic(name).unwrap_or_else(|| panic!("topic {name} missing"));
            assert!(!body.trim().is_empty(), "topic {name} is empty");
        }
        // An unknown topic does not resolve.
        assert!(doc_topic("nope").is_none());
    }

    #[test]
    fn llms_bundle_topics_all_resolve() {
        // Every name in the bundle must be a real embedded topic (the bundle uses expect()).
        for name in LLMS_BUNDLE {
            assert!(
                doc_topic(name).is_some(),
                "bundle topic {name} not embedded"
            );
        }
        // The bundle pulls together the language + library reference.
        for name in ["spec", "syntax", "stdlib"] {
            assert!(LLMS_BUNDLE.contains(&name), "bundle should include {name}");
        }
    }

    #[test]
    fn render_docs_topics_and_bundle() {
        // A known topic returns its body; an unknown topic errors with guidance.
        assert!(render_docs("stdlib").unwrap().contains("Standard library"));
        let err = render_docs("definitely-not-a-topic").unwrap_err();
        assert!(err.contains("unknown topic"), "got: {err}");
        // A flag is rejected.
        assert!(render_docs("--full").is_err());
        // The listing names the topics.
        let list = render_docs("topics").unwrap();
        assert!(list.contains("stdlib") && list.contains("syntax"));
        // The default bundle stitches spec + syntax + stdlib together with banners.
        let bundle = render_docs("llms").unwrap();
        assert!(bundle.contains("# ===== Chezzi reference: spec ====="));
        assert!(bundle.contains("# ===== Chezzi reference: syntax ====="));
        assert!(bundle.contains("# ===== Chezzi reference: stdlib ====="));
    }

    #[test]
    fn split_entrypoint_forms() {
        assert_eq!(split_entrypoint("src.main").unwrap(), ("src.main", None));
        assert_eq!(
            split_entrypoint("src.main:main").unwrap(),
            ("src.main", Some("main"))
        );
        // Split on the FIRST colon (module path can't contain one; the rest is the fn name verbatim).
        assert_eq!(split_entrypoint("a.b:c:d").unwrap(), ("a.b", Some("c:d")));
        // A trailing `:` with no function name is rejected (would otherwise be an empty fn name).
        let err = split_entrypoint("src.main:").unwrap_err();
        assert!(
            err.contains("must be followed by a function name"),
            "got: {err}"
        );
    }

    #[test]
    fn init_creates_missing_dir() {
        let d = TmpDir::new();
        let nested = d.0.join("a/b/c");
        scaffold_project(&nested).expect("should create nested dir");
        assert!(nested.join("chezzi.toml").is_file());
        assert!(nested.join("src/main.chz").is_file());
    }

    #[test]
    fn entrypoint_file_validates_dotted_path() {
        let root = std::path::Path::new("/proj");
        // Valid dotted path → root-relative .chz under the project root.
        assert_eq!(
            entrypoint_file("src.main", root).unwrap(),
            root.join("src/main.chz")
        );
        // Surrounding whitespace on segments is trimmed before the path is built, so a padded
        // entrypoint resolves to the same file as its trimmed form (was: `<root>/ app .chz`).
        assert_eq!(
            entrypoint_file(" app ", root).unwrap(),
            root.join("app.chz")
        );
        assert_eq!(
            entrypoint_file(" src . main ", root).unwrap(),
            root.join("src/main.chz")
        );
        // Bad forms must be REJECTED, not mangle the root path (push("") + set_extension footgun).
        // `"a. .b"` has a whitespace-only middle segment that trims to empty → still rejected.
        for bad in ["", "   ", ".", ".main", "src.main.", "src..main", "a. .b"] {
            assert!(
                entrypoint_file(bad, root).is_err(),
                "entrypoint {bad:?} should be rejected"
            );
        }
        // An embedded path separator ('/' or '\\') would resolve by accident via PathBuf::push
        // instead of the documented dotted form — reject it with a clear message.
        for bad in ["src/main", "src\\main", "a/b.c"] {
            let e = entrypoint_file(bad, root).unwrap_err();
            assert!(
                e.contains("'.' separators"),
                "entrypoint {bad:?} err should mention '.' separators, got: {e}"
            );
        }
    }
}
