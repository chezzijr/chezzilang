//! M4.5 cross-module type-checking tests (`check_graph` over real tempdir fixtures).
use super::*;
use std::path::Path;

use crate::test_support::TmpDir;

fn check_entry(entry: &Path) -> Result<(), Vec<CheckError>> {
    let graph = crate::resolver::build_graph(entry).expect("graph should build");
    check_graph(&graph)
}

fn errors(entry: &Path) -> Vec<String> {
    check_entry(entry)
        .unwrap_err()
        .into_iter()
        .map(|e| e.message)
        .collect()
}

// 18. A module member's type resolves: a correct use checks clean, a mismatch is rejected.
#[test]
fn module_member_type_resolves() {
    let t = TmpDir::new();
    t.write("a.chz", "fn read() -> int: return 5\n");
    let ok = t.write(
        "ok.chz",
        "import a\nx: int = a.read()\nfn main(): print(x)\n",
    );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );

    let bad = t.write(
        "bad.chz",
        "import a\nx: str = a.read()\nfn main(): print(x)\n",
    );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("cannot assign int to variable of type str")),
        "got: {errs:?}"
    );
}

// 19. A type error inside an imported module is reported (with a module label), and the entry's
// own errors are still collected in the same run.
#[test]
fn type_error_across_module_boundary() {
    let t = TmpDir::new();
    // b returns str from an int-typed fn — a type error inside b.
    t.write("b.chz", "fn helper() -> int: return \"nope\"\n");
    let entry = t.write(
        "main.chz",
        // entry has its own error too: assigning int to a str var.
        "import helper from b\nbad: str = 5\nfn main(): print(helper())\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter().any(|m| m.contains("in module 'b'")),
        "imported module error not labeled: {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|m| m.contains("cannot assign int to variable of type str")),
        "entry error not collected: {errs:?}"
    );
}

// 20. A `from` import of a name the module does not export is rejected at check time.
#[test]
fn unknown_imported_member_rejected() {
    let t = TmpDir::new();
    t.write("a.chz", "fn f(): print(\"f\")\n");
    let entry = t.write("main.chz", "import nope from a\nfn main(): print(1)\n");
    let errs = errors(&entry);
    assert!(
        errs.iter().any(|m| m.contains("has no member 'nope'")),
        "got: {errs:?}"
    );
}

// 21. Types are MODULE-SCOPED: the same struct name in two loaded modules does NOT collide
// (each is private to its module, reachable only via import).
#[test]
fn cross_module_same_type_name_no_collision() {
    let t = TmpDir::new();
    t.write("a.chz", "struct Point:\n    x: int\nfn fa(): print(1)\n");
    t.write("b.chz", "struct Point:\n    y: int\nfn fb(): print(2)\n");
    let entry = t.write("main.chz", "import a\nimport b\nfn main(): print(1)\n");
    assert!(
        check_entry(&entry).is_ok(),
        "two modules with the same type name must NOT collide: {:?}",
        errors(&entry)
    );
}

// Module-scoped types: `import S from m` (struct) makes the struct usable bare.
#[test]
fn from_import_struct_usable() {
    let t = TmpDir::new();
    t.write("types.chz", "struct S:\n    x: int\n");
    let entry = t.write(
        "main.chz",
        "import S from types\ns := S(1)\nfn main(): print(s.x)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "from-imported struct should be usable: {:?}",
        errors(&entry)
    );
}

// `import Color from m` (enum) makes the enum + its variants usable.
#[test]
fn from_import_enum_usable() {
    let t = TmpDir::new();
    t.write("types.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
        "main.chz",
        "import Color from types\nc := Color.Red\nfn main(): print(\"ok\")\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "from-imported enum should be usable: {:?}",
        errors(&entry)
    );
}

// An imported enum's METHODS must ferry across the module boundary (EnumSigInfo.methods →
// importer's enum_methods). Both the from-import and qualified-access paths.
#[test]
fn imported_enum_methods_usable() {
    let t = TmpDir::new();
    t.write(
            "types.chz",
            "enum Color:\n    Red\n    Green\n    fn cost(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\n",
        );
    let entry = t.write(
            "main.chz",
            "import Color from types\nimport types\nfn main():\n    c := Color.Red\n    x: int = c.cost()\n    q: int = types.Color.Green.cost()\n    print(x + q)\nmain()\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "imported enum methods should resolve cross-module: {:?}",
        errors(&entry)
    );
}

