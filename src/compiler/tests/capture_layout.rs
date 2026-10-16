//! M19 lever #3: captures are positional. `GetCaptured` carries a compile-time slot (u32), and
//! each closure proto records its capture names in slot order (`Proto.capture_names`).
use super::*;
use crate::base::op::Op;

fn compile(src: &str) -> crate::base::op::Program {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    compile_module_standalone(&module).expect("compile")
}

/// Find the first proto that reads a capture, returning its (proto, GetCaptured slots in code order).
fn captured_slots(prog: &crate::base::op::Program) -> Vec<u32> {
    for p in &prog.protos {
        let slots: Vec<u32> = p
            .code
            .iter()
            .filter_map(|op| match op {
                Op::GetCaptured(slot) => Some(*slot),
                _ => None,
            })
            .collect();
        if !slots.is_empty() {
            return slots;
        }
    }
    panic!("no GetCaptured op emitted");
}

#[test]
fn get_captured_carries_a_u32_slot() {
    // A closure reading one captured var emits GetCaptured(0) (a numeric slot, not a string).
    let prog = compile(
        "fn make(n: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + n\nmake(1)\n",
    );
    assert_eq!(captured_slots(&prog), vec![0], "single capture → slot 0");
}

#[test]
fn two_captures_get_distinct_slots_in_snapshot_order() {
    // `a` then `b` referenced; snapshot_entries orders innermost locals first (reverse decl).
    // Whatever the order, the two captures must occupy distinct, stable slots 0 and 1.
    let prog = compile(
        "fn make(a: int, b: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + a + b\nmake(1, 2)\n",
    );
    let mut slots = captured_slots(&prog);
    slots.sort_unstable();
    assert_eq!(slots, vec![0, 1], "two captures → slots 0 and 1");
}

#[test]
fn proto_records_capture_names_in_slot_order() {
    // The closure proto carries the captured names in slot order (cold-path metadata, mirrors
    // StructDef.fields). Slot i of capture_names is the name read by GetCaptured(i).
    let prog = compile(
        "fn make(a: int, b: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + a + b\nmake(1, 2)\n",
    );
    let clo = prog
        .protos
        .iter()
        .find(|p| !p.capture_names.is_empty())
        .expect("a closure proto with captures");
    assert_eq!(clo.capture_names.len(), 2, "two captured names recorded");
    let mut names = clo.capture_names.clone();
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn non_closure_proto_has_no_capture_names() {
    // A plain top-level fn captures nothing; its proto's capture_names is empty.
    let prog = compile("fn plain(x: int) -> int:\n    return x + 1\nplain(1)\n");
    for p in &prog.protos {
        // Only the closure proto (none here) would be non-empty.
        assert!(
            p.capture_names.is_empty(),
            "plain fn proto has no capture names"
        );
    }
}

/// Native-prelude phase 1 — BYTE-IDENTICAL LOWERING PIN. Direct calls of the four first-class
/// universe fns must lower to their specialized opcodes regardless of whether the intrinsic is
/// selected by an ad-hoc match arm or by the `Builtin` id table. This is the
/// characterization lock the refactor must not move: `print(x)`→`CallPrint(1)`,
/// `print(x, sep=…)`→`CallPrintSep{argc:1}`, `ord("a")`/`panic("m")`→`CallBuiltin(name, 1)`.
fn all_ops(prog: &crate::base::op::Program) -> Vec<Op> {
    prog.protos
        .iter()
        .flat_map(|p| p.code.iter().cloned())
        .collect()
}

/// TICKET-184 — the fall-off of an annotated non-nil fn the checker proved cannot fall off traps
/// instead of returning a silent `nil`; a legal fall-off (unannotated, alias to `nil`) does not.
#[test]
fn fall_off_trap_only_after_a_proved_unreachable_end() {
    let traps = |src: &str| {
        all_ops(&compile(src))
            .iter()
            .filter(|op| matches!(op, Op::ConstStr(s) if s.contains("fell off the end")))
            .count()
    };
    // A standalone module also carries the prelude's `Range` methods (TICKET-253): count against
    // the program that holds only those.
    let base = traps("print(1)\n");
    assert_eq!(
        traps("fn a() -> int:\n    return 1\nprint(a())\n"),
        base + 1
    );
    assert_eq!(traps("fn b():\n    print(1)\nb()\n"), base);
    assert_eq!(
        traps("type N = None\nfn f() -> N:\n    print(1)\nf()\n"),
        base
    );
}

#[test]
fn direct_builtin_calls_lower_to_specialized_opcodes() {
    let ops = all_ops(&compile("print(1)\n"));
    assert!(
        ops.iter().any(|o| matches!(o, Op::CallPrint(1))),
        "print(x) must lower to CallPrint(1); got {ops:?}"
    );

    let ops = all_ops(&compile("print(1, sep=\"-\")\n"));
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::CallPrintSep { argc: 1 })),
        "print(x, sep=…) must lower to CallPrintSep{{argc:1}}; got {ops:?}"
    );

    let ops = all_ops(&compile("ord(\"a\")\n"));
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n.name() == "ord")),
        "ord(c) must lower to CallBuiltin(\"ord\", 1); got {ops:?}"
    );

    let ops = all_ops(&compile("panic(\"m\")\n"));
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n.name() == "panic")),
        "panic(m) must lower to CallBuiltin(\"panic\", 1); got {ops:?}"
    );

    // Phase 2a — the scalar-conversion CTORS must STILL lower to CallBuiltin,
    // byte-identical to their old `is_builtin` fall-through (now `Lowering::Call`).
    for name in ["int", "float", "bool", "str", "bytes", "bytearray"] {
        let ops = all_ops(&compile(&format!("{name}(\"5\")\n")));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n.name() == name)),
            "{name}(x) must lower to CallBuiltin(\"{name}\", 1); got {ops:?}"
        );
    }

    // Phase 2b — the GENERIC / reserved-type container CTORS must STILL lower to the name-keyed
    // CallBuiltin, byte-identical to their old hard-coded `is_builtin` arm (now `Lowering::Call`).
    // `List()`/`Map()`/`Set()` each lower to `CallBuiltin(id, 1)`.
    for name in ["List", "Map", "Set"] {
        let ops = all_ops(&compile(&format!("{name}([])\n")));
        assert!(
            ops.iter()
                .any(|o| matches!(o, Op::CallBuiltin(n, 1) if n.name() == name)),
            "{name}([]) must lower to CallBuiltin(\"{name}\", 1); got {ops:?}"
        );
    }
    // A turbofished container ctor lowers to the IDENTICAL opcode — type args are type-erased
    // before the compiler, so `List[int]()` == `List()` at the opcode level (zero args here).
    let ops = all_ops(&compile("List[int]()\n"));
    assert!(
        ops.iter()
            .any(|o| matches!(o, Op::CallBuiltin(n, 0) if n.name() == "List")),
        "List[int]() must lower to CallBuiltin(\"List\", 0); got {ops:?}"
    );
}
