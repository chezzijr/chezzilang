//! TICKET-040: `stmt_has_bare_spawn`'s catch-all does not descend into `ExprKind::Recover`'s
//! `Block`, so a bare `spawn` inside a `recover:` body never triggers `Op::EnterNursery` and
//! faults at runtime with "spawn must be inside a parallel: block".
use crate::vm::run_capture;

#[test]
fn spawn_inside_recover_block_runs() {
    let src = "r := recover:\n    spawn: print(\"task\")\n    1\nprint(\"r={r}\")\n";
    let out = run_capture(src).expect("program should run without a top-level fault");
    assert!(
        out.contains("task"),
        "expected the spawned task to run and print 'task', got: {out:?}"
    );
}

#[test]
fn spawn_inside_recover_in_a_fn_value_position_runs() {
    let src = "fn f() -> int:\n    r := recover:\n        spawn: print(\"7\")\n        1\n    return 7\nf()\n";
    let out = run_capture(src).expect("program should run without a top-level fault");
    assert_eq!(
        out, "7\n",
        "expected stdout to be the single line '7', got: {out:?}"
    );
}

#[test]
fn spawn_inside_recover_in_a_struct_field_default_runs() {
    let src = "struct S:\n    n: int! = recover:\n        spawn: print(\"d\")\n        1\ns := S()\nprint(\"{s.n}\")\n";
    let out = run_capture(src).expect("program should run without a top-level fault");
    assert!(
        out.contains("d"),
        "expected the spawned task to run and print 'd', got: {out:?}"
    );
}

#[test]
fn spawn_inside_recover_survives_a_caught_fault() {
    let src = "fn main2():\n    r := recover:\n        spawn: print(\"t5\")\n        x := 1 / 0\n        print(\"{x}\")\n        1\n    print(\"r5={r}\")\nmain2()\n";
    let out = run_capture(src).expect("program should run without a top-level fault");
    assert!(
        out.contains("r5=!division by zero"),
        "expected stdout to contain r5=!division by zero, got: {out:?}"
    );
    assert!(
        out.contains("t5"),
        "expected the spawned task to still run and print 't5', got: {out:?}"
    );
}

#[test]
fn a_recover_without_a_bare_spawn_opens_no_nursery() {
    let tokens = crate::lexer::tokenize("r := recover:\n    1\n").expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let prog = super::compile_module_standalone(&module).expect("compile");
    assert!(
        prog.protos.iter().all(|p| !p.has_implicit_nursery),
        "expected no proto to carry an implicit nursery for a recover: with no bare spawn"
    );
}

#[test]
fn a_spawn_in_a_closure_body_recover_gates_only_the_closure() {
    let src = "r := recover:\n    f := fn() -> int!: recover:\n        spawn: print(\"9\")\n        1\n    1\n";
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let prog = super::compile_module_standalone(&module).expect("compile");
    let gated: Vec<&str> = prog
        .protos
        .iter()
        .filter(|p| p.has_implicit_nursery)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        gated,
        vec!["<closure>"],
        "expected exactly one proto gated, named '<closure>', got: {gated:?}"
    );
}