// `import Alias from m` (type alias) makes the alias usable as a type.
#[test]
fn from_import_alias_usable() {
    let t = TmpDir::new();
    t.write("types.chz", "type Len = int\n");
    let entry = t.write(
        "main.chz",
        "import Len from types\nx: Len = 5\nfn main(): print(x)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "from-imported alias should be usable: {:?}",
        errors(&entry)
    );
}

// Qualified access: `import geo` then `geo.Point(1,2)` and `x: geo.Point`.
#[test]
fn qualified_struct_access() {
    let t = TmpDir::new();
    t.write("geo.chz", "struct Point:\n    x: int\n    y: int\n");
    let entry = t.write(
        "main.chz",
        "import geo\np: geo.Point = geo.Point(1, 2)\nfn main(): print(p.x)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "qualified struct access should check: {:?}",
        errors(&entry)
    );
}

// Qualified enum variant access: `geo.Color.Red`.
#[test]
fn qualified_enum_access() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
        "main.chz",
        "import geo\nc := geo.Color.Red\nfn main(): print(\"ok\")\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "qualified enum access should check: {:?}",
        errors(&entry)
    );
}

// Bare use of a type whose module was imported (whole-module) but NOT `from`-imported is a
// CHECK-TIME error with a hint to import it.
#[test]
fn bare_unimported_type_is_error() {
    let t = TmpDir::new();
    t.write("geo.chz", "struct Point:\n    x: int\n");
    let entry = t.write(
        "main.chz",
        "import geo\np := Point(1)\nfn main(): print(\"ok\")\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|m| m.contains("Point")
                && (m.contains("import it from geo") || m.contains("unknown"))),
        "expected unknown-type/import hint for bare Point: {errs:?}"
    );
}

// TICKET-262: a width type is legal only in a C signature or a struct field, so `type Len = int32`
// in module m is rejected at its declaration, and the extern use in the entry cannot launder it.
#[test]
fn cross_module_width_alias_transparent() {
    let t = TmpDir::new();
    t.write("m.chz", "import int32 from std.ffi\ntype Len = int32\n");
    let entry = t.write(
        "main.chz",
        "import Len from m\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Len\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|m| m.contains("describes the C boundary only")),
        "expected the rule-1 error for the alias body: {errs:?}"
    );
}

// 22. A1: `Channel[T].try_recv()` is typed `T?` (Option[T]). A correct `int?` return checks
// clean; a `str?` return is a type mismatch (pins the element-typed Option result).
#[test]
fn channel_try_recv_returns_option_of_elem() {
    let t = TmpDir::new();
    let ok = t.write(
            "ok.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );

    let bad = t.write(
            "bad.chz",
            "fn f() -> str?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter().any(|m| m.contains("str")),
        "expected Option type mismatch: {errs:?}"
    );
}

// 23. A1: `try_recv` takes no arguments — a call with one is rejected on arity.
#[test]
fn channel_try_recv_arity_rejected() {
    let t = TmpDir::new();
    let bad = t.write(
            "bad.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv(5)\nfn main(): print(1)\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("try_recv") && m.contains("argument")),
        "got: {errs:?}"
    );
}

// 24. Qualified enum-variant access `Color.Red` (nullary) type-checks as the enum type, exactly
// like the bare `Red`. A wrong variant `Color.Bogus` is a targeted error.
#[test]
fn qualified_nullary_variant_checks() {
    let t = TmpDir::new();
    let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Red\nfn main(): print(1)\n",
        );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );

    let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Bogus\nfn main(): print(1)\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("Color") && m.contains("Bogus")),
        "expected 'no variant Bogus' style error: {errs:?}"
    );
}

