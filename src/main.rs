//! Chezzi — a fast, statically-typed, Python-feel scripting language.
//!
//! Design spec: docs/spec.md
//!
//! Pipeline (built incrementally — see roadmap in docs/spec.md):
//!   source.chz → lexer → parser → checker → tree-walk interp → bytecode VM
//!
//! Status: pre-M1 scaffold. Subcommands below are stubs.

use std::process::ExitCode;

const USAGE: &str = "\
chezzi — the Chezzi language toolchain

USAGE:
    chezzi <command> [file.chz]

COMMANDS:
    run     <file>   Run a Chezzi program            (M3+)
    tokens  <file>   Print the token stream          (M1)
    ast     <file>   Print the parsed AST            (M2)
    repl             Start an interactive REPL        (M1+)
    help             Show this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        "run" | "tokens" | "ast" | "repl" => {
            eprintln!("chezzi: '{cmd}' is not implemented yet (pre-M1 scaffold).");
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
