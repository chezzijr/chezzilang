//! W7-43 — `?.` on a `Result` is `?` then `.`. THE load-bearing test in the change: the two
//! spellings must compile to byte-identical bytecode. If that holds, the VM and the
//! `recover:`/`defer:`/nursery interactions are green BY CONSTRUCTION —
//! there is no second code path to disagree about.
use super::*;
use crate::vm::op::Op;

/// Parse → desugar → compile, i.e. the production pipeline (`resolver::build_graph` always
/// desugars before the checker/backend). The carrier SURVIVES desugar now; the checker records
/// the lowering into the `CarrierTable` that `compile_module_standalone` harvests.
fn compile(src: &str) -> crate::vm::op::Program {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let mut module = crate::parser::parse(tokens).expect("parse");
    crate::desugar::run_standalone(&mut module).expect("desugar");
    compile_module_standalone(&module).expect("compile")
}

/// Every proto rendered as `name | opcodes`, in emission order. `Op` has no `PartialEq` (it is a
/// big hot enum, and deriving one for a test would be the tail wagging the dog), so compare its
/// `Debug` — which is strictly FINER than `==` would be.
fn ops(prog: &crate::vm::op::Program) -> Vec<String> {
    prog.protos
        .iter()
        .map(|p| format!("{} | {:?}", p.name, p.code))
        .collect()
}

/// The same, plus each op's source span.
fn ops_and_spans(prog: &crate::vm::op::Program) -> Vec<String> {
    prog.protos
        .iter()
        .map(|p| format!("{} | {:?} | {:?}", p.name, p.code, p.lines))
        .collect()
}

/// Compile `template` (which must contain the `?C?` marker) in three spellings and assert the
/// identity that W7-43 rests on.
///
/// - `f()?.len()` vs `f()? .len()` — the two spellings a USER writes. Their OPCODES must match;
///   their spans cannot, because inserting the space physically moves every column to its right
///   (that is a property of the source text, not of the lowering).
/// - `f() ?.len()` vs `f()? .len()` — column-ALIGNED (same length, `len` at the same column), so
///   here the per-op SPANS must match too. Same alignment trick as
///   `desugar::tests::lower_carrier_try_matches_the_spaced_spelling_exactly`.
fn assert_spellings_agree(template: &str) {
    assert!(template.contains("?C?"), "template needs the `?C?` marker");
    let tight = compile(&template.replace("?C?", "?."));
    let spaced = compile(&template.replace("?C?", "? ."));
    let aligned = compile(&template.replace("?C?", " ?."));
    assert_eq!(
        ops(&tight),
        ops(&spaced),
        "`?.` and `? .` must emit identical bytecode"
    );
    assert_eq!(
        ops_and_spans(&aligned),
        ops_and_spans(&spaced),
        "column-aligned, the per-op SPANS must match too"
    );
}

#[test]
fn result_carrier_compiles_identically_to_the_spaced_spelling() {
    let src = "fn f() -> str!str:\n    return ?\"hi\"\nfn g() -> int!str:\n    return ?(f()?C?len())\ng()\n";
    assert_spellings_agree(src);
    // …and the emitted program really IS the try-then-dot one (a SHARED bug would also be
    // "identical"): `Op::Try` is present, which only the Result lowering emits.
    let dot = compile(&src.replace("?C?", "?."));
    assert!(
        dot.protos
            .iter()
            .any(|p| p.code.iter().any(|o| matches!(o, Op::Try))),
        "the Result lowering must emit Op::Try"
    );
}

#[test]
fn result_carrier_method_with_args_compiles_identically() {
    assert_spellings_agree(
        "struct B:\n    v: int\n    fn add(self, n: int) -> int:\n        return self.v + n\nfn f() -> B!str:\n    return ?B(1)\nfn g() -> int!str:\n    return ?(f()?C?add(5))\ng()\n",
    );
}

#[test]
fn result_carrier_with_named_args_compiles_identically() {
    // Named args + an omitted default are bound by the checker (`Checker::bind_call`); the spaced
    // spelling is an ordinary `Call` bound the same way. Identical bytecode is the proof
    // that the two normalizations agree.
    assert_spellings_agree(
        "struct B:\n    v: int\n    fn tag(self, prefix: str = \"p\", n: int = 1) -> str:\n        return \"{prefix}{self.v + n}\"\nfn f() -> B!str:\n    return ?B(1)\nfn g() -> str!str:\n    return ?(f()?C?tag(n=5))\ng()\n",
    );
}

#[test]
fn result_carrier_inside_a_string_fragment_compiles_identically() {
    // Interpolation fragments are re-parsed after the module pass, so they carry their own
    // `kw_frag_ctx`/`kw_frag_ord` key discriminators — two fragments in one literal included.
    assert_spellings_agree(
        "fn f() -> str!str:\n    return ?\"hi\"\nfn g() -> str!str:\n    return ?\"{f()?C?len()}|{f()?C?len()}\"\ng()\n",
    );
}

#[test]
fn a_mixed_chain_lowers_each_link_by_its_own_operand() {
    // `a?.b?.c` with `a: Result` and `a.b: Option` — the two links take DIFFERENT lowerings and
    // share ONE node span, so the mode table must key on the name token. The Result link emits
    // `Op::Try`; the Option link emits the `match` lowering (a jump, and a `None` variant
    // construction). Both present in one proto = both links lowered on their own operand.
    let prog = compile(
        "struct I:\n    c: int\nstruct O:\n    b: I?\nfn f() -> O!str:\n    return ?O(?I(7))\nfn g() -> int?!str:\n    return ?(f()?.b?.c)\ng()\n",
    );
    let g = prog
        .protos
        .iter()
        .find(|p| p.name == "g")
        .expect("proto for g");
    assert!(
        g.code.iter().any(|o| matches!(o, Op::Try)),
        "the Result link must emit Op::Try; got {:?}",
        g.code
    );
    assert!(
        g.code.iter().any(|o| matches!(o, Op::Jump(_))),
        "the Option link must emit the match lowering's jump; got {:?}",
        g.code
    );
    // And the whole thing is identical to spelling the Result link with a space.
    let spaced = compile(
        "struct I:\n    c: int\nstruct O:\n    b: I?\nfn f() -> O!str:\n    return ?O(?I(7))\nfn g() -> int?!str:\n    return ?(f()? .b?.c)\ng()\n",
    );
    assert_eq!(ops(&prog), ops(&spaced));
}