// 25. Qualified payload-variant construction `Shape.Circle(2)` type-checks as the enum type and
// still validates argument types, exactly like the bare `Circle(2)`.
#[test]
fn qualified_payload_variant_checks() {
    let t = TmpDir::new();
    let ok = t.write(
            "ok.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(2)\nfn main(): print(1)\n",
        );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );

    let bad = t.write(
            "bad.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(\"x\")\nfn main(): print(1)\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("Circle") && m.contains("int")),
        "expected payload type mismatch: {errs:?}"
    );
}

// 26. A real binding wins over an enum name: `c.0` where `c` is a tuple is normal field access,
// never reinterpreted as a qualified variant.
#[test]
fn binding_wins_over_enum_qualifier() {
    let t = TmpDir::new();
    let ok = t.write(
        "ok.chz",
        "enum Color:\n    Red\nfn main():\n    pair := (1, 2)\n    print(pair.0)\n",
    );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );
}

// 27. Qualified variant patterns `case Color.Red:` check clean (mixed with bare arms); a wrong
// qualifier `case Color.Circle:` (Circle belongs to a different enum) is a targeted error.
#[test]
fn qualified_variant_pattern_checks() {
    let t = TmpDir::new();
    let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f(c: Color) -> str:\n    return match c:\n        Color.Red: \"r\"\n        Color.Green: \"g\"\nfn main(): print(f(Color.Red))\n",
        );
    assert!(
        check_entry(&ok).is_ok(),
        "expected clean: {:?}",
        errors(&ok)
    );

    let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nenum Shape:\n    Circle(int)\nfn f(c: Color) -> str:\n    return match c:\n        Color.Circle: \"c\"\n        _: \"x\"\nfn main(): print(f(Color.Red))\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("Color") && m.contains("Circle")),
        "expected qualifier mismatch error: {errs:?}"
    );
}

// 28. A *module-global* binding beside an enum reads its own field: `color.Red` is the `Box`
// field, an `int`, and `Color.Red` stays the variant. A global named `Color` is a second binding.
#[test]
fn global_binding_beside_an_enum_reads_its_own_field() {
    let t = TmpDir::new();
    let good = t.write(
            "good.chz",
            "enum Color:\n    Red\n    Green\nstruct Box:\n    Red: int\ncolor := Box(7)\nfn show() -> int:\n    return color.Red\nfn main(): print(show())\n",
        );
    let res = check_entry(&good);
    assert!(res.is_ok(), "expected no type errors, got: {res:?}");
    let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nstruct Box:\n    Red: int\nColor := Box(7)\nfn main(): print(Color.Red)\n",
        );
    let errs = errors(&bad);
    assert!(
        errs.iter()
            .any(|m| m.contains("'Color' is already defined")),
        "expected the second binding of 'Color' to be rejected: {errs:?}"
    );
}

// 29. The `Enum.` qualifier is validated even under an int/str/bool scrutinee: `case Color.Bogus:`
// against an `int` must be rejected (a qualified variant is never a catch-all binding), not
// silently accepted as a binding named `Bogus`.
#[test]
fn qualified_pattern_validated_under_literal_scrutinee() {
    let t = TmpDir::new();
    let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f(n: int) -> str:\n    return match n:\n        1: \"one\"\n        Color.Bogus: \"x\"\nfn main(): print(f(1))\n",
        );
    let errs = errors(&bad);
    assert!(
        !errs.is_empty(),
        "expected a checker error for `Color.Bogus` against an int scrutinee, got none"
    );
}

// 30. Genuine struct collision (Blocker A): two modules declare `Point` and BOTH are imported and
// used qualified. The checker must resolve a field on the SECOND module's `Point` against ITS
// layout (module-scoped runtime key), not first-wins by bare name.
#[test]
fn collision_struct_field_resolves_via_qualified_key() {
    let t = TmpDir::new();
    t.write("a.chz", "struct Point:\n    x: int\n");
    t.write("b.chz", "struct Point:\n    y: int\n    z: int\n");
    let entry = t.write(
        "main.chz",
        "import a\nimport b\nfn main():\n    pb := b.Point(2, 3)\n    print(pb.y)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "field `y` on b.Point must resolve via b's layout: {:?}",
        errors(&entry)
    );
}

