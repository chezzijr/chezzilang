//! Chezzi — a fast, statically-typed, Python-feel scripting language.
//!
//! Design spec: docs/spec.md
//!
//! Pipeline (built incrementally — see roadmap in docs/spec.md):
//!   source.chz → lexer → parser → checker → tree-walk interp → bytecode VM
//!
//! Status: pre-M1 scaffold. Subcommands below are stubs.

mod ast;
mod lexer;
mod parser;

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
        "tokens" => cmd_tokens(args.get(1)),
        "ast" => cmd_ast(args.get(1)),
        "run" | "repl" => {
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
