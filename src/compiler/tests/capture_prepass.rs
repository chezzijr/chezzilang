//! Uniform by-reference capture, Task A: the compile-time capture pre-pass computes the
//! boxed-name set (names of this frame's locals captured by a nested closure/defer/spawn body).
//! Unwired this task — nothing reads the set yet; these tests pin the pre-pass computation.
use super::*;
use std::collections::HashSet;

/// Parse `src`, return the first top-level `fn`'s [`FnDecl`].
fn first_fn(src: &str) -> FnDecl {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    for s in &module.stmts {
        if let StmtKind::Fn(decl) = &s.kind {
            return decl.clone();
        }
    }
    panic!("no top-level fn in source");
}

#[test]
fn prepass_boxes_captured_local_not_uncaptured() {
    // `x` is captured by the nested closure; `y` is not. Only `x` is boxed.
    let decl =
        first_fn("fn f():\n    x := 1\n    y := 2\n    g := fn() -> int: x\n    return g()\n");
    let boxed = captured_names_of_body(&decl.body, &decl.params);
    assert_eq!(boxed, HashSet::from(["x".to_string()]));
    assert!(!boxed.contains("y"), "uncaptured local y must not be boxed");
}

#[test]
fn prepass_recurses_through_nested_closures() {
    // A grandparent local captured two closures deep must surface (free-var descent): the inner
    // closure references `x`, so `x` is free in the outer closure `g`, which is `f`'s direct child.
    let decl =
        first_fn("fn f():\n    x := 1\n    g := fn() -> int: (fn() -> int: x)()\n    return g()\n");
    let boxed = captured_names_of_body(&decl.body, &decl.params);
    assert!(
        boxed.contains("x"),
        "grandchild-captured x must be boxed; got {boxed:?}"
    );
}

#[test]
fn prepass_boxes_captured_param() {
    // A parameter captured by a nested closure is attributed to this frame's slot.
    let decl = first_fn("fn f(n: int):\n    g := fn() -> int: n\n    return g()\n");
    let boxed = captured_names_of_body(&decl.body, &decl.params);
    assert!(
        boxed.contains("n"),
        "captured param n must be boxed; got {boxed:?}"
    );
}

#[test]
fn is_boxed_slot_reads_boxed_names() {
    let mut fc = FnComp::new("f".to_string(), 0, false);
    fc.locals.push(LocalVar {
        name: "x".to_string(),
        depth: 0,
    });
    fc.locals.push(LocalVar {
        name: "y".to_string(),
        depth: 0,
    });
    fc.boxed_names = HashSet::from(["x".to_string()]);
    assert!(fc.is_boxed_slot(0), "slot 0 (x) is boxed");
    assert!(!fc.is_boxed_slot(1), "slot 1 (y) is not boxed");
}

#[test]
fn name_floor_hides_the_frame_from_both_name_readers() {
    let mut fc = FnComp::new("f".to_string(), 0, false);
    fc.add_local("n".to_string());
    fc.name_floor = fc.locals.len();
    assert_eq!(
        fc.resolve_local("n"),
        None,
        "a local below the floor is invisible to resolve_local"
    );
    assert!(
        fc.snapshot_entries().is_empty(),
        "a local below the floor is invisible to a capture snapshot"
    );
    let slot = fc.add_local("x".to_string());
    assert_eq!(
        slot, 1,
        "a binder above the floor allocates from slot_count"
    );
    assert_eq!(fc.resolve_local("x"), Some(1));
    let snap = fc.snapshot_entries();
    let names: Vec<&str> = snap.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["x"]);
}
