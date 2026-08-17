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
// it just imports the pieces the CLI body drives. (The grammar `conformance` suite and the VM's own
// golden tests — `src/vm/golden_tests.rs`, the single-engine replacement for the deleted
// serial-vs-M:N parity tests — now live + run once in the lib's test target, not in this bin.)
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
    --parallel       Accepted no-op alias (`run` only) — the engine is the default already
    --threads=N      Worker threads for the engine (0 = all cores; env: CHEZZI_THREADS)
    --max-heap=N     (`test`) Hard-abort any test whose live heap exceeds N bytes — a runaway-alloc
                     guard, bucketed OVER-MEMORY (0/omitted = off)
    --timeout=N      (`test`) Hard-abort any test running longer than N ms — a wall-clock guard,
                     bucketed TIMED-OUT (0/omitted = off)

`chezzi test` SELECTION + OUTPUT (all opt-in; default output is unchanged):
    -k, --filter S   Run only tests whose name (`fn` or `Suite::method`) contains substring S; the
                     summary notes how many were filtered out. Zero matches = clear failure.
    --fail-fast      Stop at the first non-pass verdict (deterministic order: sorted files, then each
                     file's free tests, then each suite's methods, all in declaration order).
    --show-output    Surface a FAILING test's captured stdout byte-exactly, indented under its line
                     (default: discard).
    --errors=json    Emit ONLY a JSON document ({tests:[{name,file,line?,status,duration_ms}],totals})
                     for CI/editors — suppresses the human PASS/FAIL lines (mirrors `check --errors=json`).
    -q, --quiet      Dots (`.`/`F`/`E`/`M`/`T`) per test + the summary only (no per-test lines).
    -v, --verbose    Per-test lines + per-test timing (`(Nms)`) + a total (`-q`/`-v` are exclusive).
    --color=MODE     auto (default; on iff stdout is a tty) | always | never — colors the verdict tag.

NOTE: flags must come BEFORE the file path. Anything after the file is passed
      to the program as an argument, so `chezzi run prog.chz --threads=4` hands
      `--threads=4` to the program instead of sizing the engine. Use `chezzi run --threads=4 prog.chz`.
";