// 31. Genuine struct collision for a METHOD (Blocker B): only bb.Box has `dbl`. Resolving the
// method on a `bb.Box(..)` value must hit bb's layout, not ba's (which lacks the method).
#[test]
fn collision_struct_method_resolves_via_qualified_key() {
    let t = TmpDir::new();
    t.write("ba.chz", "struct Box:\n    v: int\n");
    t.write(
        "bb.chz",
        "struct Box:\n    v: int\n    fn dbl(self) -> int:\n        return self.v * 2\n",
    );
    let entry = t.write(
        "main.chz",
        "import ba\nimport bb\nfn main():\n    bb2 := bb.Box(5)\n    print(bb2.dbl())\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "method `dbl` on bb.Box must resolve via bb's layout: {:?}",
        errors(&entry)
    );
}

// 32. Self-consistency under enum collision (Blocker C, checker side): two modules declare `Color`,
// cb has `fn classify(c: Color)` matching on its own `Color.Red`/`Color.Green`. With both imported
// the checker must still type-check the bare-`Color` annotation + the qualified match patterns.
#[test]
fn collision_enum_annotation_and_match_typecheck() {
    let t = TmpDir::new();
    t.write("ca.chz", "enum Color:\n    Red\n    Green\n");
    t.write(
            "cb.chz",
            "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\n",
        );
    let entry = t.write(
        "cmain.chz",
        "import ca\nimport cb\nfn main(): print(cb.classify(cb.Color.Red))\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "cb.classify + qualified Color match must type-check under collision: {:?}",
        errors(&entry)
    );
}

// 33. The "foreign qualifier" match error must render the scrutinee enum's BARE display name,
// never the qualified identity key. Two modules declare `Color`; a `match` on one using the
// other's `Color.<variant>` arms must report `enum 'Color'`, not `enum 'modkey::Color'`.
#[test]
fn foreign_variant_match_error_renders_bare_enum_name() {
    let t = TmpDir::new();
    t.write("ea.chz", "enum Color:\n    Red\n    Green\n");
    t.write("eb.chz", "enum Color:\n    Cyan\n    Magenta\n");
    let entry = t.write(
            "emain.chz",
            "import Color from ea\nimport Color as C2 from eb\nfn main():\n    c := Color.Red\n    match c:\n        C2.Cyan: print(\"x\")\n        C2.Magenta: print(\"y\")\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("cannot match a value of type Color")),
        "expected bare 'Color' in foreign-qualifier match error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("::Color'")),
        "qualified identity key must not leak into match error: {errs:?}"
    );
}

#[test]
fn module_qualified_variant_pattern_type_checks_clean() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Red: print(\"r\")\n        geo.Color.Green: print(\"g\")\nmain()\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "module-qualified variant pattern should type-check clean: {:?}",
        check_entry(&entry)
    );
}

#[test]
fn module_qualified_variant_pattern_aliased_binder_clean() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
            "main.chz",
            "import geo as g\nfn main():\n    c := g.Color.Red\n    match c:\n        g.Color.Red: print(\"r\")\n        g.Color.Green: print(\"g\")\nmain()\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "aliased module-qualified variant pattern should type-check clean: {:?}",
        check_entry(&entry)
    );
}

#[test]
fn module_qualified_variant_pattern_payload_binds_clean() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Shape:\n    Circle(int)\n    Square(int)\n");
    let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    s := geo.Shape.Circle(3)\n    match s:\n        geo.Shape.Circle(r): print(r)\n        geo.Shape.Square(w): print(w)\nmain()\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "payload-binding module-qualified variant pattern should type-check clean: {:?}",
        check_entry(&entry)
    );
}

#[test]
fn module_qualified_variant_pattern_wrong_variant_bare_error() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Blue: print(\"b\")\n        _: print(\"x\")\nmain()\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("enum 'Color' has no variant 'Blue'")),
        "expected bare-name wrong-variant error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("::Color")),
        "qualified identity key must not leak: {errs:?}"
    );
}

#[test]
fn module_qualified_variant_pattern_wrong_enum_bare_error() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Shape.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("module 'geo' has no enum 'Shape'")),
        "expected bare module/enum error, got: {errs:?}"
    );
}

