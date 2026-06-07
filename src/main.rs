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
mod interp;
mod json_decode;
mod lexer;
mod native;
mod parser;
mod resolver;
mod vm;

#[cfg(test)]
mod conformance;

use std::process::ExitCode;

const USAGE: &str = "\
chezzi — the Chezzi language toolchain

USAGE:
    chezzi <command> [flags] <file.chz> [program args...]

COMMANDS:
    run     <file>   Type-check, then run on the bytecode VM  (M5)
    check   <file>   Type-check only; report errors          (M4)
    tokens  <file>   Print the token stream                  (M1)
    ast     <file>   Print the parsed AST                    (M2)
    repl             Start an interactive REPL                (M1+)
    help             Show this message

FLAGS:
    --errors=json    Emit type errors as JSON (for `check` / `run`)
    --interp         Run on the tree-walk interpreter instead of the bytecode VM

NOTE: flags must come BEFORE the file path. Anything after the file is passed
      to the program as an argument, so `chezzi run prog.chz --interp` runs the
      default VM and hands `--interp` to the program. Use `chezzi run --interp prog.chz`.
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

/// `chezzi run <file> [--errors=json] [--interp]` — type-check first (M4 gate), then execute on
/// the bytecode VM (default, M5) or the tree-walk interpreter (`--interp`, the reference engine).
fn cmd_run(args: &[String]) -> ExitCode {
    let mut path = None;
    let mut json = false;
    let mut use_vm = true;
    // Positional args after the script path are the program's own args (std.os.args).
    // GOTCHA: this means flags MUST precede the file — `chezzi run prog.chz --interp`
    // treats `--interp` as a program arg (path is already set) and silently runs the
    // default VM. Correct form: `chezzi run --interp prog.chz`.
    let mut prog_args: Vec<String> = Vec::new();
    for arg in args {
        match arg.as_str() {
            _ if path.is_some() => prog_args.push(arg.clone()),
            "--errors=json" => json = true,
            "--vm" => use_vm = true,
            "--interp" => use_vm = false,
            other if other.starts_with("--") => {
                eprintln!("chezzi run: unknown flag '{other}'");
                return ExitCode::FAILURE;
            }
            other => path = Some(other.to_string()),
        }
    }
    let Some(path) = path else {
        eprintln!("chezzi run: missing file argument\nusage: chezzi run <file.chz> [--errors=json] [--interp]");
        return ExitCode::FAILURE;
    };
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
    let (output, errout, errored) = if use_vm {
        let (out, err, result) = vm::run_file_with(p, cfg);
        (out, err, result.err().map(|e| e.to_string()))
    } else {
        let (out, err, result) = interp::run_file_with(p, cfg);
        (out, err, result.err().map(|e| e.to_string()))
    };
    print!("{output}");
    // Flush program stderr (std.io.eprint output) to the real stderr.
    eprint!("{errout}");
    match errored {
        None => ExitCode::SUCCESS,
        Some(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

enum CheckOutcome {
    Ok,
    Errors(Vec<checker::CheckError>),
    /// A lex or parse error (the program never reaches the checker). `text` is the full rendered
    /// message; `line`/`col` are carried so `--errors=json` still emits structured output.
    Fatal { text: String, line: usize, col: usize },
}

/// Resolve the module graph, then type-check it, normalizing each failure mode. A resolve, lex, or
/// parse failure (in the entry or any imported module) is `Fatal`; type errors are `Errors`.
fn type_check(path: &str) -> CheckOutcome {
    let graph = match resolver::build_graph(std::path::Path::new(path)) {
        Ok(g) => g,
        Err(e) => {
            return CheckOutcome::Fatal { text: e.to_string(), line: e.span.line, col: e.span.col };
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
        println!("[{{\"line\":{line},\"col\":{col},\"message\":{}}}]", json_string(text));
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

