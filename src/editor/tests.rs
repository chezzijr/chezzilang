//! Editor-layer unit tests. Moved out of `src/editor/mod.rs`; `super::*` is that module.

use super::*;

fn diag(src: &str) -> Vec<Diag> {
    diagnostics(Path::new("/nonexistent/chezzi_editor/buf.chz"), src)
}

#[test]
fn diag_clean_empty() {
    assert_eq!(diag("x := 1\n"), Vec::new());
}

#[test]
fn diag_parse_error_pos() {
    let ds = diag("x := = 5\n");
    assert!(!ds.is_empty(), "broken syntax should produce a diagnostic");
    // The error is on the first line → 0-based line 0; col 6 (1-based) → 5 (0-based).
    assert_eq!(ds[0].line, 0);
    assert_eq!(ds[0].col, 5);
    assert!(ds[0].end_col > ds[0].col);
}

#[test]
fn diag_type_error_pos() {
    // `zzz` is undefined; the error sits on line 2 (1-based) → 0-based 1, col 6 → 5.
    let ds = diag("a := 1\nb := zzz\n");
    assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
    let d = &ds[0];
    assert_eq!(d.line, 1, "0-based line of the second source line");
    assert!(d.message.contains("zzz"));
    // The squiggle covers the whole `zzz` identifier (col 5..8, 0-based).
    assert_eq!(d.col, 5);
    assert_eq!(d.end_col, 8);
}

/// TICKET-007 criterion 14: a near-miss `help` suggestion rides the LSP's `message` field.
#[test]
fn diag_help_appended_to_message() {
    let ds = diag("struct T:\n    fn tick(self) -> int:\n        return 1\nt := T()\nt.tik()\n");
    assert!(!ds.is_empty(), "method typo should produce a diagnostic");
    let d = &ds[0];
    assert!(d.message.contains("has no method 'tik'"), "got: {d:?}");
    assert!(
        d.message.contains("help: did you mean 'tick'?"),
        "got: {d:?}"
    );
}

/// W8-17 — the LSP squiggle covers the member NAME, not the receiver. `lenght` is a 6-char
/// token starting at 0-based col 3 (1-based col 10, JSON `end_col` 16 minus one at each end).
#[test]
fn diag_member_miss_squiggles_the_member_name() {
    let ds = diag("xs := [1, 2, 3]\nxs.lenght()\n");
    assert!(!ds.is_empty(), "method typo should produce a diagnostic");
    let d = &ds[0];
    assert_eq!(d.line, 1);
    assert_eq!(d.col, 3);
    assert_eq!(d.end_col, 9);
}

fn hov(src: &str, line: u32, ch: u32) -> Option<HoverInfo> {
    hover(
        Path::new("/nonexistent/chezzi_editor/buf.chz"),
        src,
        line,
        ch,
    )
}

#[test]
fn hover_through_a_leading_bom_finds_the_number_token() {
    let h = hov("\u{feff}x := 12345\n", 0, 7).expect("hover on int through a leading BOM");
    assert_eq!(h.display, "int");
}

#[test]
fn semantic_tokens_ignore_a_leading_bom() {
    let src = "x := 12345\ns := \"ab\" # c\n";
    assert_eq!(
        semantic_tokens(&format!("\u{feff}{src}")),
        semantic_tokens(src)
    );
}

/// TICKET-252 -- a string token's extent is the lexer's: a hole may nest a literal of the SAME
/// quote (`"{d["k"]}"`), so a quote scan would end the string at the inner quote. The semantic
/// token, the caret width (`word_end_col`) and a `# comment` after the literal all agree.
#[test]
fn a_string_token_extent_covers_a_same_quote_hole() {
    // line 1: print("{d["k"]}") # c      `"` col 7 .. closing `"` col 16 (1-based)
    let src = "print(\"{d[\"k\"]}\") # c\n";
    let strings: Vec<_> = semantic_tokens(src)
        .into_iter()
        .filter(|t| t.token_type == STRING)
        .collect();
    assert_eq!(strings.len(), 1, "one string token, got {strings:?}");
    assert_eq!(
        (strings[0].line, strings[0].start, strings[0].len),
        (0, 6, 10)
    );
    // The caret of a diagnostic at the opening quote spans the whole literal: 0-based end col 16.
    assert_eq!(word_end_col(src, 1, 7), 16);
    // The `#` after the literal is a comment, and the `#` inside a nested literal is not.
    let src = "print(\"{'#'}\") # c\n";
    let comment_starts: Vec<_> = semantic_tokens(src)
        .into_iter()
        .filter(|t| t.token_type == COMMENT)
        .map(|t| t.start)
        .collect();
    assert_eq!(comment_starts, vec![15]);
    // A literal that runs on past the line is measured to the end of the line, as before.
    assert_eq!(word_end_col("x := \"{a\n", 1, 6), 8);
    // A nested chain of holes deeper than the one-line allowance falls back to the line's end.
    let deep = format!("{}x{}", "\"{".repeat(20), "}\"".repeat(20));
    assert_eq!(word_end_col(&deep, 1, 1) as usize, deep.chars().count());
}

#[test]
fn diag_columns_are_counted_from_the_first_visible_char_after_a_bom() {
    let ds = diag("\u{feff}print(nope)\n");
    assert_eq!((ds[0].line, ds[0].col, ds[0].end_col), (0, 6, 10));
}