#[test]
fn module_qualified_variant_pattern_unknown_module_error() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        nope.Color.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter().any(|e| e.contains("unknown module 'nope'")),
        "expected unknown-module error, got: {errs:?}"
    );
}

#[test]
fn module_qualified_variant_pattern_cross_enum_bare_error() {
    let t = TmpDir::new();
    t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
    t.write("light.chz", "enum Light:\n    Red\n    Off\n");
    let entry = t.write(
            "main.chz",
            "import geo\nimport light\nfn main():\n    c := geo.Color.Red\n    match c:\n        light.Light.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("cannot match a value of type Color")),
        "expected bare cross-enum error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("::")),
        "qualified identity key must not leak: {errs:?}"
    );
}

#[test]
fn module_qualified_variant_pattern_collision_no_cross_talk() {
    let t = TmpDir::new();
    t.write("a.chz", "enum Color:\n    Red\n    Green\n");
    t.write("b.chz", "enum Color:\n    Cyan\n    Magenta\n");
    // Exhaustive over a's Color via a.Color.* arms => clean.
    let ok = t.write(
            "ok.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\n        a.Color.Green: print(\"g\")\nmain()\n",
        );
    assert!(
        check_entry(&ok).is_ok(),
        "a.Color.* arms over an a.Color value should be exhaustive: {:?}",
        check_entry(&ok)
    );
    // Using b's variant under a's enum => wrong-variant error (no cross-talk).
    let xtalk = t.write(
            "xtalk.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Cyan: print(\"c\")\n        _: print(\"x\")\nmain()\n",
        );
    let errs = errors(&xtalk);
    assert!(
        errs.iter()
            .any(|e| e.contains("enum 'Color' has no variant 'Cyan'")),
        "expected no cross-talk with b's Color, got: {errs:?}"
    );
    // Non-exhaustive over a's Color => missing Green (bare name).
    let nonex = t.write(
            "nonex.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\nmain()\n",
        );
    let errs = errors(&nonex);
    assert!(
        errs.iter().any(|e| e.contains("Green")),
        "expected non-exhaustive to report missing bare 'Green', got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.contains("::")),
        "exhaustiveness error must use bare names: {errs:?}"
    );
}

// Soundness: an unannotated closure param (`Ty::Unknown` scrutinee) whose match arms name a
// single known enum but miss a variant must STILL be rejected — the Skip path used to bypass
// exhaustiveness, letting `g(E.B)` reach the engine and runtime-trap "no match arm for variant".
#[test]
fn match_unknown_param_missing_variant_rejects() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\nfn main(): print(g(E.B))\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("non-exhaustive") && e.contains("missing B")),
        "expected non-exhaustive missing B, got: {errs:?}"
    );
}

// Soundness: literal arms over an `Ty::Unknown` scrutinee with no `_` wildcard cannot be proven
// complete — must be rejected (the int-scrutinee value-leak `g(99) -> 99` otherwise).
#[test]
fn match_unknown_param_literals_no_wildcard_rejects() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "g := fn(x): match x:\n    1: \"one\"\nfn main(): print(g(99))\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("non-exhaustive") && e.contains("`_`")),
        "expected non-exhaustive add a `_` arm, got: {errs:?}"
    );
}

// Boundary (must STAY accepted): unannotated param covering ALL variants of E, no `_`.
#[test]
fn match_unknown_param_all_variants_accepts() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean (full coverage): {:?}",
        errors(&entry)
    );
}

// Boundary (must STAY accepted): unannotated param over E variants PLUS a `_` wildcard.
#[test]
fn match_unknown_param_variants_plus_wildcard_accepts() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    _: \"other\"\nfn main(): print(g(E.B))\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean (wildcard closes): {:?}",
        errors(&entry)
    );
}

// Boundary (must STAY accepted): literal arms PLUS a `_` wildcard.
#[test]
fn match_unknown_param_literals_plus_wildcard_accepts() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "g := fn(x) -> str: match x:\n    1: \"one\"\n    _: \"other\"\nfn main(): print(g(99))\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean (wildcard closes literals): {:?}",
        errors(&entry)
    );
}