fn main() -> ExitCode {
    // `args_os`, not `args`: `std::env::args()` PANICS on a non-UTF-8 argument, so hostile bytes in
    // argv (or in the script path) aborted the CLI with rc=101 before the program ever started.
    // Decoding is LOSSY (invalid bytes → U+FFFD) — argv reaches Chezzi as `str`; documented in
    // docs/stdlib.md under std.os.
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
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

    // Lex + parse + the `{:#?}` Debug walk of the whole AST on the dedicated front-end stack: the
    // Debug walk recurses once per AST node, and a deep-but-valid buffer (left-leaning chains the
    // parser's recursive MAX_DEPTH guard never sees) can overflow a caller stack — see
    // `chezzi::on_frontend_stack`. `println!` runs INSIDE the closure so it keeps streaming through
    // stdout's `LineWriter` as the Debug walk produces it; building a `String` first and printing it
    // outside would materialise the whole render up front (`{:#?}` indentation is quadratic in depth)
    // before the first byte reaches stdout. Only the exit-code mapping stays on the main thread. A
    // broken-pipe panic from `println!` still unwinds through `on_frontend_stack`'s `resume_unwind`
    // onto this thread with the same exit code as before (101). The panic line now names
    // `<unnamed>` rather than `main`, since the write happens on the spawned thread and
    // `resume_unwind` does not re-run the panic hook — measured, and the only observable delta.
    let result: Result<(), String> = chezzi::on_frontend_stack(move || {
        let tokens = lexer::tokenize(&source).map_err(|e| e.to_string())?;
        let module = parser::parse(tokens).map_err(|e| e.to_string())?;
        println!("{module:#?}");
        Ok(())
    });

    match result {
        Ok(()) => ExitCode::SUCCESS,
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
    // up from the file — pass `None`. The entry FUNCTION is derived from the project manifest when
    // this file IS the declared entry module (`manifest::entry_fn_for`), so a static check of an
    // entry module a project cannot start reports it.
    let (outcome, warns) = type_check(&path, None, EntryGate::FromManifest);
    // `check`'s stdout IS the diagnostic document, so in machine mode the warnings ride the single
    // array the arms below print; in plain text they precede the verdict on stderr. Exactly one of
    // the two — the `!json` guard is what keeps a warning from being printed on both streams.
    if !json {
        report_check_warnings(&warns);
    }
    match outcome {
        CheckOutcome::Ok => {
            // Warnings are not errors: the verdict and the exit code are unchanged. In JSON mode
            // they ARE the array (`[]` when there are none), so a machine consumer sees them.
            if json {
                println!("{}", diags_json(&warns));
            } else {
                println!("ok: no type errors");
            }
            ExitCode::SUCCESS
        }
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, &warns, json);
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

/// `chezzi run [file] [--errors=json] [--parallel] [--threads=N]` — type-check first, then execute on
/// the bytecode VM. With NO file argument, the project's manifest entrypoint is run: the project root
/// is found by walking up from the cwd for `chezzi.toml`, and its `[project] entrypoint` (a dotted
/// module path, e.g. `"src.main"`) is resolved root-relatively and run. The VM runs the real
/// OS-thread (M:N) engine — the sole engine. `--parallel` is kept as an accepted no-op alias.
/// `--threads=N` (or the `CHEZZI_THREADS` env var) sizes the engine's worker pool — `0` (or omitted)
/// = all cores; the flag wins over the env var.
fn cmd_run(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut json = false;
    // `--threads=N` worker count for the engine. `0` = all cores. `None` = unset (fall through to
    // `CHEZZI_THREADS`, then auto). The flag wins over env.
    let mut threads_flag: Option<usize> = None;
    // Positional args after the script path are the program's own args (std.os.args).
    // GOTCHA: this means flags MUST precede the file — `chezzi run prog.chz --threads=4`
    // treats `--threads=4` as a program arg (path is already set), not an engine size.
    // Correct form: `chezzi run --threads=4 prog.chz`.
    let mut prog_args: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            _ if path.is_some() => prog_args.push(arg.clone()),
            "--errors=json" => json = true,
            "--parallel" => {} // accepted no-op alias — the engine is the default already
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

    // Resolve the engine's worker count. An explicit `--threads` wins; otherwise `CHEZZI_THREADS`
    // applies. `0`/unset both mean auto (all cores).
    if let Some(n) = threads_flag {
        vm::set_worker_count(n);
    } else {
        apply_env_worker_count("run");
    }

    if read_source(&path).is_none() {
        return ExitCode::FAILURE;
    }

    // Pre-run type check: type errors block execution (no partial output). Pins the SAME root the VM
    // will run (below), so the checker and the VM never disagree on which same-named module to load.
    // Bare `chezzi run` gates on the entrypoint it resolved; an explicit `chezzi run <file>` is
    // SCRIPT mode — it runs the top level and invokes nothing, so the gate is not its business even
    // when the file happens to be the manifest's entry module (`chezzi check` still reports it).
    let gate = match &entry_fn {
        Some(f) => EntryGate::Named(f),
        None => EntryGate::Script,
    };
    let (outcome, warns) = type_check(&path, root_override.as_deref(), gate);
    // `run`'s stdout belongs to the PROGRAM, so a warning goes to stderr in both modes — unguarded,
    // unlike `check` above. The `&[]` below is the other half of that decision: were the warnings
    // also folded into the stdout array, the same diagnostic would print twice, on two streams.
    report_check_warnings(&warns);
    match outcome {
        CheckOutcome::Ok => {}
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, &[], json);
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

    // The CLI STREAMS: the VM writes each `print` straight to the real stdout as it happens (see
    // `HostConfig::stream`), so a prompt appears before its `read_line`, a long-running program is
    // not silent, and a spawned task's log is visible before its nursery joins. The lib helpers keep
    // the buffered sink instead, so `out`/`err` come back empty here. The native std modules read
    // args/env/stdin from a process-backed config.
    let p = std::path::Path::new(&path);
    let mut cfg = native::HostConfig::from_process(prog_args);
    cfg.stream = true;
    let (errored, exit_code) = {
        let (_out, _err, result, code) =
            vm::run_file_with_entry(p, cfg, entry_fn.as_deref(), root_override.clone());
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

/// Parse a raw `CHEZZI_THREADS` value (as read from the env, or `None` if unset) into a worker
/// count. `None`/empty/whitespace-only means "not set" (`Ok(None)`, caller leaves the engine at its
/// existing default). A pure function — no I/O — so parsing is unit-tested directly instead of via
/// env-var mutation, which would race other tests in this binary.
fn resolve_threads_env(raw: Option<&str>) -> Result<Option<usize>, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => s.parse::<usize>().map(Some).map_err(|_| s.to_string()),
    }
}

/// Apply `CHEZZI_THREADS`, if set, to the engine's worker count. Shared by `cmd_run` (which layers an
/// explicit `--threads=N` flag on top — the flag wins, this is only the env fallback) and `cmd_test`
/// (env-only; `test` takes no `--threads` flag). `cmd` names the caller in the warning so an invalid
/// value is traceable to which subcommand read it.
///
/// Before this, `chezzi test` silently ignored `CHEZZI_THREADS` — only `cmd_run` read it
/// (`test_runner.rs`'s `over_memory_trips_on_an_all_native_task_body` test documents exactly this
/// gap: "`CHEZZI_THREADS=1` does NOT reproduce that — the env var is read by `main::cmd_run`, not by
/// `run_tests_capped`"). That made a `CHEZZI_THREADS=2 chezzi test tests/chz` differential a no-op:
/// both runs used the same (auto) worker count. This closes it for `test` too.
fn apply_env_worker_count(cmd: &str) {
    match resolve_threads_env(std::env::var("CHEZZI_THREADS").ok().as_deref()) {
        Ok(Some(n)) => vm::set_worker_count(n),
        Ok(None) => {}
        Err(bad) => eprintln!(
            "chezzi {cmd}: ignoring invalid CHEZZI_THREADS='{bad}' (expected a non-negative integer; 0 = all cores)"
        ),
    }
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
    let (module_path, entry_fn) = manifest::split_entrypoint(&entrypoint)
        .map_err(|e| format!("chezzi run: {} {e}", manifest_path.display()))?;
    let file = manifest::entrypoint_file(module_path, &root)
        .map_err(|e| format!("chezzi run: {} {e}", manifest_path.display()))?;
    Ok((
        file.to_string_lossy().into_owned(),
        entry_fn.map(str::to_string),
        root,
    ))
}

/// `chezzi test [path]` — discover + run every `test fn` in `*_test.chz` files under `path` (default
/// cwd; a single `*_test.chz` file runs that file; a directory is walked recursively). Reports
/// `PASS/FAIL name (file:line) msg` per test, a summary, and a non-zero exit if anything failed.
/// Runs on the M:N engine, sized by `CHEZZI_THREADS` like `chezzi run` (no `--threads` flag here).
fn cmd_test(args: &[String]) -> ExitCode {
    use chezzi::test_runner::{RunOpts, Verbosity};
    let mut path: Option<String> = None;
    let mut opts = RunOpts::default();
    let mut saw_quiet = false;
    let mut saw_verbose = false;
    // `auto` (default) resolves to isatty; `always`/`never` force it. Kept as an enum'd string so the
    // resolution happens once, after the loop, against the real stdout.
    let mut color_mode = "auto";
    // Index-based so `-k`/`--filter` can consume the following argument (two-token form).
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--fail-fast" => opts.fail_fast = true,
            "--show-output" => opts.show_output = true,
            "--errors=json" => opts.json = true,
            "-q" | "--quiet" => saw_quiet = true,
            "-v" | "--verbose" => saw_verbose = true,
            "-k" | "--filter" => {
                let Some(pat) = args.get(i + 1) else {
                    eprintln!("chezzi test: {arg} expects a substring argument");
                    return ExitCode::FAILURE;
                };
                opts.filter = Some(pat.clone());
                i += 1; // consume the pattern
            }
            other if other.starts_with("--color=") => match &other["--color=".len()..] {
                m @ ("auto" | "always" | "never") => color_mode = m,
                bad => {
                    eprintln!("chezzi test: --color expects auto|always|never, got '{bad}'");
                    return ExitCode::FAILURE;
                }
            },
            other if other.starts_with("--max-heap=") => {
                let raw = &other["--max-heap=".len()..];
                match raw.parse::<usize>() {
                    Ok(n) => opts.max_heap = n,
                    Err(_) => {
                        eprintln!(
                            "chezzi test: --max-heap expects a byte count (a non-negative integer), got '{raw}'"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            other if other.starts_with("--timeout=") => {
                let raw = &other["--timeout=".len()..];
                match raw.parse::<u64>() {
                    Ok(n) => opts.timeout_ms = n,
                    Err(_) => {
                        eprintln!(
                            "chezzi test: --timeout expects a millisecond count (a non-negative integer), got '{raw}'"
                        );
                        return ExitCode::FAILURE;
                    }
                }
            }
            other if other.starts_with("--") => {
                eprintln!("chezzi test: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
            other if path.is_none() => {
                if reject_lossy_path(other) {
                    return ExitCode::FAILURE;
                }
                path = Some(other.to_string())
            }
            _ => {
                eprintln!("chezzi test: unexpected extra argument");
                return ExitCode::FAILURE;
            }
        }
        i += 1;
    }
    if saw_quiet && saw_verbose {
        eprintln!("chezzi test: -q and -v are mutually exclusive");
        return ExitCode::FAILURE;
    }
    opts.verbosity = if saw_quiet {
        Verbosity::Quiet
    } else if saw_verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    };
    // Resolve color: `always` forces on, `never` off, `auto` = stdout is a tty. The runner itself never
    // probes the tty (its `report.text` is a pure String the harness string-matches; `report.bytes` is
    // the byte-exact twin actually written below), so this is the ONE seam.
    opts.color = match color_mode {
        "always" => true,
        "never" => false,
        _ => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    // JSON is machine output: never colorize it.
    if opts.json {
        opts.color = false;
    }
    let root = path.unwrap_or_else(|| ".".to_string());
    // The engine is the M:N OS-thread VM, matching `chezzi run` — the sole engine. Same worker-count
    // knob as `run`, minus the `--threads` flag (env only; see `apply_env_worker_count`'s doc for
    // why this wasn't wired before).
    apply_env_worker_count("test");
    let report = test_runner::run_tests_opts(std::path::Path::new(&root), opts);
    // Byte-exact (W6-9r item 4): `report.bytes`, not `report.text`, so a test's `--show-output`
    // capture reaches fd 1 unchanged — matching `chezzi run` (W6-9) and `go test`. No explicit flush:
    // the report always ends in `\n`, so the `LineWriter` flushes, same as `print!` did.
    // ANY write failure — including a closed reader (`| head -1`) — truncates the report, so the run
    // must not report success. Measured against the reference runners with a PASSING run piped into
    // `head -1`: `go test -v` exits 141 (SIGPIPE), `pytest -s` exits 1 — neither treats a closed
    // reader as a clean pass. Matches `chezzi run`'s own broken-pipe handling (see `out_dead_reason`
    // above): a truncated report is a failure, full stop.
    if let Err(e) = std::io::Write::write_all(&mut std::io::stdout(), &report.bytes) {
        let why = if e.kind() == std::io::ErrorKind::BrokenPipe {
            "stdout closed (broken pipe)".to_string()
        } else {
            format!("cannot write stdout: {e}")
        };
        eprintln!("chezzi test: {why}");
        return ExitCode::FAILURE;
    }
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
                "  chezzi.toml          project manifest (entrypoint = \"src.main:main\" — drives bare `chezzi run`)"
            );
            println!(
                "  src/main.chz         entry script  — run with: chezzi run (from {dir}). NOTE `chezzi run {dir}/src/main.chz` runs the file's TOP LEVEL only, so it will NOT call main()",
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
/// Returns the outcome plus the pass's non-fatal warnings (empty on a `Fatal`, which never reaches
/// the checker).
fn type_check(
    path: &str,
    root: Option<&std::path::Path>,
    entry_fn: EntryGate<'_>,
) -> (CheckOutcome, Vec<checker::CheckError>) {
    // Resolve + desugar + type-check on the dedicated front-end stack: the recursive AST walkers can
    // overflow the caller's (main-thread) stack on a deep-but-valid AST — see `chezzi::on_frontend_stack`.
    let path = path.to_string();
    let root = root.map(|r| r.to_path_buf());
    let owned = match entry_fn {
        EntryGate::Named(f) => Some(f.to_string()),
        _ => None,
    };
    let script = matches!(entry_fn, EntryGate::Script);
    chezzi::on_frontend_stack(move || {
        let gate = match (&owned, script) {
            (Some(f), _) => EntryGate::Named(f),
            (None, true) => EntryGate::Script,
            (None, false) => EntryGate::FromManifest,
        };
        type_check_inner(&path, root.as_deref(), gate)
    })
}

/// Which entry FUNCTION a static check must hold this file's declarations to — the three CLI shapes,
/// spelled out so a caller cannot land in the wrong one by passing `None` and meaning two things.
///
/// The gate exists because the manifest's `entrypoint = "mod:fn"` declares a call the runtime makes
/// BY NAME at arity zero. It is a property of the PROJECT (Go rejects a `func main` with parameters
/// at build time), so a static check of the entry module reports it — but only where that call
/// actually happens.
#[derive(Clone, Copy)]
enum EntryGate<'a> {
    /// Bare `chezzi run` — the manifest entrypoint function it already resolved.
    Named(&'a str),
    /// `chezzi check <file>` and the editor/LSP: derive it from the project manifest, so a static
    /// check of the declared entry module reports a `main` the project cannot start.
    FromManifest,
    /// `chezzi run <file>` — SCRIPT mode: the file's top level runs and no function is invoked, so
    /// the gate must not fire. It fired here for one commit, and a project whose `main` took a
    /// witness could no longer run its own entry file as a script at all.
    Script,
}

fn type_check_inner(
    path: &str,
    root: Option<&std::path::Path>,
    entry_fn: EntryGate<'_>,
) -> (CheckOutcome, Vec<checker::CheckError>) {
    let entry = std::path::Path::new(path);
    // M24 — the manifest-entrypoint gate reaches EVERY consumer that checks this file, not just bare
    // `chezzi run` (which passes the name it already resolved). `chezzi check src/main.chz` used to
    // say "ok: no type errors" about a project that cannot start; the editor, which calls
    // `check_graph` through the same one derivation, showed nothing. What the caller must NOT be
    // able to do is land in that arm by accident — see [`EntryGate::Script`].
    let derived = match entry_fn {
        EntryGate::FromManifest => manifest::entry_fn_for(entry),
        _ => None,
    };
    let entry_fn = match entry_fn {
        EntryGate::Named(f) => Some(f),
        _ => derived.as_deref(),
    };
    let build = match root {
        Some(r) => resolver::build_graph_with_root(entry, r.to_path_buf()),
        None => resolver::build_graph(entry),
    };
    let graph = match build {
        Ok(g) => g,
        Err(e) => {
            return (
                CheckOutcome::Fatal {
                    text: e.to_string(),
                    message: e.message.clone(),
                    // `as usize`: widening a `Span`'s u32 line/col — lossless.
                    line: e.span.line as usize,
                    col: e.span.col as usize,
                },
                Vec::new(),
            );
        }
    };
    let (res, warns) = checker::check_graph_diags(&graph, entry_fn);
    let outcome = match res {
        Ok(()) => CheckOutcome::Ok,
        Err(errs) => CheckOutcome::Errors(errs),
    };
    (outcome, warns)
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

/// Reject a path that reached us through a **lossy** argv decode (W7-6).
///
/// `OsStr::to_string_lossy` is NOT injective: every invalid byte becomes `U+FFFD`, so the raw path
/// `sc\xffipt.chz` and a real file literally named `sc\u{FFFD}ipt.chz` decode to the same string. A
/// path selects *which code runs*, so silently opening the alias would be strictly worse than the
/// rc=101 host panic `args_os()` replaced — it would run a different program and exit 0. Argv and env
/// values surfaced to Chezzi as `str` stay lossy (they select nothing); a path does not.
///
/// ponytail: refusing is the v1 ceiling — running a genuinely non-UTF-8-named script needs `OsString`
/// threaded through the resolver and module graph, its own milestone (docs/gaps.md W7-6).
fn reject_lossy_path(path: &str) -> bool {
    if path.contains('\u{FFFD}') {
        eprintln!(
            "chezzi: cannot use '{path}' as a path — it contains U+FFFD, which is how a non-UTF-8 \
             argument decodes, so it may not name the file you meant"
        );
        return true;
    }
    false
}

fn read_source(path: &str) -> Option<String> {
    if reject_lossy_path(path) {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("chezzi: cannot read '{path}': {e}");
            None
        }
    }
}

/// Render checker diagnostics as the `--errors=json` array. ONE renderer for both severities so an
/// error and a warning can never drift in shape — they differ only in the `severity` value.
fn diags_json(diags: &[checker::CheckError]) -> String {
    let items: Vec<String> = diags
        .iter()
        .map(|d| {
            let severity = match d.severity {
                checker::Severity::Error => "error",
                checker::Severity::Warning => "warning",
            };
            format!(
                "{{\"line\":{},\"col\":{},\"severity\":\"{severity}\",\"message\":{}}}",
                d.span.line,
                d.span.col,
                json_string(&d.message)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Print non-fatal checker warnings as plain text on **stderr**. Always plain text, even under
/// `--errors=json`: the only caller that reaches here in machine mode is `chezzi run`, whose stdout
/// belongs to the program and whose stderr is shared with the program's own — a JSON array there
/// could not be parsed out of that stream anyway, and rendering one would only invite a consumer to
/// try. The machine-readable path for warnings is `chezzi check --errors=json`, which puts them in
/// the ONE array on stdout ([`report_check_errors`]) and never calls this.
///
/// Which stream carries a warning, per command, and why:
/// * `chezzi check` — stdout IS the diagnostic document. Machine mode: the stdout array, beside the
///   errors. Plain text: here, on stderr, above the verdict. Exactly one of the two, ever.
/// * `chezzi run` / `chezzi test` — stdout belongs to the running program. Always here, on stderr,
///   in both modes; the stdout array stays errors-only so nothing is reported twice.
fn report_check_warnings(warns: &[checker::CheckError]) {
    for w in warns {
        eprintln!("{w}");
    }
}

/// Print type errors as plain text (default) or a JSON array (`--errors=json`). `warns` rides the
/// SAME json array (a machine consumer gets one document, keyed by `severity`); in plain text the
/// caller has already put them on stderr, and the trailing count stays errors-only. Pass `&[]` from
/// any command whose warnings already went to stderr — see [`report_check_warnings`].
fn report_check_errors(errs: &[checker::CheckError], warns: &[checker::CheckError], json: bool) {
    if json {
        let all: Vec<checker::CheckError> = warns.iter().chain(errs).cloned().collect();
        println!("{}", diags_json(&all));
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
/// prefix); JSON goes through the SAME [`diags_json`] renderer as a type error, carrying the clean
/// `message` (no embedded Display prefix) — the `in module 'X':` attribution rides along inside
/// `message`, and the shape is identical object-for-object.
fn report_fatal(text: &str, message: &str, line: usize, col: usize, json: bool) {
    if json {
        // Same renderer as a type error, so `severity` is present on EVERY object a consumer can
        // receive — a schema that carries the key only sometimes is worse than one that never does.
        // `as u32`: a `Span`'s own width, widened to `usize` on the way in and narrowed back.
        let span = chezzi::lexer::Span {
            line: line as u32,
            col: col as u32,
            file: 0,
        };
        println!(
            "{}",
            diags_json(&[checker::CheckError::error(message.to_string(), span)])
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
        let (stdout, stderr, result, _exit) =
            vm::run_file_with_entry(&main, native::HostConfig::from_process(vec![]), None, None);
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
        let report = test_runner::run_tests(&d.0);
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
        let (module_path, entry_fn) =
            manifest::split_entrypoint(m.entrypoint.as_deref().unwrap()).unwrap();
        assert_eq!(module_path, "src.main");
        assert_eq!(entry_fn, Some("main"));

        // find_root_from_dir(root) → the project root (the cwd case for bare `chezzi run`).
        let root = resolver::find_root_from_dir(&d.0).expect("chezzi.toml marks the root");

        // The module path resolves root-relatively to src/main.chz.
        let entry = manifest::entrypoint_file(module_path, &root).expect("module path resolves");
        assert!(
            entry.ends_with("src/main.chz"),
            "entry: {}",
            entry.display()
        );

        // Calling the named entry function prints the greeting.
        let (stdout, stderr, result, _exit) = vm::run_file_with_entry(
            &entry,
            native::HostConfig::from_process(vec![]),
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
        assert_eq!(
            manifest::split_entrypoint("src.main").unwrap(),
            ("src.main", None)
        );
        assert_eq!(
            manifest::split_entrypoint("src.main:main").unwrap(),
            ("src.main", Some("main"))
        );
        // Split on the FIRST colon (module path can't contain one; the rest is the fn name verbatim).
        assert_eq!(
            manifest::split_entrypoint("a.b:c:d").unwrap(),
            ("a.b", Some("c:d"))
        );
        // A trailing `:` with no function name is rejected (would otherwise be an empty fn name).
        let err = manifest::split_entrypoint("src.main:").unwrap_err();
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
            manifest::entrypoint_file("src.main", root).unwrap(),
            root.join("src/main.chz")
        );
        // Surrounding whitespace on segments is trimmed before the path is built, so a padded
        // entrypoint resolves to the same file as its trimmed form (was: `<root>/ app .chz`).
        assert_eq!(
            manifest::entrypoint_file(" app ", root).unwrap(),
            root.join("app.chz")
        );
        assert_eq!(
            manifest::entrypoint_file(" src . main ", root).unwrap(),
            root.join("src/main.chz")
        );
        // Bad forms must be REJECTED, not mangle the root path (push("") + set_extension footgun).
        // `"a. .b"` has a whitespace-only middle segment that trims to empty → still rejected.
        for bad in ["", "   ", ".", ".main", "src.main.", "src..main", "a. .b"] {
            assert!(
                manifest::entrypoint_file(bad, root).is_err(),
                "entrypoint {bad:?} should be rejected"
            );
        }
        // An embedded path separator ('/' or '\\') would resolve by accident via PathBuf::push
        // instead of the documented dotted form — reject it with a clear message.
        for bad in ["src/main", "src\\main", "a/b.c"] {
            let e = manifest::entrypoint_file(bad, root).unwrap_err();
            assert!(
                e.contains("'.' separators"),
                "entrypoint {bad:?} err should mention '.' separators, got: {e}"
            );
        }
    }

    /// `resolve_threads_env` is the parsing half of `CHEZZI_THREADS` for both `cmd_run` and
    /// `cmd_test` (task 5's fix: `test` used to silently ignore the var). Pure function, no env-var
    /// mutation, so this is race-free against every other test in this binary.
    #[test]
    fn resolve_threads_env_cases() {
        assert_eq!(super::resolve_threads_env(None), Ok(None), "unset");
        assert_eq!(super::resolve_threads_env(Some("")), Ok(None), "empty");
        assert_eq!(
            super::resolve_threads_env(Some("   ")),
            Ok(None),
            "whitespace-only"
        );
        assert_eq!(
            super::resolve_threads_env(Some("0")),
            Ok(Some(0)),
            "0 = auto"
        );
        assert_eq!(super::resolve_threads_env(Some("2")), Ok(Some(2)));
        assert_eq!(
            super::resolve_threads_env(Some(" 4 ")),
            Ok(Some(4)),
            "surrounding whitespace trimmed"
        );
        assert_eq!(
            super::resolve_threads_env(Some("nope")),
            Err("nope".to_string())
        );
        assert_eq!(
            super::resolve_threads_env(Some("-1")),
            Err("-1".to_string()),
            "negative is not a valid usize"
        );
    }
}
