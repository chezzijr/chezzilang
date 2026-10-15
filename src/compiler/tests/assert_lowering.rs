//! W8-13 — a failing comparison `assert` splits the condition so the operand VALUES survive the
//! comparison opcode. These pin the bytecode shape: a passing comparison assert must pop both
//! duplicated operands (no stack growth per execution), and a non-comparison assert must keep its
//! existing (unchanged) lowering.

use super::*;
use crate::base::op::Op;

fn compile(src: &str) -> crate::base::op::Program {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let mut module = crate::parser::parse(tokens).expect("parse");
    crate::desugar::run_standalone(&mut module).expect("desugar");
    compile_module_standalone(&module).expect("compile")
}

fn toplevel(p: &crate::base::op::Program) -> &crate::base::op::Proto {
    p.protos
        .iter()
        .find(|q| q.name == "<toplevel>")
        .expect("toplevel proto")
}

#[test]
fn passing_comparison_assert_pops_both_operands() {
    let prog = compile("i := 0\nassert i < 3\n");
    let code = &toplevel(&prog).code;
    let j = code
        .iter()
        .position(|op| matches!(op, Op::JumpIfFalse(_)))
        .unwrap_or_else(|| panic!("no JumpIfFalse in {code:?}"));
    assert!(matches!(code[j + 1], Op::Pop), "{code:?}");
    assert!(matches!(code[j + 2], Op::Pop), "{code:?}");
    assert!(matches!(code[j + 3], Op::Jump(_)), "{code:?}");
    assert_eq!(
        code.iter().filter(|op| matches!(op, Op::Dup2)).count(),
        1,
        "{code:?}"
    );
}

#[test]
fn non_comparison_assert_keeps_the_jump_adjacent() {
    let prog = compile("assert 4 in [1, 2, 3]\n");
    let code = &toplevel(&prog).code;
    let j = code
        .iter()
        .position(|op| matches!(op, Op::JumpIfFalse(_)))
        .unwrap_or_else(|| panic!("no JumpIfFalse in {code:?}"));
    assert!(matches!(code[j + 1], Op::Jump(_)), "{code:?}");
    assert_eq!(
        code.iter().filter(|op| matches!(op, Op::Dup2)).count(),
        0,
        "{code:?}"
    );
}