// MIGRATED (closure-param inference, phase 3): the first literal arm (`1`) now PINS the bare
// param `x` to `int` (source #2 — a bare-param match scrutinee). The `"b"` arm is then a str
// literal against an `int` scrutinee → rejected. The pre-inference `OpenScrutinee` behaviour
// (accept heterogeneous literals when a `_` closes the match) no longer applies: the scrutinee is
// no longer un-inferable.
#[test]
fn match_unknown_param_hetero_literals_plus_wildcard_rejects() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    1: \"a\"\n    \"b\": \"c\"\n    _: \"d\"\nfn main(): print(g(1))\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
        "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
    );
}

// MIGRATED (phase 3): the first arm (`1`) pins `x` to `int`; the `"b"` arm is then a literal
// mismatch (and the match is also non-exhaustive). Either way it rejects.
#[test]
fn match_unknown_param_hetero_literals_no_wildcard_rejects() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "g := fn(x): match x:\n    1: \"a\"\n    \"b\": \"c\"\nfn main(): print(g(true))\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
        "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
    );
}

// Boundary (must STAY accepted): a bare-ident catch-all (`n:`) over an un-inferable scrutinee is
// an irrefutable binding that closes the match — exactly like a concretely-typed int scrutinee.
// Regression guard: routing the literal case through the permissive `Skip` bind branch wrongly
// treated `n` as a refutable nullary variant → spurious non-exhaustive + undeclared binding.
#[test]
fn match_unknown_param_bare_ident_catchall_accepts() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    0: \"zero\"\n    n: \"got {n}\"\nfn main(): print(g(7))\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean (bare-ident catch-all closes + binds): {:?}",
        errors(&entry)
    );
}

// === Qualified-type-as-static-method-receiver (Part 1) ===

// `module.Struct.static_method()` — qualified type dotted with a static method checks clean.
#[test]
fn qualified_type_struct_static_ok() {
    let t = TmpDir::new();
    t.write(
        "counter.chz",
        "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
    );
    let entry = t.write(
        "main.chz",
        "import counter\nfn main():\n    c := counter.Counter.zero()\n    print(c.n)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean qualified static call: {:?}",
        errors(&entry)
    );
}

// Negative: a non-existent static method on a qualified type is a clear "no static method" error.
#[test]
fn qualified_type_struct_static_unknown_rejects() {
    let t = TmpDir::new();
    t.write(
        "counter.chz",
        "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
    );
    let entry = t.write(
        "main.chz",
        "import counter\nfn main():\n    c := counter.Counter.no_such()\n    print(c)\n",
    );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|m| m.contains("has no static method 'no_such'")),
        "got: {errs:?}"
    );
}

// `module.Enum.static_method()` — qualified enum dotted with a static method (NOT a variant).
#[test]
fn qualified_type_enum_static_ok() {
    let t = TmpDir::new();
    t.write(
        "col.chz",
        "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
    );
    let entry = t.write(
        "main.chz",
        "import col\nfn main():\n    c := col.Color.first()\n    print(c)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "expected clean qualified enum-static call: {:?}",
        errors(&entry)
    );
}

// Regression KEEP-WORKING: bare static after `from` import.
#[test]
fn qualified_keep_bare_static() {
    let t = TmpDir::new();
    t.write(
        "counter.chz",
        "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
    );
    let entry = t.write(
        "main.chz",
        "import Counter from counter\nfn main():\n    c := Counter.zero()\n    print(c.n)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "bare static regressed: {:?}",
        errors(&entry)
    );
}

// Regression KEEP-WORKING: qualified constructor `module.Type(args)`.
#[test]
fn qualified_keep_qualified_ctor() {
    let t = TmpDir::new();
    t.write("counter.chz", "struct Counter:\n    n: int\n");
    let entry = t.write(
        "main.chz",
        "import counter\nfn main():\n    c := counter.Counter(7)\n    print(c.n)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "qualified ctor regressed: {:?}",
        errors(&entry)
    );
}

