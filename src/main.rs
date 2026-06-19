//! Chezzi — a fast, statically-typed, Python-feel scripting language.
//!
//! Design spec: docs/spec.md
//!
//! Pipeline (built incrementally — see roadmap in docs/spec.md):
//!   source.chz → lexer → parser → checker → tree-walk interp → bytecode VM
//!
//! Status: pre-M1 scaffold. Subcommands below are stubs.

mod ast;
mod checker;
mod compiler;
mod desugar;
mod fmtspec;
mod interp;
mod json_decode;
mod lexer;
mod native;
mod parser;
mod resolver;
mod runtime;
mod slice;
mod test_runner;
mod vm;

#[cfg(test)]
mod conformance;

use std::process::ExitCode;

const USAGE: &str = "\
chezzi — the Chezzi language toolchain

USAGE:
    chezzi <command> [flags] <file.chz> [program args...]

COMMANDS:
    init    [dir]    Scaffold a new Chezzi project (manifest + src)
    run     <file>   Type-check, then run on the bytecode VM  (M5)
    test    [path]   Run every `test fn` in *_test.chz files  (M20)
    check   <file>   Type-check only; report errors          (M4)
    tokens  <file>   Print the token stream                  (M1)
    ast     <file>   Print the parsed AST                    (M2)
    repl             Start an interactive REPL                (M1+)
    help             Show this message

FLAGS:
    --errors=json    Emit type errors as JSON (for `check` / `run`)
    --serial         Run on the cooperative single-thread VM (the frozen parity oracle)
    --parallel       Select the OS-thread engine (now the DEFAULT; flag kept as a no-op)
    --threads=N      Worker threads for the OS-thread engine (0 = all cores; env: CHEZZI_THREADS)
    --interp         Run on the tree-walk interpreter instead of the bytecode VM

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
        "repl" => {
            eprintln!("chezzi: 'repl' is not implemented yet.");
            eprintln!("        see the roadmap in docs/spec.md");
            ExitCode::FAILURE
        }
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

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("chezzi: cannot read '{path}': {e}");
            return ExitCode::FAILURE;
        }
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

    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("chezzi: cannot read '{path}': {e}");
            return ExitCode::FAILURE;
        }
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

    match type_check(&path) {
        CheckOutcome::Ok => {
            println!("{}", if json { "[]" } else { "ok: no type errors" });
            ExitCode::SUCCESS
        }
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, json);
            ExitCode::FAILURE
        }
        CheckOutcome::Fatal { text, line, col } => {
            report_fatal(&text, line, col, json);
            ExitCode::FAILURE
        }
    }
}

