//! compiler: compile and run a program for the front end's own tests. The VM is a dev-dependency of
//! this crate; it runs the `Program` through `vm::run_built`, the same entry the CLI driver uses.

use crate::base::op::CompileError;
use crate::native::HostConfig;
use crate::vm::{RunError, on_vm_stack, run_built, to_str_output};

/// Run a single-module source string; its stdout, or the error that stopped it.
pub(crate) fn run_capture(src: &str) -> Result<String, RunError> {
    let src = src.to_string();
    let (out, _err, result, _code) = to_str_output(on_vm_stack(move || {
        run_built(
            || {
                let front = |message: String, span| CompileError { message, span };
                let tokens = crate::lexer::tokenize(&src)
                    .map_err(|e| front(e.to_string(), crate::lexer::Span::RUNTIME))?;
                let module = crate::parser::parse(tokens).map_err(|e| front(e.message, e.span))?;
                super::compile_module_standalone(&module)
            },
            HostConfig::default(),
            None,
            false,
        )
    }));
    result.map(|()| out)
}

/// Run the program whose entry file is `entry` (imports resolve from its directory).
pub(crate) fn run_file(entry: &std::path::Path) -> crate::vm::RunOutput {
    let entry = entry.to_path_buf();
    to_str_output(on_vm_stack(move || {
        run_built(
            || super::compile_entry_file(&entry, None),
            HostConfig::default(),
            None,
            false,
        )
    }))
}