// Regression KEEP-WORKING: qualified enum VARIANT (variant-first wins over static).
#[test]
fn qualified_keep_enum_variant() {
    let t = TmpDir::new();
    t.write(
        "col.chz",
        "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
    );
    let entry = t.write(
        "main.chz",
        "import col\nfn main():\n    c := col.Color.Red\n    print(c)\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "qualified enum variant regressed: {:?}",
        errors(&entry)
    );
}

// === Two-level path diagnostics (Part 2) ===

// EXPR position: `std.concurrency.Shared(0)` after an un-aliased `import std.concurrency` is the
// Python full path of that module (TICKET-175), so it checks clean.
#[test]
fn full_expr_path_of_an_imported_module_checks_clean() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.concurrency\nfn main():\n    s := std.concurrency.Shared(0)\n    print(s)\n",
    );
    let errs = check_entry(&entry).err().unwrap_or_default();
    assert!(errs.is_empty(), "expected no errors, got: {errs:?}");
}

// EXPR position, ALIASED sibling: `import std.net as n` binds only `n`, so a `std.net.X` use
// names the bound name `n`, never the un-aliased sibling `concurrency`.
#[test]
fn aliased_module_full_path_names_its_bound_name() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "import std.concurrency\nimport std.net as n\nfn main():\n    s := std.net.Socket(0)\n    print(s)\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|m| m.contains("`std.net` is imported as `n`")),
        "expected hint naming the bound name `n`, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|m| m.contains("concurrency")),
        "must NOT steer to the sibling `concurrency`: {errs:?}"
    );
}

// EXPR position, THREE-LEVEL import: `import std.concurrency.collection` binds the full path
// `std.concurrency.collection`, NOT its prefix `std.concurrency` (no implicit parents).
#[test]
fn full_path_prefix_of_a_deeper_import_is_not_imported() {
    let t = TmpDir::new();
    let entry = t.write(
            "main.chz",
            "import std.concurrency.collection\nfn main():\n    c := std.concurrency.Nope(0)\n    print(c)\n",
        );
    let errs = errors(&entry);
    assert!(
        errs.iter()
            .any(|m| m.contains("module `std.concurrency` is not imported")),
        "expected the not-imported hint, got: {errs:?}"
    );
}

// Negative: a genuine undefined name still gives the normal "unknown name" error.
#[test]
fn multilevel_expr_real_typo_still_unknown_name() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "fn main():\n    nope_xyz()\n");
    let errs = errors(&entry);
    assert!(
        errs.iter().any(|m| m.contains("unknown name 'nope_xyz'")),
        "got: {errs:?}"
    );
}

// Negative: a real undefined TYPE still gives the normal unknown-type error (not the hint).
#[test]
fn multilevel_unknown_type_still_normal_error() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "fn f(x: Nope): print(1)\nfn main(): f(1)\n");
    let errs = errors(&entry);
    assert!(
        errs.iter().any(|m| m.contains("Nope")),
        "expected unknown-type error mentioning Nope, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|m| m.contains("two-level")),
        "should not emit two-level hint for a real typo: {errs:?}"
    );
}

// ===== expected-type-hint inference: an ANNOTATION pins a generic ctor/fn-call's type params =====
// (a self-contained Heap clone — fields `data: List[T]`, `less: fn(T,T)->bool` — exercises the
// module-prefixed-key check_graph path, decoupled from the std.collections path.)
const HEAP_MOD: &str = "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\n\n    fn len(self) -> int:\n        return self.data.len()\n";

#[test]
fn annotation_pins_generic_ctor_let() {
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let entry = t.write(
        "main.chz",
        "import H from a\nfn main():\n    h: H[int] = H([], fn(x, y): x < y)\n    print(h.len())\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "annotation should pin H[int] T=int: {:?}",
        errors(&entry)
    );
}

#[test]
fn annotation_pins_generic_ctor_return() {
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let entry = t.write(
            "main.chz",
            "import H from a\nfn mk() -> H[int]:\n    return H([], fn(x, y): x < y)\nfn main():\n    print(mk().len())\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "declared return H[int] should pin T=int: {:?}",
        errors(&entry)
    );
}