#[test]
fn hover_local_type() {
    // `x := 1` — hovering the binding `x` reports its inferred type `int`.
    let h = hov("x := 1\n", 0, 0).expect("hover on local");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_for_binding_decl() {
    // `for i in [1,2,3]:` — hovering the binding `i` at the decl site (right after `for `)
    // reports its inferred element type `int`.
    let h = hov("for i in [1,2,3]:\n    print(i)\n", 0, 4).expect("hover on for binding");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_for_binding_body() {
    // The body use-site of the loop var still resolves (additive decl-site span must not regress).
    let h = hov("for i in [1,2,3]:\n    print(i)\n", 1, 10).expect("hover on for body use");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_prints_carrier_sugar() {
    // A hover reads the one type renderer, so it shows the sugar the user writes.
    let h =
        hov("fn f(x: Map[str, int!str]):\n    print(x)\n", 1, 10).expect("hover on param body use");
    assert_eq!(h.display, "Map[str, int!str]");
}

#[test]
fn hover_destructure_first() {
    // `a, b := (1, 2)` — hovering the first binding `a` (col 0) reports its tuple-element type.
    let h = hov("a, b := (1, 2)\nprint(a)\n", 0, 0).expect("hover on destructure first");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_destructure_second() {
    // The second binding `b` (col 3) reports its own tuple-element type.
    let h = hov("a, b := (1, 2)\nprint(a)\n", 0, 3).expect("hover on destructure second");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_single_let_regression() {
    // The single-name let path must NOT regress when destructure plumbing is added.
    let h = hov("x := 1\n", 0, 0).expect("hover on single let");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_typed_empty_decl_and_use_show_the_annotation() {
    // TICKET-238: a binding's type is known on its own statement, so the decl-site hover and a
    // use before any `push` both show it. The three `hover_refined_empty_*` tests were deleted:
    // their subject was a later `b.push(0)` refining `b := []`, which is now an error.
    let src = "fn main():\n b: List[int] = []\n print(b)\n b.push(0)\nmain()\n";
    let h = hov(src, 1, 1).expect("hover on empty-list decl");
    assert_eq!(h.display, "List[int]");
    let h = hov(src, 2, 7).expect("hover on a use of b before its first push");
    assert_eq!(h.display, "List[int]");
}

#[test]
fn hover_match_variant_bind() {
    // The `n` binding inside a variant pattern `Col.Val(n)` reports the payload type.
    let src =
        "enum Col:\n    Val(int)\n\nc := Col.Val(3)\nmatch c:\n    Col.Val(n):\n        print(n)\n";
    let h = hov(src, 5, 12).expect("hover on match variant bind");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_match_tuple_bind() {
    // The `a` binding inside a tuple pattern `(a, b)` reports its element type.
    let src = "p := (1, 2)\nmatch p:\n    (a, b):\n        print(a)\n";
    let h = hov(src, 2, 5).expect("hover on match tuple bind");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_fn_param_type() {
    // The use of param `a` inside the body reports the declared param type `str`.
    let h = hov("fn f(a: str):\n    a\n", 1, 4).expect("hover on param use");
    assert_eq!(h.display, "str");
}

#[test]
fn hover_fn_name() {
    // A bare use of a function name reports its function type.
    let h = hov("fn f(a: int) -> int:\n    return a\nf\n", 2, 0).expect("hover on fn name");
    assert_eq!(h.display, "fn(int) -> int");
}

#[test]
fn hover_literal() {
    // An integer literal reports `int`.
    let h = hov("y := 123\n", 0, 5).expect("hover on literal");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_type_alias_transparent() {
    // Hovering the type token `Id` in `x: Id` shows the RESOLVED type (`int`, the alias body).
    let h = hov("type Id = int\nx: Id = 5\nprint(x)\n", 1, 3).expect("hover on alias type token");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_param_type_token() {
    // The `int` param-type token (line0 col8) hovers to `int`.
    let h = hov("fn f(a: int) -> int:\n    return a\nprint(f(1))\n", 0, 8)
        .expect("hover on param type token");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_return_type_token() {
    // The return-type `int` token (line0 col16) hovers to `int`.
    let h = hov("fn f(a: int) -> int:\n    return a\nprint(f(1))\n", 0, 16)
        .expect("hover on return type token");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_field_type_token() {
    // The struct field-type `int` token (line1 col7) hovers to `int`.
    let h = hov("struct S:\n    x: int\ns := S(1)\nprint(s.x)\n", 1, 7)
        .expect("hover on field type token");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_struct_name_type_token() {
    // The `P` param-type token (line2 col8) hovers to the struct `P`.
    let h = hov(
        "struct P:\n    v: int\nfn g(p: P):\n    print(p.v)\ng(P(1))\n",
        2,
        8,
    )
    .expect("hover on struct-name type token");
    assert_eq!(h.display, "P");
}

#[test]
fn hover_generic_inner_type_token() {
    // The inner `int` of `List[int]` (line0 col9) hovers via composite recursion.
    let h =
        hov("xs: List[int] = [1]\nprint(xs)\n", 0, 9).expect("hover on generic inner type token");
    assert_eq!(h.display, "int");
}

#[test]
fn hover_generic_fn_param_type_no_latch() {
    // The param-type `T` (line0 col11) must show `T` (Ty::Param), not `?` (prepass-latch guard).
    let h = hov("fn f[T](x: T) -> T:\n    return x\nprint(f(1))\n", 0, 11)
        .expect("hover on generic fn param type token");
    assert_eq!(h.display, "T");
}

#[test]
fn hover_native_type_import_shows_doc() {
    // A per-name import of a native/reserved TYPE (`import Shared from std.concurrency`) must
    // show its builtin blurb on the IMPORT-LINE token — not "No information available". `Shared`
    // is at line0 col7. (The annotation use `Shared[int]` already worked via builtin_type_doc.)
    let src = "import Shared from std.concurrency\nfn main():\n    s: Shared[int] = Shared(0)\n    print(s.get())\n";
    let h = hov(src, 0, 7).expect("hover on native-type import token");
    assert!(h.display.contains("Shared"), "display: {:?}", h.display);
    assert!(
        h.doc
            .as_deref()
            .is_some_and(|d| d.contains("cross-task shared cell")),
        "import-line hover should carry the Shared blurb, got: {:?}",
        h.doc
    );
}

#[test]
fn hover_timer_import_shows_func_doc() {
    // `timer` is an ordinary std.time FUNCTION (`timer(ms) -> Channel[bool]`); its import-line token
    // (`import timer from std.time`, `timer` at line0 col7) shows its decl's doc.
    let src = "import timer from std.time\nfn main():\n    t := timer(10)\n    print(1)\n";
    let h = hov(src, 0, 7).expect("hover on timer import token");
    assert!(
        h.doc
            .as_deref()
            .is_some_and(|d| d.contains("one-shot timeout channel")),
        "timer import should carry its blurb, got: {:?}",
        h.doc
    );
}

#[test]
fn hover_generic_param_shadowing_decl_no_doc_leak() {
    // A type param that SHADOWS a documented same-named top-level decl resolves to Ty::Param —
    // an unrelated entity — so its annotation-token hover must NOT borrow the decl's docstring
    // (the name_docs fallback is keyed by bare name). `v: Item` is at line3 col20; the param
    // `Item` shadows the documented `struct Item`.
    let src = "# an item\nstruct Item:\n    x: int\nfn process[Item](v: Item) -> Item:\n    return v\nprint(1)\n";
    let h = hov(src, 3, 20).expect("hover on shadowing param type token");
    assert_eq!(h.display, "Item");
    assert_eq!(
        h.doc, None,
        "type param must not show the shadowed struct's doc"
    );
}

#[test]
fn hover_type_kind_is_type() {
    // A type-annotation hover is classified `HoverKind::Type`.
    let h = hov("x: int = 5\nprint(x)\n", 0, 3).expect("hover on let annotation type token");
    assert_eq!(h.kind, crate::checker::HoverKind::Type);
}

#[test]
fn hover_fn_shows_doc() {
    // A `#` comment immediately above a `fn` becomes its doc, surfaced on hover of the fn's use.
    let h = hov("# greet the world\nfn f():\n    1\nf\n", 3, 0).expect("hover on fn use");
    assert_eq!(h.display, "fn() -> None");
    assert_eq!(h.doc.as_deref(), Some("greet the world"));
}

#[test]
fn hover_doc_multiline_joins() {
    // Stacked `#` lines (no gap) join with a newline.
    let h = hov("# line one\n# line two\nfn f():\n    1\nf\n", 4, 0).expect("hover on fn use");
    assert_eq!(h.doc.as_deref(), Some("line one\nline two"));
}

#[test]
fn hover_doc_blank_line_detaches() {
    // A blank line between two comment blocks detaches the earlier one.
    let h = hov("# detached\n\n# attached\nfn f():\n    1\nf\n", 5, 0).expect("hover on fn use");
    assert_eq!(h.doc.as_deref(), Some("attached"));
}

#[test]
fn hover_doc_on_struct() {
    // Doc on a struct surfaces when hovering the struct constructor.
    let h = hov("# a 2D point\nstruct P:\n    x: int\nP(1)\n", 3, 0).expect("hover on struct ctor");
    assert_eq!(h.doc.as_deref(), Some("a 2D point"));
}

#[test]
fn hover_doc_on_method() {
    // Doc on a method surfaces when hovering the method call.
    let src = "struct C:\n    # double it\n    fn dbl(self) -> int:\n        return 2\nc := C()\nc.dbl()\n";
    let h = hov(src, 5, 2).expect("hover on method call");
    assert_eq!(h.doc.as_deref(), Some("double it"));
}

#[test]
fn hover_doc_on_top_level_let() {
    // Doc on a top-level let surfaces on the binding.
    let h = hov("# the answer\nanswer := 42\n", 1, 0).expect("hover on let binding");
    assert_eq!(h.display, "int");
    assert_eq!(h.doc.as_deref(), Some("the answer"));
}

#[test]
fn hover_inline_trailing_comment_not_doc() {
    // An inline trailing comment on the decl line is not picked up as a doc.
    let h = hov("fn f(): 1  # not a doc\nf\n", 1, 0).expect("hover on fn use");
    assert_eq!(h.doc, None);
}

#[test]
fn hover_local_shadowing_documented_global_has_no_doc() {
    // A param/local that shadows a documented top-level name must NOT borrow the global's doc
    // (`name_docs` is keyed by bare name). Repro from the adversarial review.
    let src = "# the seed value\nseed := 42\nfn f(seed: int) -> int:\n    return seed\n";
    // The param USE inside the body resolves to the local `seed` (scope 1) → no doc.
    let use_h = hov(src, 3, 11).expect("hover on param use");
    assert_eq!(use_h.kind, crate::checker::HoverKind::Local);
    assert_eq!(use_h.doc, None, "local use must not show the global's doc");
    // Guard: the genuine top-level `seed` binding still shows its doc.
    let global_h = hov(src, 1, 0).expect("hover on top-level let");
    assert_eq!(global_h.doc.as_deref(), Some("the seed value"));
}

#[test]
fn hover_none_on_operator() {
    // Hovering the `+` operator (a non-leaf, non-token position) yields no type.
    assert_eq!(hov("a := 1 + 2\n", 0, 7), None);
}

#[test]
fn hover_no_check_returns_none() {
    // The program does not type-check (`zzz` undefined) → hover returns None.
    assert_eq!(hov("x := zzz\n", 0, 0), None);
}

#[test]
fn hover_field_access() {
    // Hovering the field name `x` in `p.x` reports the field's type (`int`) and `Field` kind.
    // The Field node anchors recording on the field-name span, not the receiver's start span.
    let src = "struct P:\n    x: int\np := P(1)\np.x\n";
    let h = hov(src, 3, 2).expect("hover on field name");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Field);
}

#[test]
fn hover_utf16_emoji() {
    // 🙂 is one char but TWO UTF-16 units. The cursor on `n` arrives as a UTF-16 column; the
    // reverse conversion must land on `n` (str), not slip a column because of the astral char.
    // line 1: `out := "🙂" + n` — `n` is at char col 13, UTF-16 col 14 (🙂 adds one unit).
    let src = "n := \"a\"\nout := \"\u{1f642}\" + n\n";
    let h = hov(src, 1, 14).expect("hover on n after emoji");
    assert_eq!(h.display, "str");
}

#[test]
fn utf16_to_char_col_clamps_mid_surrogate() {
    // "a🙂": 'a' is 1 UTF-16 unit, 🙂 is 2 (cols 1 and 2 both fall within the emoji).
    assert_eq!(utf16_to_char_col("a\u{1f642}", 0), 0); // 'a'
    assert_eq!(utf16_to_char_col("a\u{1f642}", 1), 1); // boundary: start of 🙂
    assert_eq!(utf16_to_char_col("a\u{1f642}", 2), 1); // MID-surrogate → clamp to 🙂's start
    assert_eq!(utf16_to_char_col("a\u{1f642}", 3), 2); // past end → char count
}

#[test]
fn hover_free_fn_callee() {
    // Hovering the CALLEE `inc` of a call `inc(3)` reports the function's signature.
    let src = "fn inc(x: int) -> int:\n    return x + 1\ninc(3)\n";
    let h = hov(src, 2, 0).expect("hover on free-fn callee");
    assert_eq!(h.display, "fn(int) -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_struct_ctor_callee() {
    // Hovering the ctor callee `Vec2` of `Vec2(1, 2)` reports a fn from fields to the struct.
    let src = "struct Vec2:\n    x: int\n    y: int\nVec2(1, 2)\n";
    let h = hov(src, 3, 0).expect("hover on struct-ctor callee");
    assert_eq!(h.display, "fn(int, int) -> Vec2");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_generic_fn_callee() {
    // A generic fn callee reports its DECLARED signature with the type parameters intact.
    let src = "import Arithmetic from std.ops\nfn combine[T: Arithmetic](a: T, b: T) -> T:\n    return a + b\ncombine(1, 2)\n";
    let h = hov(src, 3, 0).expect("hover on generic-fn callee");
    assert_eq!(h.display, "fn(T, T) -> T");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_builtin_callee_print() {
    // `print` is the file-backed variadic decl `native fn print(...args: Any, sep, end)`, so hover
    // shows its fn type: the variadic `...Any`, then the defaulted keyword-only `sep`/`end` (part of
    // the fn type, `FnLabels::kw_fits`), returning `None`.
    let h = hov("print(\"hi\")\n", 0, 0).expect("hover on builtin print callee");
    assert_eq!(h.display, "fn(...Any, str = …, str = …) -> None");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_builtin_method_str() {
    // CASE 1: a builtin `str` method reports the inference-source signature (receiver stripped).
    let src = "s := \"abc\".upper()\n";
    // `upper` starts at char col 11 (`s := "abc".` is 11 chars).
    let h = hov(src, 0, 11).expect("hover on str builtin method");
    assert_eq!(h.display, "fn() -> str");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_builtin_method_list_len() {
    // `len` is a METHOD (not a free fn): `xs.len()` reports `fn() -> int` via list_method_sig.
    let src = "xs := [1, 2]\nn := xs.len()\n";
    // line 1 `n := xs.` → `len` at char col 8.
    let h = hov(src, 1, 8).expect("hover on list len method");
    assert_eq!(h.display, "fn() -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_stdlib_module_fn() {
    // CASE 2: a stdlib module fn (`math.sqrt`) reports its native FnSig.
    let src = "import std.math\nx := math.sqrt(2.0)\n";
    // line 1 `x := math.` → `sqrt` at char col 10.
    let h = hov(src, 1, 10).expect("hover on std.math.sqrt");
    assert_eq!(h.display, "fn(float) -> float");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_method_callee() {
    // Hovering a method name in `c.foo(2)` reports the method's CALL signature (receiver stripped).
    let src = "struct C:\n    n: int\n    fn foo(self, n: int) -> int:\n        return n\nc := C(1)\nc.foo(2)\n";
    let h = hov(src, 5, 2).expect("hover on method callee");
    assert_eq!(h.display, "fn(int) -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_enum_variant_callee() {
    // Hovering the variant-name `Val` of `Col.Val(3)` reports the variant's ctor signature.
    let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
    let h = hov(src, 3, 4).expect("hover on enum-variant callee");
    assert_eq!(h.display, "fn(int) -> Col");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_enum_variant_decl_name() {
    // Hovering the variant-name `Val` AT ITS DECLARATION (line 2, col 4) reports the variant's
    // ctor signature, matching the use-site hover (`fn(int) -> Col`).
    let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
    let h = hov(src, 2, 4).expect("hover on enum-variant decl name");
    assert_eq!(h.display, "fn(int) -> Col");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_generic_enum_variant_decl_name() {
    // LATCH/shape guard: a generic enum's variant decl preserves the enum's `Ty::Param` shape —
    // `Full(T)` Displays "fn(T) -> Box[T]" (T not `?`/Unknown). `Full` token starts at line 1 col 4.
    let src = "enum Box[T]:\n    Full(T)\n    Empty\nx := Box[int].Full(3)\n";
    let h = hov(src, 1, 4).expect("hover on generic enum-variant decl name");
    assert_eq!(h.display, "fn(T) -> Box[T]");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_nullary_enum_variant_decl_name() {
    // A no-payload variant decl `Red` (line 1, col 4) Displays "fn() -> Col" — the nullary
    // convention locked by hover_enum_variant_callee.
    let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
    let h = hov(src, 1, 4).expect("hover on nullary enum-variant decl name");
    assert_eq!(h.display, "fn() -> Col");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_enum_variant_receiver() {
    // Hovering the receiver `Col` of `Col.Val(3)` reports the enum type.
    let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
    let h = hov(src, 3, 0).expect("hover on enum-variant receiver");
    assert_eq!(h.display, "Col");
}

#[test]
fn hover_static_method_callee() {
    // Hovering the static-method name `default` of `Foo.default()` reports its call signature.
    let src =
        "struct Foo:\n    x: int\n    fn default() -> Foo:\n        return Foo(0)\nFoo.default()\n";
    let h = hov(src, 4, 4).expect("hover on static-method callee");
    assert_eq!(h.display, "fn() -> Foo");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_builtin_static_method_shows_its_decl() {
    // `List.from_iter` is declared in `std/prelude.chz`; its hover is that declaration's signature.
    let h = hov("xs := List.from_iter([1])\n", 0, 12).expect("hover on List.from_iter");
    assert_eq!(h.display, "fn(S) -> List[T]");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_static_method_receiver() {
    // Hovering the receiver `Foo` of `Foo.default()` reports the struct type.
    let src =
        "struct Foo:\n    x: int\n    fn default() -> Foo:\n        return Foo(0)\nFoo.default()\n";
    let h = hov(src, 4, 0).expect("hover on static-method receiver");
    assert_eq!(h.display, "Foo");
}

#[test]
fn hover_builtin_callee_chr() {
    // CONFIRMING (already green): a bare-name builtin callee (`chr`) records its display sig via
    // callee_display_ty -> builtin_sig. `len(...)` is method-only (not a free fn / not reserved-
    // callable), so `len(x)` is an undefined-name error and is out of scope here.
    let h = hov("chr(65)\n", 0, 0).expect("hover on builtin chr callee");
    assert_eq!(h.display, "fn(int) -> str");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_fn_param_decl() {
    // Hovering the param `a` at its DECL site in the signature reports its declared type.
    let h = hov("fn f(a: str):\n    a\n", 0, 5).expect("hover on fn param decl");
    assert_eq!(h.display, "str");
    assert_eq!(h.kind, crate::checker::HoverKind::Param);
}

#[test]
fn hover_method_param_decl() {
    // The same check_fn_body record also covers METHOD param decls.
    let src = "struct C:\n    n: int\n    fn foo(self, n: int) -> int:\n        return n\nc := C(1)\nc.foo(2)\n";
    let h = hov(src, 2, 17).expect("hover on method param decl");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Param);
}

#[test]
fn hover_closure_param_decl() {
    // Hovering a closure param `a` at its DECL site reports its annotated type.
    let h =
        hov("f := fn(a: int): a + 1\nprint(f(2))\n", 0, 8).expect("hover on closure param decl");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Param);
}

#[test]
fn hover_unannotated_closure_param_decl_in_generic_arg() {
    // An UNANNOTATED closure param passed to a generic fn: its decl-site hover must report the
    // INFERRED type (`int`), not the `?` the generic-arg unification prepass forces. Regression
    // guard for the prepass first-hit-wins latch (the record is gated on `!generic_arg_prepass`).
    let src =
        "fn apply[T](x: T, g: fn(T) -> T) -> T:\n    return g(x)\nprint(apply(1, fn(a): a + 1))\n";
    // line 2: `print(apply(1, fn(a): a + 1))` — the closure param `a` is at char 18.
    let h = hov(src, 2, 18).expect("hover on unannotated closure param decl");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Param);
}

#[test]
fn hover_struct_field_decl() {
    // Hovering the field `x` at its DECL site reports its declared type.
    let h = hov("struct P:\n    x: int\np := P(1)\n", 1, 4).expect("hover on struct field decl");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Field);
}

#[test]
fn hover_container_ctor_turbofish_callee() {
    // `List[int]()` callee is a bare Ident reaching builtin_sig: the empty ctor takes no argument.
    let h = hov("a := List[int]()\n", 0, 5).expect("hover on List[int] ctor callee");
    assert_eq!(h.display, "fn() -> List[?]");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_container_ctor_bare_callee() {
    // CONFIRMING (already green): bare `List()` ctor callee records the same display sig.
    // (The annotation types the empty list on its own statement — an error would make
    // `hover_type` short-circuit to None before the callee hit is returned.)
    let h = hov("b: List[int] = List()\n", 0, 15).expect("hover on bare List ctor callee");
    assert_eq!(h.display, "fn() -> List[?]");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_map_ctor_turbofish_callee() {
    // CONFIRMING (already green): `Map[str, int]()` callee records its display sig.
    let h = hov("m := Map[str, int]()\n", 0, 5).expect("hover on Map ctor callee");
    assert_eq!(h.display, "fn() -> Map[?, ?]");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

// ===== builtin / stdlib doc-on-hover (Tier C) =====

#[test]
fn hover_builtin_type_list_shows_methods() {
    // Hovering the `List` ctor callee surfaces its usage blurb + the "methods:" line.
    let h = hov("xs := List[int]()\n", 0, 6).expect("hover on List ctor callee");
    let doc = h
        .doc
        .as_deref()
        .expect("List ctor should carry a Tier-C doc");
    assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
    assert!(doc.contains("push"), "missing push method: {doc:?}");
}

#[test]
fn hover_builtin_type_token_str_shows_doc() {
    // Hovering the `str` type token in an annotation surfaces its usage blurb + methods.
    let h = hov("s: str = \"x\"\n", 0, 3).expect("hover on str type token");
    let doc = h
        .doc
        .as_deref()
        .expect("str token should carry a Tier-C doc");
    assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
    assert!(doc.contains("upper"), "missing upper method: {doc:?}");
}

#[test]
fn hover_module_fn_sqrt_shows_doc() {
    // Hovering `math.sqrt` surfaces the authored stdlib-fn blurb.
    let h = hov("import std.math\nr := math.sqrt(2.0)\n", 1, 10).expect("hover on math.sqrt");
    let doc = h
        .doc
        .as_deref()
        .expect("math.sqrt should carry a Tier-C doc");
    assert!(
        doc.to_lowercase().contains("square root"),
        "expected 'square root' blurb, got: {doc:?}"
    );
}

#[test]
fn hover_builtin_does_not_break_user_doc() {
    // The `.or_else(builtin_type_doc)` fallback must not shadow a user fn's own docstring:
    // a documented user fn still surfaces ITS doc at the call-callee hover (Tier A intact).
    let src = "# doubles n\nfn dbl(n: int) -> int:\n    return n * 2\ndbl(3)\n";
    let h = hov(src, 3, 0).expect("hover on user fn callee");
    assert_eq!(h.doc.as_deref(), Some("doubles n"));
}

#[test]
fn hover_generic_annotation_head_shows_doc() {
    // Hovering the GENERIC head `List` in `xs: List[int]` (goes through Type::Generic) surfaces
    // the builtin usage+methods blurb (Tier-C), same as the non-generic `str` token does.
    let h = hov("xs: List[int] = []\n", 0, 4).expect("hover on List generic head");
    let doc = h
        .doc
        .as_deref()
        .expect("List head should carry a Tier-C doc");
    assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
    assert!(doc.contains("push"), "missing push method: {doc:?}");
}

#[test]
fn hover_imported_type_shows_doc() {
    // Hovering the imported user type `Heap` on the import line surfaces a doc (its own decl
    // docstring carried across the module boundary, else a kind+module fallback).
    let h = hov("import Heap from std.collections\n", 0, 7).expect("hover on imported Heap");
    assert!(
        h.doc.is_some(),
        "imported type should carry a doc, got: {:?}",
        h.doc
    );
}

#[test]
fn hover_imported_generic_head_shows_doc() {
    // Case (a)+(b) together: a later GENERIC use of the imported type (`Heap[int]` head) surfaces
    // the imported doc carried into name_docs by the import-line binding.
    let src = "import Heap from std.collections\nfn f(h: Heap[int]):\n    print(h.len())\n";
    let h = hov(src, 1, 8).expect("hover on Heap generic head in param annotation");
    assert!(
        h.doc.is_some(),
        "imported generic head should carry a doc, got: {:?}",
        h.doc
    );
}

// ===== decl-site NAME-token hovers (Tier A) =====

#[test]
fn hover_struct_decl_name() {
    // Hovering the declared name `P` at `struct P:` reports the struct type.
    let h = hov("struct P:\n    x: int\n", 0, 7).expect("hover on struct decl name");
    assert_eq!(h.display, "P");
    assert_eq!(h.kind, crate::checker::HoverKind::Struct);
}

#[test]
fn hover_struct_decl_name_shows_doc() {
    // A `#` doc immediately above the struct surfaces on its decl-name hover.
    let h = hov("# a point\nstruct P:\n    x: int\n", 1, 7)
        .expect("hover on documented struct decl name");
    assert_eq!(h.display, "P");
    assert_eq!(h.doc.as_deref(), Some("a point"));
}

#[test]
fn hover_enum_decl_name() {
    let h = hov("enum Col:\n    Val(int)\n", 0, 5).expect("hover on enum decl name");
    assert_eq!(h.display, "Col");
    assert_eq!(h.kind, crate::checker::HoverKind::Struct);
}

#[test]
fn hover_type_alias_decl_name() {
    // The alias name reports the aliased type it stands for.
    let h = hov("type Id = int\n", 0, 5).expect("hover on type alias decl name");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Struct);
}

#[test]
fn hover_protocol_decl_name() {
    let h = hov("protocol Bar:\n    fn f(self)\n", 0, 9).expect("hover on protocol decl name");
    assert_eq!(h.display, "Bar");
    assert_eq!(h.kind, crate::checker::HoverKind::Struct);
}

#[test]
fn hover_type_param_decl_fn() {
    // Hovering `T` at the fn DECLARATION `fn id[T](...)` reports the param name.
    let h = hov("fn id[T](x: T) -> T:\n    return x\n", 0, 6).expect("hover on fn type-param decl");
    assert_eq!(h.display, "T");
}

#[test]
fn hover_type_param_decl_struct() {
    // The same enter_type_params record covers a struct's `[T]`.
    let h = hov("struct Box[T]:\n    x: T\n", 0, 11).expect("hover on struct type-param decl");
    assert_eq!(h.display, "T");
}

#[test]
fn hover_assign_lhs() {
    // Hovering the LHS `i` of a reassignment reports its type.
    let h = hov("i := 0\ni = i + 1\n", 1, 0).expect("hover on assign lhs");
    assert_eq!(h.display, "int");
    assert_eq!(h.kind, crate::checker::HoverKind::Local);
}

#[test]
fn hover_method_decl_name() {
    // Hovering the method name at its definition reports the call signature (receiver stripped),
    // matching the call-site method hover.
    let src = "struct C:\n    n: int\n    fn dbl(self) -> int:\n        return self.n * 2\n";
    let h = hov(src, 2, 7).expect("hover on method decl name");
    assert_eq!(h.display, "fn() -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_free_fn_decl_name() {
    // Hovering the free-function name at its definition reports the function signature (no `self`
    // to strip for a free fn), matching the call-site hover. `foo` token starts at col 3.
    let h = hov("fn foo(bar: int) -> int:\n    return bar\n", 0, 3)
        .expect("hover on free fn decl name");
    assert_eq!(h.display, "fn(int) -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_free_fn_decl_name_shows_doc() {
    // The fn-name token surfaces the decl's doc-comment (unlike a param, which has none).
    let h = hov(
        "# doubles it\nfn foo(bar: int) -> int:\n    return bar\n",
        1,
        3,
    )
    .expect("hover on free fn decl name with doc");
    assert_eq!(h.display, "fn(int) -> int");
    assert_eq!(h.doc.as_deref(), Some("doubles it"));
}

#[test]
fn hover_generic_free_fn_decl_name() {
    // LATCH guard: the generic-arg prepass must NOT latch a `?`/Unknown at the fn name; the
    // generic sig Displays its type params. `pick` token starts at col 3.
    let h = hov("fn pick[T](a: T, b: T) -> T:\n    return a\n", 0, 3)
        .expect("hover on generic free fn decl name");
    assert_eq!(h.display, "fn(T, T) -> T");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

#[test]
fn hover_import_module() {
    let h = hov("import std.math\n", 0, 11).expect("hover on import module name");
    assert_eq!(h.display, "module math");
}

#[test]
fn hover_import_module_alias() {
    let h = hov("import std.math as m\n", 0, 19).expect("hover on import alias");
    assert_eq!(h.display, "module m");
}

#[test]
fn hover_from_import_name() {
    let h = hov("import sqrt from std.math\n", 0, 7).expect("hover on from-import name");
    assert_eq!(h.display, "fn(float) -> float");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

/// The `token_type` of the semantic token starting at 0-based `(line, start)`, or `None`.
fn role_at(toks: &[SemTok], line: u32, start: u32) -> Option<u32> {
    toks.iter()
        .find(|t| t.line == line && t.start == start)
        .map(|t| t.token_type)
}

#[test]
fn overlay_roles() {
    // One buffer exercising every AST-derived role. Columns are 0-based UTF-16 (all ASCII here).
    let src = "fn f(p: int) -> int:\n    return p\nstruct S:\n    fld: int\nm := S(1)\na := m.fld\nb := f(2)\nc := 5\n";
    let toks = semantic_tokens(src);
    // fn-decl name → function; param decl → parameter; type annotations (param + return) → type.
    assert_eq!(role_at(&toks, 0, 3), Some(FUNCTION), "fn-decl name");
    assert_eq!(role_at(&toks, 0, 5), Some(PARAMETER), "param decl");
    assert_eq!(role_at(&toks, 0, 8), Some(TYPE), "param type annotation");
    assert_eq!(role_at(&toks, 0, 16), Some(TYPE), "return type annotation");
    // struct field decl → property; its type annotation → type.
    assert_eq!(role_at(&toks, 3, 4), Some(PROPERTY), "struct field decl");
    assert_eq!(role_at(&toks, 3, 9), Some(TYPE), "field type annotation");
    // ctor / fn-call callee → function; field access → property.
    assert_eq!(role_at(&toks, 4, 5), Some(FUNCTION), "ctor callee");
    assert_eq!(role_at(&toks, 5, 7), Some(PROPERTY), "field access");
    assert_eq!(role_at(&toks, 6, 5), Some(FUNCTION), "fn-call callee");
    // A plain local binding + a param USE stay variable (only DECL sites are roled).
    assert_eq!(role_at(&toks, 7, 0), Some(VARIABLE), "plain local");
    assert_eq!(role_at(&toks, 1, 11), Some(VARIABLE), "param use");
}

#[test]
fn overlay_graceful_on_parse_error() {
    // A mid-edit unparseable buffer: the overlay is empty and semantic_tokens degrades to
    // lexer-only (every ident VARIABLE) WITHOUT panicking and still emits tokens.
    let toks = semantic_tokens("fn f( := = bad\n");
    assert!(
        !toks.is_empty(),
        "tokens must still be emitted on a parse error"
    );
    for t in &toks {
        assert_ne!(
            t.token_type, FUNCTION,
            "no overlay role should leak from an unparseable buffer"
        );
    }
    // The identifiers `f` and `bad` are present and classified VARIABLE (no role override).
    assert_eq!(
        role_at(&toks, 0, 3),
        Some(VARIABLE),
        "ident f stays variable"
    );
}

/// Build the measured worst-parser-accepted-depth source: `R(0) = "a"`, `R(k) = "f(g(" +
/// R(k-1) + ")" + ".f".repeat(498) + ")"`. `lv = 15` is the deepest level the parser still
/// accepts; `lv = 16` trips the parser's own "expression nested too deeply" — this is a
/// MEASURED value, not re-derived here.
fn deep_but_legal_source(lv: usize) -> String {
    fn r(k: usize) -> String {
        if k == 0 {
            "a".to_string()
        } else {
            format!("f(g({}){})", r(k - 1), ".f".repeat(498))
        }
    }
    format!("x := {}\n", r(lv))
}

/// Regression for W7-50: `semantic_tokens` used to run its post-parse AST walk (`overlay_expr`,
/// recursing once per node) on the CALLER's stack. `chezzi-lsp`'s `textDocument/semanticTokens/full`
/// calls it from a `#[tokio::main]` worker with the default ~2 MiB stack — far smaller than the
/// 8 MiB CLI main thread — so a deep-but-PARSER-ACCEPTED buffer (a left-leaning chain the parser's
/// own recursive `MAX_DEPTH` guard never sees, only its `MAX_AST_DEPTH` fold-depth bound) would
/// SIGABRT the language server. `semantic_tokens` now hops onto the dedicated 1 GiB front-end
/// stack (`crate::on_frontend_stack`), same as `diagnostics`/`hover`.
///
/// On regression this test ABORTS the whole test-process run rather than failing a single
/// assertion — the same honesty `parser::tests::deep_nesting_errors_not_crash`'s doc-comment
/// gives its own stack-overflow class: a red run here looks like a killed `cargo test` process,
/// not a reported failure line.
#[test]
fn deep_but_legal_input_does_not_crash_semantic_tokens() {
    use crate::{lexer, parser};
    let src = deep_but_legal_source(15);
    // Fails loudly (not silently going shallow) if `MAX_DEPTH`/`MAX_AST_DEPTH` ever move.
    assert!(
        parser::parse(lexer::tokenize(&src).unwrap()).is_ok(),
        "fixture must still be parser-accepted at lv=15"
    );
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024) // matches the tokio default the LSP worker actually gets
        .spawn(move || crate::editor::semantic_tokens(&src))
        .expect("spawn 2 MiB probe thread");
    let toks = handle
        .join()
        .expect("semantic_tokens must not crash the 2 MiB worker");
    assert!(!toks.is_empty());
}

fn st(line: u32, start: u32, len: u32, ty: u32) -> SemTok {
    SemTok {
        line,
        start,
        len,
        token_type: ty,
    }
}

#[test]
fn semtok_fn_keyword() {
    assert_eq!(semantic_tokens("fn"), vec![st(0, 0, 2, KEYWORD)]);
}

#[test]
fn semtok_number_string_ident_op() {
    assert_eq!(semantic_tokens("123"), vec![st(0, 0, 3, NUMBER)]);
    // quotes are part of the highlighted string → `"hi"` is 4 chars.
    assert_eq!(semantic_tokens("\"hi\""), vec![st(0, 0, 4, STRING)]);
    assert_eq!(semantic_tokens("foo"), vec![st(0, 0, 3, VARIABLE)]);
    // `a+b` → variable, operator, variable, all length 1.
    assert_eq!(
        semantic_tokens("a+b"),
        vec![
            st(0, 0, 1, VARIABLE),
            st(0, 1, 1, OPERATOR),
            st(0, 2, 1, VARIABLE)
        ]
    );
}

#[test]
fn semtok_i64_min_magnitude_is_number() {
    // `-9223372036854775808`: the minus is an OPERATOR, the 2^63 magnitude (an IntMinMagnitude
    // token) still highlights as a NUMBER (19 digit chars), same as any int literal.
    assert_eq!(
        semantic_tokens("-9223372036854775808"),
        vec![st(0, 0, 1, OPERATOR), st(0, 1, 19, NUMBER)]
    );
}

#[test]
fn semtok_float_and_raw_and_byte_string() {
    assert_eq!(semantic_tokens("3.14"), vec![st(0, 0, 4, NUMBER)]);
    // r"ab" → 4 chars (r + quotes + ab), classified STRING.
    assert_eq!(semantic_tokens("r\"ab\""), vec![st(0, 0, 5, STRING)]);
    // b"x" → 4 chars.
    assert_eq!(semantic_tokens("b\"x\""), vec![st(0, 0, 4, STRING)]);
}

#[test]
fn semtok_comment_and_hash_in_string() {
    // `x # c` → VARIABLE over x, COMMENT over `# c` (3 chars).
    assert_eq!(
        semantic_tokens("x # c"),
        vec![st(0, 0, 1, VARIABLE), st(0, 2, 3, COMMENT)]
    );
    // A `#` inside a string is part of the STRING, never a comment.
    assert_eq!(semantic_tokens("\"#\""), vec![st(0, 0, 3, STRING)]);
}

#[test]
fn semtok_delta_encode() {
    // "fn\nx" → keyword then variable on the next line.
    let toks = semantic_tokens("fn\nx");
    assert_eq!(toks, vec![st(0, 0, 2, KEYWORD), st(1, 0, 1, VARIABLE)]);
    // delta-encoded: [dLine, dStart, len, type, modifiers] per token.
    assert_eq!(
        encode_semantic_tokens(&toks),
        vec![0, 0, 2, KEYWORD, 0, 1, 0, 1, VARIABLE, 0]
    );
}

#[test]
fn semtok_multiline_string_splits_per_line() {
    // A triple-quoted string spanning lines must NOT emit one token whose length runs past
    // end-of-line (LSP tokens are single-line). It splits into one STRING token per line.
    let toks = semantic_tokens("\"\"\"\nab\n\"\"\"\n");
    let strs: Vec<SemTok> = toks
        .into_iter()
        .filter(|t| t.token_type == STRING)
        .collect();
    assert_eq!(
        strs,
        vec![
            st(0, 0, 3, STRING), // opening """
            st(1, 0, 2, STRING), // ab
            st(2, 0, 3, STRING), // closing """
        ]
    );
}

#[test]
fn semtok_utf16_astral_columns() {
    // 🙂 is one Unicode scalar but TWO UTF-16 code units. LSP positions default to UTF-16, so
    // the `+` and `x` after the string must land at UTF-16 columns 4 and 5, not char columns 3/4.
    let toks = semantic_tokens("\"🙂\"+x");
    assert_eq!(
        toks,
        vec![
            st(0, 0, 4, STRING),   // "🙂" = 1 + 2 + 1 UTF-16 units
            st(0, 4, 1, OPERATOR), // +
            st(0, 5, 1, VARIABLE), // x
        ]
    );
}

#[test]
fn diag_utf16_astral_column() {
    // An emoji before the error token shifts every later column by one UTF-16 unit.
    let ds = diag("a := \"🙂\" + zzz\n");
    assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
    let d = &ds[0];
    assert!(d.message.contains("zzz"));
    // `zzz` starts at char col 11 (0-based); UTF-16 col is 12 (🙂 adds one extra unit).
    assert_eq!(d.col, 12);
    assert_eq!(d.end_col, 15); // 12 + 3 chars of "zzz"
}

/// W7-21 — a CALL through a module global that holds a fn value (`l.BARE()`) hovers the member
/// name with its function type, exactly like a call on a declared module `fn`. The value form
/// (`x := l.BARE`) always hovered; the call form did not exist until the member became callable,
/// so it would have shipped as a silent hole. Needs real files on disk: `hover` resolves the
/// module graph from the entry PATH.
#[test]
fn hover_module_fn_value_member_call() {
    let dir = std::env::temp_dir().join(format!("chezzi_hov_w721_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "fn one() -> int:\n    return 1\n").unwrap();
    std::fs::write(dir.join("l.chz"), "import k\nBARE := k.one\n").unwrap();
    let src = "import l\nprint(l.BARE())\n";
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    // Line 1 (0-based), the `BARE` token starts at char col 8: `print(l.BARE())`.
    let h = hover(&entry, src, 1, 8);
    let _ = std::fs::remove_dir_all(&dir);
    let h = h.expect("hover on a module fn-value member call");
    assert_eq!(h.display, "fn() -> int");
    assert_eq!(h.kind, crate::checker::HoverKind::Func);
}

/// A5 — a type error inside an IMPORTED module carries its own path (not the entry's), and its
/// range is computed against the imported module's own text. The entry's `import core.badmod` line
/// (19 chars) and badmod's own line 1 (16 chars) deliberately differ — and, at the error's column,
/// badmod's char OPENS a six-char string literal (`"oops"`, so `word_end_col` runs to col 15)
/// while the entry's char at that same column is the `r` of `core` (so it would stop at 11) — so
/// computing the range against the WRONG source is detectable rather than coincidentally correct.
#[test]
fn cross_module_diagnostic_carries_its_own_file_and_range() {
    let dir = std::env::temp_dir().join(format!("chezzi_editor_a5_cross_{}", std::process::id()));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
    let expected_file = std::fs::canonicalize(&badmod_path).unwrap();
    let entry = dir.join("app.chz");
    let src = "import core.badmod\n";
    let ds = diagnostics(&entry, src);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(ds.len(), 1, "expected exactly one diagnostic: {ds:?}");
    let d = &ds[0];
    assert_eq!(
        d.file.as_deref(),
        Some(expected_file.as_path()),
        "diagnostic must be attributed to the imported module, not the entry"
    );
    assert!(d.message.contains("core.badmod"), "{}", d.message);
    assert_eq!(d.line, 0);
    assert_eq!(d.col, 9);
    assert_eq!(
        d.end_col, 15,
        "range must come from badmod's OWN text (a six-char string literal) — computed against \
             the entry's text at the same column it would wrongly stop at the end of 'core' (11)"
    );
}

/// Negative control for the test above: a diagnostic that genuinely originates in the entry module
/// itself must NOT claim a module path — `Diag::file` stays `None`.
#[test]
fn entry_module_diagnostic_has_no_file() {
    let ds = diag("a := 1\nb := zzz\n");
    assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
    assert_eq!(
        ds[0].file, None,
        "an entry-module diagnostic must not claim a module path"
    );
}

/// A5 — a lex/parse (resolve) error INSIDE an imported module is also attributed to that module,
/// not the entry buffer: the `Err(e)` arm of `diagnostics_inner` gets the same treatment as the
/// `Ok(graph)` arm above.
#[test]
fn resolve_error_in_an_imported_module_is_attributed_to_it() {
    let dir = std::env::temp_dir().join(format!(
        "chezzi_editor_a5_resolve_err_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("core")).unwrap();
    let badmod_path = dir.join("core").join("badmod.chz");
    // Syntactically broken — a parse error, not a type error.
    std::fs::write(&badmod_path, "x := = 5\n").unwrap();
    let expected_file = std::fs::canonicalize(&badmod_path).unwrap();
    let entry = dir.join("app.chz");
    let src = "import core.badmod\n";
    let ds = diagnostics(&entry, src);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(ds.len(), 1, "expected exactly one diagnostic: {ds:?}");
    let d = &ds[0];
    assert_eq!(
        d.file.as_deref(),
        Some(expected_file.as_path()),
        "a parse error in an imported module must be attributed to it, not the entry"
    );
    assert_eq!(d.severity, Severity::Error);
    // Same fixture as `diag_parse_error_pos`: col 6 (1-based) → 5 (0-based), computed off badmod's
    // OWN line 1 ("x := = 5"), not the entry's ("import core.badmod").
    assert_eq!(d.line, 0);
    assert_eq!(d.col, 5);
}

/// F1 (Critical regression) — the `Err(e)` arm decided entry-vs-imported with RAW path equality
/// (`p.as_path() != path`). `e.path` for the entry's OWN parse failure is `id.0 =
/// canonical_or_abs(entry_abs)`, which resolves symlinks whenever the file exists; the caller's
/// `path` (from `uri.to_file_path()`) never does. So on a project reached through a symlinked
/// component the two differ even though it IS the entry, the guard wrongly takes the
/// imported-module arm, and the entry's own error is (a) computed against a stale on-disk read
/// instead of the live buffer, and (b) tagged `file: Some(canonical_path)`, which
/// `chezzi-lsp::publish` maps to a DIFFERENT URI than the one the editor has open — the diagnostic
/// is published where nothing listens and silently vanishes.
///
/// Repro: a real symlinked directory, an entry file that EXISTS on disk with stale content, and a
/// live (unsaved) buffer that differs from that stale content but has a parse error at the same
/// structural position. `expected` is computed by running the SAME live source through an ordinary
/// (non-symlinked, non-existent-on-disk) path — that's what a correct implementation must also
/// produce for the symlinked entry: `file: None`, and a range computed from the LIVE buffer, not
/// the stale disk text (the disk text is deliberately built so a wrong-source read is detectable —
/// its char at the error column starts an 8-char word, so a buggy end_col overruns to 14 instead of
/// the live buffer's 7).
#[cfg(unix)]
#[test]
fn entry_own_error_survives_a_symlinked_project_path() {
    use std::os::unix::fs::symlink;

    let base =
        std::env::temp_dir().join(format!("chezzi_editor_a5_symlink_{}", std::process::id()));
    let real_dir = base.join("real");
    let link_dir = base.join("link");
    std::fs::create_dir_all(&real_dir).unwrap();

    // Stale on-disk content: deliberately NOT what the live buffer holds, and its char at the
    // error column (index 6) starts a long word — so misreading it produces a clearly wrong,
    // over-long end_col instead of a merely-off-by-one one.
    let stale_disk_src = "012345abcdefgh\n";
    let entry_real = real_dir.join("app.chz");
    std::fs::write(&entry_real, stale_disk_src).unwrap();

    symlink(&real_dir, &link_dir).unwrap();
    let entry_via_symlink = link_dir.join("app.chz");

    // Live (unsaved) buffer: same broken-`:=` shape as `diag_parse_error_pos`'s fixture
    // ("x := = 5"), with the identifier lengthened by one char so its error column (7, 1-based)
    // lands on the stale disk text's word-starting index.
    let live_src = "xx := = 5\n";

    // Control: the SAME live source at an ordinary path with nothing on disk — ground truth for
    // what the symlinked entry's diagnostic must also look like.
    let expected = diagnostics(
        Path::new("/nonexistent/chezzi_editor/control_app.chz"),
        live_src,
    );
    let actual = diagnostics(&entry_via_symlink, live_src);
    let _ = std::fs::remove_dir_all(&base);

    assert_eq!(
        expected.len(),
        1,
        "control fixture should produce exactly one diagnostic: {expected:?}"
    );
    assert_eq!(
        actual.len(),
        1,
        "expected exactly one diagnostic: {actual:?}"
    );
    let (e, a) = (&expected[0], &actual[0]);

    assert_eq!(
        a.file, None,
        "the entry's OWN parse error must be attributed to the live buffer (file: None), not \
             misrouted to the imported-module arm because canonicalize() resolved the symlink; got {a:?}"
    );
    assert_eq!(a.line, e.line);
    assert_eq!(
        a.col, e.col,
        "start column must be computed from the LIVE buffer, not the stale on-disk copy"
    );
    assert_eq!(
        a.end_col, e.end_col,
        "range must be computed from the LIVE buffer, not the stale on-disk copy"
    );
    assert_eq!(a.message, e.message);
}

/// TICKET-228 (D6) — the prelude still spells the long names in its code; every reader of it
/// must keep parsing it, this one from source text alone.
#[test]
fn prelude_source_gets_a_semantic_overlay() {
    let src = crate::native::std_embed::lookup("prelude.chz").unwrap();
    assert!(!semantic_overlay(src).is_empty());
}

/// TICKET-228 (D6) — hover and diagnostics print a carrier as the user writes it.
#[test]
fn hover_and_diagnostics_name_no_removed_spelling() {
    fn clean(text: &str) {
        let hit = text
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|w| ["Option", "Result", "Some", "Ok", "Err"].contains(w));
        assert_eq!(hit, None, "{text}");
    }
    let cells = [
        ("xs := [1]\ny := xs.pop()\n", 1, 8, "fn() -> int?"),
        ("xs := [1]\ny := xs.pop()\n", 1, 0, "int?"),
        (
            "fn f() -> int!str:\n    return 1\nz := f()\n",
            2,
            5,
            "fn() -> int!str",
        ),
        ("p := \"1\".to_int()\n", 0, 9, "fn() -> int!"),
    ];
    for (src, line, ch, want) in cells {
        let h = hov(src, line, ch).unwrap_or_else(|| panic!("no hover: {src}"));
        assert_eq!(h.display, want, "{src}");
        clean(&h.display);
        clean(h.doc.as_deref().unwrap_or(""));
    }
    let ds = diag("x: option = 1\n");
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert!(
        ds[0].message.contains("unknown type 'option'"),
        "{}",
        ds[0].message
    );
    clean(&ds[0].message);
}

/// TICKET-271 -- a CRLF file highlights like the same file with LF line breaks, line by line: a
/// multi-line string's extent, a `#` on its last line and a trailing comment end where they end
/// in the LF form. The hover lookup (`token_at`) agrees at every cursor position.
#[test]
fn a_crlf_file_highlights_and_hovers_like_lf() {
    let cells = [
        ("plain string", "s := \"a\nb\nc\n#\"\nx := 1\n"),
        ("triple string", "s := \"\"\"a\n\n\n\n#\"\"\"\nx := 1\n"),
        (
            "hash on the last line",
            "s := \"\"\"a\nb # c\"\"\"  # d\nx := 1  # e\n",
        ),
    ];
    let mut red = Vec::new();
    for (cell, lf) in cells {
        let crlf = lf.replace('\n', "\r\n");
        let (want, got) = (semantic_tokens(lf), semantic_tokens(&crlf));
        for (line0, text) in lf.lines().enumerate() {
            let on = |toks: &[SemTok]| -> Vec<SemTok> {
                toks.iter()
                    .filter(|t| t.line as usize == line0)
                    .cloned()
                    .collect()
            };
            if on(&got) != on(&want) {
                red.push(format!("{cell}: highlight line {line0}: {:?}", on(&got)));
            }
            for col in 0..=text.chars().count() {
                if token_at(&crlf, line0, col) != token_at(lf, line0, col) {
                    red.push(format!("{cell}: token_at line {line0} col {col}"));
                }
            }
        }
        if got.len() != want.len() {
            red.push(format!(
                "{cell}: {} highlights, LF has {}",
                got.len(),
                want.len()
            ));
        }
    }
    assert!(red.is_empty(), "CRLF highlights unlike LF in: {red:#?}");
}

/// TICKET-281 — the hover doc of an annotation head comes from the arm the resolver took: a user
/// `struct str` shows its own docstring, not the builtin `str` blurb.
#[test]
fn hover_user_struct_str_annotation_shows_its_own_doc() {
    let src = "# my text\nstruct str:\n    x: int\nfn f(s: str):\n    pass\n";
    let h = hov(src, 3, 8).expect("hover on user str annotation");
    assert_eq!(h.doc.as_deref(), Some("my text"));
}

/// TICKET-281 — same for a generic head: a user `struct List[T]` shows its own docstring.
#[test]
fn hover_user_generic_list_head_shows_its_own_doc() {
    let src = "# my list\nstruct List[T]:\n    v: T\nfn f(xs: List[int]):\n    pass\n";
    let h = hov(src, 3, 9).expect("hover on user List head");
    assert_eq!(h.doc.as_deref(), Some("my list"));
}