/// `chezzi run <file> [--errors=json] [--interp] [--serial] [--parallel] [--threads=N]` —
/// type-check first (M4 gate), then execute on the bytecode VM (default, M5) or the tree-walk
/// interpreter (`--interp`, the reference engine). The VM now runs the real OS-thread engine
/// (B3.3-threads) BY DEFAULT; `--serial` opts back into the cooperative single-thread VM (the frozen
/// byte-identical oracle). `--parallel` is kept as an accepted no-op alias for the (now default)
/// OS-thread engine. `--threads=N` (or the `CHEZZI_THREADS` env var) sizes the OS-thread engine's
/// worker pool — `0` (or omitted) = all cores; the flag wins over the env var.
fn cmd_run(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut json = false;
    let mut use_vm = true;
    // The OS-thread engine (bounded pool + condvar `recv`) is now the DEFAULT VM engine.
    // `--serial` opts back into the cooperative single-thread VM (the frozen parity oracle).
    // `--interp` is the frozen sequential oracle and never runs parallel.
    let mut parallel = true;
    // Track explicit `--parallel`/`--serial` so contradictory combos (and `--interp --parallel`)
    // still error instead of silently picking one.
    let mut saw_parallel = false;
    let mut saw_serial = false;
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
            "--vm" => use_vm = true,
            "--interp" => use_vm = false,
            "--parallel" => saw_parallel = true,
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
    let Some(path) = path else {
        eprintln!(
            "chezzi run: missing file argument\nusage: chezzi run <file.chz> [--errors=json] [--interp] [--serial] [--parallel] [--threads=N]"
        );
        return ExitCode::FAILURE;
    };
    if saw_parallel && saw_serial {
        eprintln!("chezzi run: --parallel and --serial are mutually exclusive");
        return ExitCode::FAILURE;
    }
    if saw_parallel && !use_vm {
        eprintln!(
            "chezzi run: --parallel is VM-only and cannot combine with --interp (the interpreter is the frozen sequential engine)"
        );
        return ExitCode::FAILURE;
    }

    // Resolve the M:N worker count. An explicit `--threads` wins and errors if the parallel engine
    // won't run (contradiction); otherwise `CHEZZI_THREADS` applies only when it actually will, so a
    // stray env var never breaks `--serial`/`--interp`. `0`/unset both mean auto (all cores).
    let runs_parallel = use_vm && parallel;
    if let Some(n) = threads_flag {
        if !runs_parallel {
            let conflict = if !use_vm {
                "--interp (the interpreter is single-threaded)"
            } else {
                "--serial (the cooperative single-thread engine)"
            };
            eprintln!(
                "chezzi run: --threads sizes the parallel engine and has no effect with {conflict}"
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

    // Pre-run type check: type errors block execution (no partial output).
    match type_check(&path) {
        CheckOutcome::Ok => {}
        CheckOutcome::Errors(errs) => {
            report_check_errors(&errs, json);
            return ExitCode::FAILURE;
        }
        CheckOutcome::Fatal { text, line, col } => {
            report_fatal(&text, line, col, json);
            return ExitCode::FAILURE;
        }
    }

    // Print whatever the program emitted before any error, then the error itself. The native std
    // modules read args/env/stdin from a process-backed config.
    let p = std::path::Path::new(&path);
    let cfg = native::HostConfig::from_process(prog_args);
    let (output, errout, errored, exit_code) = if use_vm {
        let (out, err, result, code) = if parallel {
            vm::run_file_parallel(p, cfg)
        } else {
            vm::run_file_with(p, cfg)
        };
        (
            out,
            err,
            result
                .err()
                .map(|e| vm::format_trace(&e.message, e.span, &e.trace)),
            code,
        )
    } else {
        let (out, err, result, code) = interp::run_file_with(p, cfg);
        (
            out,
            err,
            result
                .err()
                .map(|e| interp::format_trace(&e.message, e.span, &e.trace)),
            code,
        )
    };
    print!("{output}");
    // Flush program stderr (std.io.eprint output) to the real stderr.
    eprint!("{errout}");
    // `std.os.exit(code)` takes precedence: a clean halt with the requested status.
    if let Some(code) = exit_code {
        return ExitCode::from(code as u8);
    }
    match errored {
        None => ExitCode::SUCCESS,
        Some(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

/// `chezzi test [path]` — discover + run every `test fn` in `*_test.chz` files under `path` (default
/// cwd; a single `*_test.chz` file runs that file; a directory is walked recursively). Reports
/// `PASS/FAIL name (file:line) msg` per test, a summary, and a non-zero exit if anything failed.
fn cmd_test(args: &[String]) -> ExitCode {
    let mut path: Option<String> = None;
    for arg in args {
        match arg.as_str() {
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
    let root = path.unwrap_or_else(|| ".".to_string());
    let report = test_runner::run_tests(std::path::Path::new(&root));
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
                "  chezzi.toml          project manifest (marker / tooling-only — not parsed yet)"
            );
            println!(
                "  src/main.chz         entry script  — run with: chezzi run {}/src/main.chz",
                dir
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
/// unit-testable core behind `cmd_init`. The manifest is written as a string literal only; nothing
/// parses `chezzi.toml` (it stays a marker / forward-looking tooling artifact, per docs/spec.md).
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
         # NOTE: this file is NOT parsed by the toolchain yet. It is a project ROOT MARKER\n\
         # (module resolution walks up for it; see docs/spec.md \"Imports & module resolution\")\n\
         # and a place for forward-looking, tooling-only configuration. The fields below are a\n\
         # sensible default; the commented sections document settings a future build tool may read.\n\
         \n\
         [project]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         # root = \".\"            # forward-looking: project root dir (default: this manifest's dir)\n\
         # entrypoint = \"main\"   # forward-looking (docs/spec.md): the fn a project build would run.\n\
         #                       #   The language has NO automatic main — src/main.chz calls main()\n\
         #                       #   itself today; this field is tooling-only and not parsed yet.\n\
         \n\
         # [test]               # forward-looking: test discovery config (not parsed yet)\n\
         # include = [\"*_test.chz\"]   # `chezzi test` already discovers *_test.chz files today.\n",
    );
    fs::write(&manifest, manifest_body)
        .map_err(|e| format!("cannot write {}: {e}", manifest.display()))?;

    let src = dir.join("src");
    fs::create_dir_all(&src).map_err(|e| format!("cannot create {}: {e}", src.display()))?;

    let main_chz = "# src/main.chz — program entry.\n\
        #\n\
        # Chezzi is a scripting language: code runs top-to-bottom and there is NO automatic\n\
        # entry point (see docs/syntax.md \"9b. Program entry\"). `main` is an ordinary function;\n\
        # define it and call it yourself.\n\
        \n\
        fn main():\n\
        \x20   print(\"Hello from Chezzi!\")\n\
        \n\
        main()        # nothing runs main for you\n";
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

enum CheckOutcome {
    Ok,
    Errors(Vec<checker::CheckError>),
    /// A lex or parse error (the program never reaches the checker). `text` is the full rendered
    /// message; `line`/`col` are carried so `--errors=json` still emits structured output.
    Fatal {
        text: String,
        line: usize,
        col: usize,
    },
}

/// Resolve the module graph, then type-check it, normalizing each failure mode. A resolve, lex, or
/// parse failure (in the entry or any imported module) is `Fatal`; type errors are `Errors`.
fn type_check(path: &str) -> CheckOutcome {
    let graph = match resolver::build_graph(std::path::Path::new(path)) {
        Ok(g) => g,
        Err(e) => {
            return CheckOutcome::Fatal {
                text: e.to_string(),
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

/// Report a fatal lex/parse error, preserving the `--errors=json` contract (valid JSON on stdout).
fn report_fatal(text: &str, line: usize, col: usize, json: bool) {
    if json {
        println!(
            "[{{\"line\":{line},\"col\":{col},\"message\":{}}}]",
            json_string(text)
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

        // Actually execute the scaffolded entry on the VM (not just type-check it).
        let (stdout, stderr, result, _exit) = vm::run_file(&main);
        assert!(
            result.is_ok(),
            "scaffolded main.chz must run; stderr:\n{stderr}"
        );
        assert!(
            stdout.contains("Hello from Chezzi!"),
            "scaffolded main.chz should print the greeting; stdout:\n{stdout}"
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
    fn init_creates_missing_dir() {
        let d = TmpDir::new();
        let nested = d.0.join("a/b/c");
        scaffold_project(&nested).expect("should create nested dir");
        assert!(nested.join("chezzi.toml").is_file());
        assert!(nested.join("src/main.chz").is_file());
    }
}
