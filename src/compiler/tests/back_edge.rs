//! Cancellation points: a cancel is delivered at loop BACK-EDGES (plus blocking ops), so a hot
//! loop stays cancellable (`Vm::jump_checked`, src/vm/exec.rs). That rests on every loop form
//! lowering to a BACKWARD `Op::Jump` — pin it here so a future peephole pass cannot thread the
//! back-edge out of existence and silently make hot loops uncancellable.
use super::*;
use crate::base::op::Op;

fn has_back_edge(src: &str) -> bool {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let prog = compile_module_standalone(&module).expect("compile");
    prog.protos.iter().any(|p| {
        p.code
            .iter()
            .enumerate()
            .any(|(k, op)| matches!(op, Op::Jump(t) if *t <= k))
    })
}

#[test]
fn loop_back_edge_is_a_backward_jump() {
    assert!(
        has_back_edge("fn f():\n    i := 0\n    while i < 3:\n        i = i + 1\nf()\n"),
        "`while` must lower to a backward Op::Jump (the cancellation checkpoint)"
    );
    assert!(
        has_back_edge("fn f():\n    for x in [1, 2]:\n        print(x)\nf()\n"),
        "`for` must lower to a backward Op::Jump (the cancellation checkpoint)"
    );
}