#[test]
fn annotation_pins_generic_ctor_call_arg() {
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let entry = t.write(
            "main.chz",
            "import H from a\nfn take(h: H[int]): print(h.len())\nfn main():\n    take(H([], fn(x, y): x < y))\n",
        );
    assert!(
        check_entry(&entry).is_ok(),
        "param type H[int] should pin T=int: {:?}",
        errors(&entry)
    );
}

#[test]
fn annotation_pins_qualified_generic_ctor() {
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let entry = t.write(
        "main.chz",
        "import a\nfn main():\n    h: a.H[int] = a.H([], fn(x, y): x < y)\n    print(h.len())\n",
    );
    assert!(
        check_entry(&entry).is_ok(),
        "qualified ctor annotation should pin T=int: {:?}",
        errors(&entry)
    );
}

#[test]
fn annotation_pins_generic_free_fn_return() {
    let t = TmpDir::new();
    t.write("a.chz", "fn empty[T]() -> List[T]:\n    return []\n");
    let entry = t.write(
        "main.chz",
        "import empty from a\nfn main():\n    xs: List[int] = empty()\n    print(xs.len())\n",
    );
    let errs = errors_or_empty(&entry);
    assert!(
        errs.is_empty(),
        "annotation should pin empty()'s T=int: {errs:?}"
    );
    assert!(
        !errs.iter().any(|m| m.contains("cannot assign List[T] to")),
        "stale leaked-Param error: {errs:?}"
    );
}

#[test]
fn annotation_does_not_override_explicit_or_args() {
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    // (a) turbofish, no annotation — still works.
    let tf = t.write(
            "tf.chz",
            "import H from a\nfn main():\n    h := H[int]([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
    assert!(check_entry(&tf).is_ok(), "turbofish: {:?}", errors(&tf));
    // (b) annotated closure params, no annotation — still works.
    let ann = t.write(
            "ann.chz",
            "import H from a\nfn main():\n    h := H([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
    assert!(
        check_entry(&ann).is_ok(),
        "annotated closure params: {:?}",
        errors(&ann)
    );
    // (c) args win over annotation: a concrete int element vs an H[str] annotation is STILL a
    // type error (the seed only fills params left FREE by args; it must not override a pinned T).
    let bad = t.write(
            "bad.chz",
            "import H from a\nfn main():\n    h: H[str] = H([1], fn(x, y): x < y)\n    print(h.len())\n",
        );
    assert!(
        check_entry(&bad).is_err(),
        "args-pinned int vs H[str] annotation must still error"
    );
}

#[test]
fn annotation_pins_generic_ctor_in_if_else_branches() {
    // An if-else in value position is itself the bound value; BOTH branches are the tail
    // value, so each must receive the expected-type hint. The hint is a take()-once slot, so
    // without re-installing it per branch the SECOND-inferred branch starves and a generic
    // ctor there deadlocks on `T` — making acceptance depend purely on branch order.
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let e1 = t.write(
            "e1.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x > y) else: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
    assert!(
        check_entry(&e1).is_ok(),
        "if-else both branches: {:?}",
        errors(&e1)
    );
    // Swapping the branches must behave identically (no order dependence).
    let e2 = t.write(
            "e2.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x < y) else: H([], fn(x, y): x > y)\n    print(h.len())\n",
        );
    assert!(
        check_entry(&e2).is_ok(),
        "if-else swapped branches: {:?}",
        errors(&e2)
    );
}

#[test]
fn annotation_pins_generic_ctor_in_match_arms() {
    // Same per-branch hint requirement for an expression-`match` value: every arm body is a
    // tail value and must see the hint, not just the first-inferred arm.
    let t = TmpDir::new();
    t.write("a.chz", HEAP_MOD);
    let e = t.write(
            "m.chz",
            "import H from a\nfn main():\n    k := 1\n    h: H[int] = match k:\n        0: H([], fn(x, y): x > y)\n        _: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
    assert!(
        check_entry(&e).is_ok(),
        "match arms both pinned: {:?}",
        errors(&e)
    );
}

fn errors_or_empty(entry: &Path) -> Vec<String> {
    match check_entry(entry) {
        Ok(()) => Vec::new(),
        Err(es) => es.into_iter().map(|e| e.message).collect(),
    }
}
