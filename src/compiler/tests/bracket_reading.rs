//! TICKET-300 (SC-6): a one-arg bracketed call `head[k](args)` has two readings (DEC-210). The
//! checker records which one it took on the CALL node, for both outcomes; the compiler reads it
//! and never guesses.
use super::*;

/// The `Debug` of every resolution the checker recorded for `src`'s module, one per line.
fn recorded(src: &str) -> Vec<String> {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let mut module = crate::parser::parse(tokens).expect("parse");
    crate::desugar::run_standalone(&mut module).expect("desugar");
    let graph = crate::resolver::standalone_graph(&module.stmts);
    let (_, tables) = crate::checker::resolve_compile_tables(&graph);
    let (.., rs, _, _, _, _, _) = tables;
    rs.values().map(|r| format!("{r:?}")).collect()
}

fn count(rs: &[String], name: &str) -> usize {
    rs.iter().filter(|r| r.as_str() == name).count()
}

#[test]
fn a_type_args_bracket_call_records_its_reading() {
    let rs = recorded(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    y := id[int](1)\n    print(y)\n",
    );
    assert_eq!(count(&rs, "TypeArgsCall"), 1, "reading counts");
    assert_eq!(count(&rs, "IndexCall"), 0, "reading counts");
}

#[test]
fn an_index_bracket_call_records_its_reading() {
    let rs = recorded(
        "fn one(x: int) -> int:\n    return x\nfn main():\n    fs := [one]\n    k := 0\n    print(fs[k](1))\n",
    );
    assert_eq!(count(&rs, "IndexCall"), 1, "reading counts");
    assert_eq!(count(&rs, "TypeArgsCall"), 0, "reading counts");
}
