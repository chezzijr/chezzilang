//! Checker tests: static methods, turbofish, operator protocols, protocol embedding, object safety.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== static (associated) methods: the "no self ⇒ static" rule =====

/// BASELINE INVARIANT: a static method (no `self`) called on an INSTANCE must error — calling a
/// static method on a value remains illegal. It must point at the `Type.method(...)` call form.
#[test]
fn static_method_instance_call_still_errors() {
    rejects(
        "struct Rect:\n    w: int\n    h: int\n    fn square(s: int) -> Rect:\n        return Rect(s, s)\nfn main():\n    r := Rect(1, 2)\n    _ := r.square(5)\n",
        "is a static method",
    );
}

/// A ZERO-PARAM method is a STATIC method (no `self`); calling it on an INSTANCE errors — it must
/// be called `Type.method()`. The instance-call remains illegal (the receiver-error sites stay).
#[test]
fn zero_param_method_instance_call_errors() {
    rejects(
        "struct Rect:\n    w: int\n    h: int\n    fn blank() -> Rect:\n        return Rect(0, 0)\nfn main():\n    r := Rect(1, 2)\n    _ := r.blank()\n",
        "is a static method",
    );
}

/// A struct static method called `Type.method(args)` type-checks to the method's return type.
#[test]
fn struct_static_method_call_typechecks() {
    ok(
        "struct Rect:\n    w: int\n    h: int\n    fn square(s: int) -> Rect:\n        return Rect(s, s)\nfn main():\n    r := Rect.square(5)\n    print(r.w)\n",
    );
}

/// An enum static method called `Enum.method(args)` type-checks (returns Option[Enum] here).
#[test]
fn enum_static_method_call_typechecks() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn from_str(s: str) -> Color?:\n        if s == \"red\":\n            return ?Color.Red\n        return None\nfn main():\n    c := Color.from_str(\"red\")\n    print(c == ?Color.Red)\n",
    );
}

/// A variant name always wins over a static method name: `Color.Red` stays the variant.
#[test]
fn enum_variant_wins_over_static() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn from_str(s: str) -> Color?:\n        return None\nfn main():\n    c := Color.Red\n    print(c == Color.Red)\n",
    );
}

/// A static method whose name collides with a variant is a decl-time error (disjointness).
#[test]
fn enum_variant_static_collision_errors() {
    rejects(
        "enum E:\n    Red\n    fn Red(x: int) -> E:\n        return E.Red\nfn main():\n    print(1)\n",
        "is already a variant of enum",
    );
}

/// PART 2: a static method declaring its OWN `[U]` type params is now ALLOWED; `U` is inferred from
/// the argument like a generic free fn. `Box[int].make(5)` (make[U](x:U)->Box[U]) checks to Box[int]
/// (the method param wins the value's type), and a non-generic `Plain.make(5)` infers `U` too.
#[test]
fn static_own_type_params_inferred_ok() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    b := Box[int].make(5)\n    n: int = b.val\n    print(n)\n",
    );
    ok(
        "struct Plain:\n    val: int\n    fn make[U](x: U) -> U:\n        return x\nfn main():\n    n: int = Plain.make(5)\n    print(n)\n",
    );
}

/// PART 2 (no-leak): a static method param that cannot be bound by any argument or turbofish degrades
/// to `Ty::Unknown`, never a free `Ty::Param`. `Box[int].make()` for `make[U]()->List[U]` (U unbound)
/// must type-check and push refine cleanly (mirrors `generic_static_no_turbofish_degrades_param`).
#[test]
fn static_own_type_params_no_leak_unknown() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U]() -> List[U]:\n        return []\nfn main():\n    xs: List[str] = Box[int].make()\n    xs.push(\"x\")\n    print(xs.len())\n",
    );
}

/// PART 2 (combined turbofish): `Box[int].make[str](\"hi\")` — enclosing-type targs from `Box[int]`
/// AND the method targ from `.make[str]` compose. `make[U](x:U)->Box[U]` ⇒ Box[str].
#[test]
fn combined_type_and_method_turbofish_ok() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    b := Box[int].make[str](\"hi\")\n    s: str = b.val\n    print(s)\n",
    );
}

/// PART 2 (combined mismatch): the explicit method turbofish targ that conflicts with the argument
/// type is an error (`Box[int].make[str](5)` — U=str but the arg is int).
#[test]
fn combined_method_turbofish_mismatch_errors() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    _ := Box[int].make[str](5)\n",
        "expected str",
    );
}

/// PART 2 (combined variant guard): a method-level turbofish on a generic VARIANT ctor is an error —
/// `Box[int].Has[str](5)` (a variant takes no method type args). Under the broadened parser steal this
/// arrives as a Field callee at the `type_apply_head` branch; without the explicit guard the `targs`
/// would be silently dropped (the old Index-over-Field block that errored is now bypassed).
#[test]
fn combined_variant_method_turbofish_errors() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    _ := Box[int].Has[str](5)\nmain()\n",
        "takes no method type arguments",
    );
}

/// PART 2: a static method param that SHADOWS the enclosing struct's type param name is rejected
/// (the existing `fn_sig` guard fires for static methods now that they're allowed).
#[test]
fn static_method_param_shadows_enclosing_rejected() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn make[T](x: T) -> Box[T]:\n        return Box(x)\nfn main():\n    print(1)\n",
        "shadows the struct's type parameter",
    );
}

/// A generic static factory called WITHOUT a type-level turbofish AND without an annotation leaves the
/// ENCLOSING type's param un-inferred — this must be REJECTED, not silently degraded to `Ty::Unknown`
/// (which swallowed any later argument and defeated homogeneity checking). `Box.empty() -> Box[T]` (no
/// turbofish, no arg binding `T`, no hint) leaks `T` as a `Ty::Param`; the first mismatching mutating use
/// (`b.items.push("x")`, element slot `Ty::Param(T)` out of scope) fires the construction-site diagnostic.
/// Mirrors the already-sound generic FREE-FUNCTION path (`mkbox()` used heterogeneously rejects the same
/// way). Method-OWN `[U]` params still degrade to `Ty::Unknown` (see `static_own_type_params_no_leak_unknown`).
#[test]
fn generic_static_no_turbofish_rejects_uninferred_param() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\nfn main():\n    b := Box.empty()\n    b.items.push(\"x\")\n    print(b.items.len())\n",
        "cannot infer the",
    );
}

/// A `let` annotation pins the ENCLOSING type param of an un-turbofished static factory from the
/// expected type (`b: Box[int] = Box.empty()` seeds `T=int`), then homogeneous use is accepted.
#[test]
fn annotation_pins_static_factory_ok() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b: Box[int] = Box.empty()\n    b.add(5)\n    print(b.first())\n",
    );
}

/// Same annotation-pinned factory: once `T=int` is fixed from the annotation, a heterogeneous add is
/// rejected (`b.add(\"hello\")` — expected int).
#[test]
fn annotation_pins_static_factory_then_rejects_heterogeneous() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b: Box[int] = Box.empty()\n    b.add(\"hello\")\n",
        "expected int",
    );
}

/// SAME HOLE in generic ENUMS: an un-turbofished, un-annotated enum static factory (`Wrap.none()` for
/// `fn none() -> Wrap[T]`) leaks the enclosing `T`; a heterogeneous instance-method use (`w.put(\"s\")`
/// where `fn put(self, x: T)`) is then rejected. The struct fix (shared degrade loop + `seed_from_hint`)
/// covers enums with no extra code.
#[test]
fn generic_enum_static_factory_rejects_heterogeneous() {
    rejects(
        "enum Wrap[T]:\n    Has(T)\n    Empty\n    fn none() -> Wrap[T]:\n        return Wrap.Empty\n    fn put(self, x: T):\n        print(x)\nfn main():\n    w := Wrap.none()\n    w.put(\"s\")\nmain()\n",
        "cannot infer the",
    );
}

/// GRAPH-PATH regression (REPRO A, the full check-ok → runtime-trap program): a static factory called
/// with no pin, a heterogeneous add, then arithmetic on the read-back value. Proves the fix holds
/// through `build_graph` → `check_graph` (the real CLI path), not only single-module `check_src`.
#[test]
fn graph_static_factory_uninferred_rejects_repro_a() {
    entry_rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b := Box.empty()\n    b.add(\"hello\")\n    x := b.first()\n    print(x + 1)\nmain()\n",
        "cannot infer the type arguments of `b`",
    );
}

/// GRAPH-PATH regression (enum variant of REPRO A): the enum static-factory hole rejects through the
/// real graph path too.
#[test]
fn graph_enum_static_factory_uninferred_rejects() {
    entry_rejects(
        "enum Wrap[T]:\n    Has(T)\n    Empty\n    fn none() -> Wrap[T]:\n        return Wrap.Empty\n    fn put(self, x: T):\n        print(x)\nfn main():\n    w := Wrap.none()\n    w.put(1)\n    w.put(\"s\")\nmain()\n",
        "cannot infer the type arguments of `w`",
    );
}

/// OVER-REJECTION guard: a static factory whose RETURN omits the enclosing param (`fn count() -> int`)
/// leaves no free param — `subst` is a no-op, so it must NOT falsely reject. Confirms the degrade-drop
/// was scoped to params that actually appear in the return.
#[test]
fn static_factory_return_omits_param_still_ok() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn count() -> int:\n        return 0\nfn main():\n    n := Box.count()\n    print(n)\n",
    );
}

/// A generic static method INFERS the enclosing type's params from its arguments, like the ctor:
/// `Box.wrap(5)` for `fn wrap(x: T) -> Box[T]` yields `Box[int]` with no turbofish.
#[test]
fn generic_static_infers_type_param_from_arg() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn wrap(x: T) -> Box[T]:\n        return Box([x])\nfn main():\n    b := Box.wrap(5)\n    n: int = b.items[0]\n    print(n)\n",
    );
}

/// A generic static via the TYPE-level turbofish `Box[int].empty()` type-checks (v1's supported form).
#[test]
fn generic_static_turbofish_box_int_empty_typechecks() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn len(self) -> int:\n        return self.items.len()\nfn main():\n    b := Box[int].empty()\n    print(b.len())\n",
    );
}

/// PART 2: a method-level turbofish on a static method that declares NO `[U]` is an arity error —
/// `Box.empty[int]()` for `fn empty()->Box[T]` (no own type params) takes no method type arguments.
#[test]
fn method_level_turbofish_on_non_generic_static_errors() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\nfn main():\n    _ := Box.empty[int]()\n",
        "type argument",
    );
}

/// PART 2 (instance multi-turbofish): an instance method declaring its OWN `[A, B]` takes a multi
/// type-arg + multi value-arg turbofish `s.m[int, str](1, "x")`.
#[test]
fn instance_method_multi_turbofish_ok() {
    ok(
        "struct S:\n    n: int\n    fn m[A, B](self, x: A, y: B) -> A:\n        return x\nfn main():\n    s := S(0)\n    r: int = s.m[int, str](1, \"x\")\n    print(r)\n",
    );
}

/// PART 2 (instance turbofish mismatch): the explicit instance-method turbofish targ that conflicts
/// with the arg is an error (previously the targs were DROPPED, so this was silently accepted).
#[test]
fn instance_method_turbofish_mismatch_errors() {
    rejects(
        "struct S:\n    n: int\n    fn m[A](self, x: A) -> A:\n        return x\nfn main():\n    s := S(0)\n    _ := s.m[str](5)\n",
        "expected str",
    );
}

/// PART 2 (soundness): a method type param in RECEIVER position (`fn m[U](self: U)`) turbofished to a
/// type that contradicts the actual receiver must ERROR — without the post-subst receiver check the
/// turbofish `b.idret[str]()` on a `Box[int]` certified `str` for a struct value (a soundness hole).
#[test]
fn method_param_in_receiver_turbofish_contradiction_errors() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn idret[U](self: U) -> U:\n        return self\nfn main():\n    b := Box(5)\n    x: str = b.idret[str]()\n    print(x)\n",
        "'self' is only allowed as the first parameter of a method",
    );
}

/// PART 2 (.iter swallow fix): a method-level turbofish on a BUILTIN/non-generic member must error
/// like `xs.len[int]()` — including the `.iter` fast-path (`xs.iter[int]()` was silently accepted).
#[test]
fn iter_method_turbofish_errors() {
    rejects(
        "fn main():\n    xs := [1, 2, 3]\n    _ := xs.iter[int]()\n",
        "type argument",
    );
}

/// TICKET-210: `recv.name[X](args)` keeps both readings, and the checker chooses. A fn-list FIELD
/// on an indexed receiver, `arr[i].handlers[k](10)`, is data, so the bracket indexes it and the
/// element is called (Go, CPython). It used to read `k` as a type and reject.
#[test]
fn var_index_then_call_on_indexed_receiver_indexes() {
    ok(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    i := 0\n    k := 0\n    print(arr[i].handlers[k](10))\n",
    );
}

/// The parenthesized index-then-call, `(arr[i].handlers[k])(10)`, still type-checks (the fn-valued
/// field is indexed then the value is called).
#[test]
fn parenthesized_var_index_then_call_ok() {
    ok(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    i := 0\n    k := 0\n    print((arr[i].handlers[k])(10))\n",
    );
}

/// A NUMERIC index-then-call on an indexed receiver stays ordinary index-then-call: `0` is not a
/// type, so `try_parse_type_arg_call` backtracks and `arr[0].handlers[0](20)` keeps its meaning.
#[test]
fn numeric_index_then_call_on_indexed_receiver_stays() {
    ok(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    print(arr[0].handlers[0](20))\n",
    );
}

/// BOUNDARY: a plain subscript with NO following call stays an Index on a non-bare receiver — neither
/// `obj.items[0]` (numeric) nor `m.data[k]` (variable) is followed by `(`, so the steal never fires.
#[test]
fn subscript_without_call_on_non_bare_receiver_stays_index() {
    ok(
        "struct S:\n    items: List[int]\nfn main():\n    s := S([1, 2, 3])\n    print(s.items[0])\n",
    );
    ok(
        "struct M:\n    data: Map[str, int]\nfn main():\n    m := M({\"a\": 1})\n    k := \"a\"\n    print(m.data[k])\n",
    );
}

// ===== member-level turbofish on a NON-BARE receiver (broadened parser steal) =====
// `recv.name[X](args)` now parses as a method turbofish on ANY receiver (not just a bare ident),
// so a call-result / factory-result / field / index receiver can all take a member turbofish.

/// Call-result receiver: `W(1).cast[str]("a")` — the receiver is a struct ctor CALL, not a bare
/// ident. Previously mis-parsed as `Index{Field, str}` then a value-call ⇒ "unknown name 'str'".
#[test]
fn member_turbofish_on_call_result_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    a := W(1).cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Factory-result receiver: `mk().cast[str]("a")` — receiver is a free-fn call.
#[test]
fn member_turbofish_on_factory_result_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn mk() -> W[int]:\n    return W(1)\nfn main():\n    a := mk().cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Field receiver: `h.w.cast[str]("a")` — receiver is a (deeper) field access, NOT a bare ident.
#[test]
fn member_turbofish_on_field_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nstruct H:\n    w: W[int]\nfn main():\n    h := H(W(1))\n    a := h.w.cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Index receiver: `xs[0].cast[str]("a")` — receiver is an index expression.
#[test]
fn member_turbofish_on_index_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    xs := [W(1)]\n    a := xs[0].cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Chained: `W(1).cast("a").cast[bool](true)` — a member turbofish on the result of a prior method
/// call (the receiver is itself a method-call expression).
#[test]
fn member_turbofish_chained_on_method_result_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    a := W(1).cast(\"a\").cast[bool](true)\n    b: bool = a.v\n    print(b)\nmain()\n",
    );
}

/// Multi-type-arg member turbofish on a NON-BARE receiver: `mk().pair[int, str](1, "x")`. The comma
/// in the type list means this can NEVER ride the old single-arg Index-reinterpret path — it REQUIRES
/// the broadened parser steal.
#[test]
fn member_multi_turbofish_on_non_bare_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn pair[A, B](self, x: A, y: B) -> A:\n        return x\nfn mk() -> W[int]:\n    return W(1)\nfn main():\n    r: int = mk().pair[int, str](1, \"x\")\n    print(r)\nmain()\n",
    );
}

/// Nested-generic member turbofish on a NON-BARE receiver: `W(1).cast[Map[str, int]](m)`. The inner
/// `[..]` of the type arg likewise cannot ride the Index-reinterpret path — REQUIRES the broaden.
#[test]
fn member_nested_generic_turbofish_on_non_bare_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    m: Map[str, int] = {\"a\": 1}\n    a := W(1).cast[Map[str, int]](m)\n    n: int = a.v[\"a\"]\n    print(n)\nmain()\n",
    );
}

// ===== type-side declaration-site turbofish (PART 1): `Type[T].Variant` / `Type[T].static()` =====

/// `Box[int].Has(5)` — a generic enum VARIANT constructor via the type-level turbofish (single arg,
/// rides the `Index` reinterpretation path). Resolves to `Box[int]`.
#[test]
fn turbofish_type_variant_single_arg_ok() {
    ok(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[int] = Box[int].Has(5)\n    print(1)\nmain()\n",
    );
}

/// The resolved variant carries the EXPLICIT type args — `Box[int].Has(5)` is `Box[int]`, so binding
/// it to `Box[str]` must error (proving the targs are seeded, not left `Unknown`).
#[test]
fn turbofish_type_variant_targs_seeded_not_unknown() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[str] = Box[int].Has(5)\n    print(1)\nmain()\n",
        "Box[int]",
    );
}

/// `E[int, str].Pair(1, \"x\")` — MULTI type-arg turbofish (the `TypeApply` carrier path) on a
/// 2-param enum resolves to `E[int, str]`.
#[test]
fn turbofish_type_variant_multi_arg_ok() {
    ok(
        "enum E[T, U]:\n    Pair(T, U)\nfn main():\n    p: E[int, str] = E[int, str].Pair(1, \"x\")\n    print(1)\nmain()\n",
    );
}

/// `Box[int, str].Has(5)` — too many type args for a single-param enum errors via the existing
/// arity check (`seed_targs`).
#[test]
fn turbofish_type_variant_arity_mismatch_rejected() {
    rejects(
        "enum Box[T]:\n    Has(T)\nfn main():\n    _ := Box[int, str].Has(5)\nmain()\n",
        "type argument",
    );
}

/// Nullary value form `Box[int].Empty` resolves to `Box[int]` (the explicit args seeded), NOT
/// `Box[Unknown]` — binding to `Box[str]` must error.
#[test]
fn turbofish_type_nullary_variant_seeded() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[str] = Box[int].Empty\n    print(1)\nmain()\n",
        "Box[int]",
    );
    ok(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[int] = Box[int].Empty\n    print(1)\nmain()\n",
    );
}

/// The OLD gliding form `Box.Full[int](9)` (type args on the VARIANT) is REMOVED — it must error
/// with a redirect to the type-side form.
#[test]
fn old_gliding_variant_turbofish_redirects() {
    rejects(
        "enum Box[T]:\n    Full(T)\n    Empty\nfn main():\n    _ := Box.Full[int](9)\nmain()\n",
        "put the type arguments on the type: Box[int].Full(",
    );
}

/// Regression: a generic STATIC method via the type-level turbofish still resolves (variant-first
/// falls through to the static path when no variant matches the member name).
#[test]
fn turbofish_type_static_method_regression() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn len(self) -> int:\n        return self.items.len()\nfn main():\n    b := Box[int].empty()\n    print(b.len())\nmain()\n",
    );
}

/// Task 5 (HARD rename): the builtin container TYPE names are now PascalCase `List`/`Map`/`Set`.
/// These resolve as types in annotations, nested forms, fn params/returns, and struct fields.
#[test]
fn pascal_containers_resolve() {
    ok(
        "fn main():\n    x: List[int] = [1]\n    m: Map[str, int] = {\"a\": 1}\n    s: Set[int] = {1}\n    nested: List[Map[str, Set[int]]] = []\n    print(x.len())\n    print(m.len())\n    print(s.len())\n    print(nested.len())\nmain()\n",
    );
}

/// The lowercase `list`/`map`/`set` are GONE as type names (hard rename, no alias): they fall to the
/// unknown-type branch.
#[test]
fn lowercase_containers_rejected() {
    rejects(
        "fn main():\n    x: list[int] = [1]\n    print(x.len())\nmain()\n",
        "unknown",
    );
    rejects(
        "fn main():\n    m: map[str, int] = {}\n    print(m.len())\nmain()\n",
        "unknown",
    );
    rejects(
        "fn main():\n    s: set[int] = {}\n    print(s.len())\nmain()\n",
        "unknown",
    );
}

/// The PascalCase constructors `List(it)`/`Set(it)`/`Map(it)` + empty `Set()` type-check.
#[test]
fn pascal_ctor_calls() {
    ok(
        "fn main():\n    a := List([1, 2])\n    b := Set([1, 2, 3])\n    c := Map([(\"a\", 1)])\n    d: Set[int] = Set()\n    print(a.len())\n    print(b.len())\n    print(c.len())\n    print(d.len())\nmain()\n",
    );
}

// ===== M22: operator protocols Div/Mod/Neg + protocol embedding + Arithmetic =====

/// A struct defining div/mod/neg overloads `/`, `%`, and unary `-`.
#[test]
fn div_mod_neg_struct_overload_typechecks() {
    ok(
        "struct V:\n    n: int\n    fn div(self, other: V) -> V:\n        return V(self.n / other.n)\n    fn mod(self, other: V) -> V:\n        return V(self.n % other.n)\n    fn neg(self) -> V:\n        return V(-self.n)\nfn main():\n    a := V(7)\n    b := V(2)\n    print((a / b).n)\n    print((a % b).n)\n    print((-a).n)\nmain()\n",
    );
}

/// GENERALIZATION GUARD: a USER empty protocol (`protocol Foo:\n    pass`) is an accept-all top type
/// byte-identical to the reserved `Any` — accepts scalars + structs as a param and types a
/// heterogeneous list. The accept-all behaviour is NOT special-cased on the literal name "Any".
#[test]
fn empty_protocol_accepts_all_like_any() {
    ok("protocol Foo:\n    pass\n\
        struct P:\n    x: int\n\
        fn takes_foo(v: Foo) -> int:\n    return 1\n\
        fn takes_any(v: Any) -> int:\n    return 1\n\
        fn main():\n\
        \x20   takes_foo(1)\n\
        \x20   takes_foo(\"a\")\n\
        \x20   takes_foo(true)\n\
        \x20   takes_foo(P(1))\n\
        \x20   takes_any(1)\n\
        \x20   xs: List[Foo] = [1, \"a\", true]\n\
        \x20   ys: List[Any] = [1, \"a\", true]\n\
        \x20   print(xs.len() + ys.len())\n\
        main()\n");
}

/// TICKET-280 (owner decision U15): a zero-field struct (`struct S:\n    pass`) is no `Hashable` on
/// its own, so it is neither a Set element nor a Map key without `hash` and `eq`.
#[test]
fn empty_struct_is_not_hashable() {
    rejects(
        "struct S:\n    pass\n\
        fn main():\n\
        \x20   seen: Set[S] = {S(), S()}\n\
        main()\n",
        "Set element type must implement Hashable",
    );
}

/// TICKET-250 — a `hash` that does not satisfy `Hashable`'s requirement (wrong return, extra
/// parameter) is an ordinary method, so the zero-field struct has no `hash` hook and is no key
/// (TICKET-280: no struct is `Hashable` on its own).
#[test]
fn empty_struct_with_mistyped_hash_is_not_the_hook() {
    rejects(
        "struct S:\n    fn hash(self) -> str:\n        return \"x\"\n\
        fn main():\n\
        \x20   m: Map[S, int] = {S(): 1}\n\
        \x20   print(m.len())\n\
        main()\n",
        "Map key type must implement Hashable",
    );
    // Arity variant: an extra parameter is the same class.
    rejects(
        "struct S:\n    fn hash(self, k: int) -> int:\n        return k\n\
        fn main():\n\
        \x20   seen: Set[S] = {S()}\n\
        \x20   print(seen.len())\n\
        main()\n",
        "Set element type must implement Hashable",
    );
}

/// A zero-field struct WITH a correctly-typed `hash(self) -> int` and an `eq` is Hashable.
#[test]
fn empty_struct_with_valid_hash_ok() {
    ok("struct S:\n    fn hash(self) -> int:\n        return 42\n\
        \x20   fn eq(self, other: S) -> bool:\n        return true\n\
        fn main():\n\
        \x20   seen: Set[S] = {S()}\n\
        \x20   print(seen.len())\n\
        main()\n");
}

/// `[T: Div]` / `[T: Mod]` / `[T: Neg]` generic bounds flow, with int instantiation.
#[test]
fn div_mod_neg_bound_flows() {
    ok(
        "import Div from std.ops\nfn d[T: Div](a: T, b: T) -> T:\n    return a / b\nfn main():\n    print(d(7, 2))\nmain()\n",
    );
    ok(
        "import Mod from std.ops\nfn m[T: Mod](a: T, b: T) -> T:\n    return a % b\nfn main():\n    print(m(7, 2))\nmain()\n",
    );
    ok(
        "import Neg from std.ops\nfn n[T: Neg](a: T) -> T:\n    return -a\nfn main():\n    print(n(5))\nmain()\n",
    );
}

/// `[T: Arithmetic]` accepts a struct with all four ops and uses them in the body; rejects a struct
/// missing div (error mentions div).
#[test]
fn arithmetic_bundle_accepts_and_rejects() {
    let prelude = "struct V:\n    n: int\n    fn add(self, other: V) -> V:\n        return V(self.n + other.n)\n    fn sub(self, other: V) -> V:\n        return V(self.n - other.n)\n    fn mul(self, other: V) -> V:\n        return V(self.n * other.n)\n    fn div(self, other: V) -> V:\n        return V(self.n / other.n)\n";
    ok(&format!(
        "import Arithmetic from std.ops\n{prelude}fn calc[T: Arithmetic](a: T, b: T) -> T:\n    return a + b - a * b\nfn main():\n    print(calc(V(6), V(2)).n)\nmain()\n"
    ));
    // A struct lacking div fails an Arithmetic bound, mentioning div.
    let no_div = "import Arithmetic from std.ops\nstruct W:\n    n: int\n    fn add(self, other: W) -> W:\n        return W(self.n + other.n)\n    fn sub(self, other: W) -> W:\n        return W(self.n - other.n)\n    fn mul(self, other: W) -> W:\n        return W(self.n * other.n)\nfn calc[T: Arithmetic](a: T, b: T) -> T:\n    return a + b\nfn main():\n    print(calc(W(1), W(2)).n)\nmain()\n";
    rejects(no_div, "div");
}

/// Inside a `[T: Arithmetic]` body, `+ - * /` all type-check (transitive bound flattening).
#[test]
fn arithmetic_body_uses_ops() {
    ok(
        "import Arithmetic from std.ops\nfn f[T: Arithmetic](a: T, b: T) -> T:\n    return ((a + b) - (a * b)) / a\nfn main():\n    print(f(8, 2))\nmain()\n",
    );
}

/// A user protocol embedding Arithmetic plus its own methods (transitive embedding).
#[test]
fn user_protocol_embeds_arithmetic_and_own_methods() {
    ok(
        "import Arithmetic from std.ops\nprotocol Field:\n    Arithmetic\n    fn zero(self) -> Self\nstruct V:\n    n: int\n    fn add(self, other: V) -> V:\n        return V(self.n + other.n)\n    fn sub(self, other: V) -> V:\n        return V(self.n - other.n)\n    fn mul(self, other: V) -> V:\n        return V(self.n * other.n)\n    fn div(self, other: V) -> V:\n        return V(self.n / other.n)\n    fn zero(self) -> V:\n        return V(0)\nfn g[T: Field](a: T, b: T) -> T:\n    return a / b\nfn main():\n    print(g(V(9), V(3)).n)\nmain()\n",
    );
}

/// Diamond dedup: embedding Arithmetic AND Add (both supply the same `add` sig) is legal.
#[test]
fn embed_diamond_dedup_ok() {
    ok("import Add, Arithmetic from std.ops\nprotocol P:\n    Arithmetic + Add\n");
}

/// An own `fn` colliding with an embedded-required method name is a declare-time error.
#[test]
fn own_fn_vs_embed_collision_errors() {
    rejects(
        "import Add from std.ops\nprotocol P:\n    Add\n    fn add(self, other: Self) -> Self\n",
        "conflicts with embedded",
    );
}

/// Two embeds pulling the same method name with differing signatures is an error.
#[test]
fn embed_sig_conflict_errors() {
    rejects(
        "protocol P1:\n    fn m(self) -> int\nprotocol P2:\n    fn m(self, o: Self) -> int\nprotocol Q:\n    P1 + P2\n",
        "conflicting signature",
    );
}

/// Cyclic embedding (A embeds B, B embeds A) is rejected at declare time.
#[test]
fn embed_cycle_errors() {
    rejects("protocol A:\n    B\nprotocol B:\n    A\n", "cyclic");
}

// ===== M22 — the embed set is flattened at EVERY use site, not just at a bound (gaps.md's
// "protocol embeds" row). `spec.md` always promised this; five lookup sites read the protocol's OWN
// method list only, so an embedded method was invisible through an interface value, through a bound,
// and to `<` / `in`. Go is the ancestor for interfaces and agrees on all of it (an embedded method is
// callable through the interface value, and a `Person` value is assignable to a `Named` parameter);
// pyright accepts the Python `Protocol`-inheritance twin. Every acceptance below is paired with the
// negative that proves the rule did not just go permissive. Runtime dispatch is by NAME and always
// worked — the end-to-end twin of these lives in `tests/chz/spec/protocol_embed_test.chz`. =====

/// The filed row: an embedded method is callable through the protocol EXISTENTIAL.
#[test]
fn embedded_method_callable_through_interface_value() {
    ok(
        "protocol Named:\n    fn name(self) -> str\nprotocol Person:\n    Named\n    fn age(self) -> int\nstruct Dev:\n    n: str\n    a: int\n    fn name(self) -> str:\n        return self.n\n    fn age(self) -> int:\n        return self.a\nfn show(p: Person) -> str:\n    return p.name()\nfn main():\n    print(show(Dev(\"ada\", 36)))\nmain()\n",
    );
}

/// …and through a BOUND — the same own-only lookup, one arm over.
#[test]
fn embedded_method_callable_through_bound() {
    ok(
        "protocol Named:\n    fn name(self) -> str\nprotocol Person:\n    Named\n    fn age(self) -> int\nstruct Dev:\n    n: str\n    a: int\n    fn name(self) -> str:\n        return self.n\n    fn age(self) -> int:\n        return self.a\nfn show[T: Person](p: T) -> str:\n    return p.name()\nfn main():\n    print(show(Dev(\"ada\", 36)))\nmain()\n",
    );
}

/// A method NO embed supplies is still rejected — the lookup widened, it did not go permissive.
#[test]
fn unembedded_method_still_rejected_on_interface_value() {
    rejects(
        "protocol Named:\n    fn name(self) -> str\nprotocol Person:\n    Named\nfn f(p: Person) -> str:\n    return p.nope()\n",
        "has no method 'nope'",
    );
}

/// A protocol VALUE is assignable to a parameter of a protocol it embeds — Go's interface-to-
/// interface assignment (`func onlyNamed(n Named)` accepts a `Person`, measured).
#[test]
fn protocol_value_satisfies_its_embedded_protocol() {
    ok(
        "protocol Named:\n    fn name(self) -> str\nprotocol Person:\n    Named\n    fn age(self) -> int\nstruct Dev:\n    n: str\n    a: int\n    fn name(self) -> str:\n        return self.n\n    fn age(self) -> int:\n        return self.a\nfn only_named(n: Named) -> str:\n    return n.name()\nfn take(p: Person) -> str:\n    return only_named(p)\nfn main():\n    print(take(Dev(\"ada\", 36)))\nmain()\n",
    );
}

/// …but an UNRELATED protocol parameter still rejects the same value.
#[test]
fn protocol_value_does_not_satisfy_an_unrelated_protocol() {
    rejects(
        "protocol Named:\n    fn name(self) -> str\nprotocol Other:\n    fn zzz(self) -> int\nprotocol Person:\n    Named\nfn g(o: Other) -> int:\n    return o.zzz()\nfn f(p: Person) -> int:\n    return g(p)\n",
        "expected Other, found Person",
    );
}

/// Arg invariance survives the widening: `Box2[str]` is not a `Box2[int]`.
#[test]
fn protocol_value_arg_invariance_survives_embed_widening() {
    rejects(
        "protocol Box2[T]:\n    fn get(self, i: int) -> T\nfn want(c: Box2[int]) -> int:\n    return c.get(0)\nfn f(c: Box2[str]) -> int:\n    return want(c)\n",
        "expected Box2[int], found Box2[str]",
    );
}

/// `<` resolves through an embedded `Comparable` on a bound, exactly as a declared `compare` does.
#[test]
fn ordering_through_embedded_comparable_bound() {
    ok(
        "import Comparable from std.cmp\nprotocol Ord2:\n    Comparable\n    fn tag(self) -> str\nstruct N:\n    v: int\n    fn compare(self, other: N) -> int:\n        return self.v - other.v\n    fn eq(self, other: N) -> bool:\n        return self.v == other.v\n    fn tag(self) -> str:\n        return \"n\"\nfn lt[T: Ord2](a: T, b: T) -> bool:\n    return a < b\nfn main():\n    print(lt(N(1), N(2)))\nmain()\n",
    );
}

/// `in` resolves through an embedded `Contains` on a bound.
#[test]
fn membership_through_embedded_contains_bound() {
    ok(
        "import Contains from std.ops\nprotocol Bag:\n    Contains[int]\n    fn tag(self) -> str\nstruct B:\n    xs: List[int]\n    fn contains(self, item: int) -> bool:\n        return item in self.xs\n    fn tag(self) -> str:\n        return \"b\"\nfn has[T: Bag](b: T, x: int) -> bool:\n    return x in b\nfn main():\n    print(has(B([1, 2]), 2))\nmain()\n",
    );
}

/// A PARAMETERIZED embed is re-spelled in the OUTER protocol's vocabulary, so `Bag[int]` gives `in`
/// an item type of `int`. Without that substitution the embedded arg resolves to `Unknown` and every
/// item type passes — this is the test that fails if the flatten skips `Bound.args`.
#[test]
fn parameterized_embed_substitutes_its_arg_into_the_item_type() {
    ok(
        "import Contains from std.ops\nprotocol Bag[T]:\n    Contains[T]\nfn has(b: Bag[int]) -> bool:\n    return 3 in b\n",
    );
    rejects(
        "import Contains from std.ops\nprotocol Bag[T]:\n    Contains[T]\nfn has(b: Bag[int]) -> bool:\n    return \"x\" in b\n",
        "cannot test membership of str in Bag[int]",
    );
    // …and the same through a bound, where the ambient scope is the caller's params, not the
    // protocol's — the seam that made the arg resolve to the wrong `T` entirely.
    rejects(
        "import Contains from std.ops\nprotocol Bag[T]:\n    Contains[T]\nfn has[C: Bag[int]](b: C) -> bool:\n    return \"x\" in b\n",
        "cannot test membership of str in C",
    );
}

// ===== OBJECT SAFETY — `Self` in a PARAMETER slot is not dispatchable through an existential.
//
// A protocol value erases which witness it holds, so two values of one protocol need not be the same
// concrete type. `fn add(self, other: Self) -> Self` through a `Vecish` would hand a `W` to `V::add` —
// `check: ok`, then `runtime error: no field 'x' on W(s=q)`. Rust states the same
// rule (a `Self`-typed parameter makes a trait non-`dyn`-able) and Go bans `Self` from interfaces
// outright, so neither ancestor admits the program. Every operator protocol's method is
// `(self, Self) -> Self`, which is why `+ - * / % <` are all un-dispatchable on an existential; the
// SOUND spelling binds the operands together with a generic parameter, and is fenced below. `Self`
// in the RETURN is fine (it widens to the existential) — that is what keeps unary `-` working. =====

/// `+` on two existentials is rejected: the pair is not provably the same witness.
#[test]
fn operator_rejected_on_a_protocol_existential_pair() {
    rejects(
        "protocol Vecish:\n    fn add(self, other: Self) -> Self\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\nfn plus(a: Vecish, b: Vecish) -> Vecish:\n    return a + b\n",
        "cannot apply + to Vecish and Vecish",
    );
    // The builtin operator protocol used directly as the annotation is the same case…
    rejects(
        "import Add from std.ops\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\nfn plus(a: Add, b: Add) -> Add:\n    return a + b\n",
        "cannot apply + to Add and Add",
    );
    // …and so is reaching it through an EMBED.
    rejects(
        "import Add from std.ops\nprotocol Vecish:\n    Add\n    fn tag(self) -> str\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\n    fn tag(self) -> str:\n        return \"v\"\nfn plus(a: Vecish, b: Vecish) -> Vecish:\n    return a + b\n",
        "cannot apply + to Vecish and Vecish",
    );
    // `<` through an embedded `Comparable` is the same shape (`compare(self, o: Self)`).
    rejects(
        "import Comparable from std.cmp\nprotocol Ord2:\n    Comparable\n    fn tag(self) -> str\nfn lt(a: Ord2, b: Ord2) -> bool:\n    return a < b\n",
        "cannot compare Ord2 and Ord2",
    );
}

/// The SOUND spelling of the same program — a generic parameter binds both operands to ONE witness.
/// This is the remedy the diagnostic names, so it has to actually work.
#[test]
fn the_generic_bound_spelling_of_an_operator_over_a_protocol_is_accepted() {
    ok(
        "import Add from std.ops\nprotocol Vecish:\n    Add\n    fn tag(self) -> str\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\n    fn tag(self) -> str:\n        return \"v\"\nfn plus[T: Vecish](a: T, b: T) -> T:\n    return a + b\nfn main():\n    print(plus(V(1), V(2)).tag())\nmain()\n",
    );
}

/// The explicit-call form is rejected too, with the remedy in the message — an operator that lowers
/// to a method must not have a legal hand-written spelling the operator lacks.
#[test]
fn self_typed_method_rejected_on_a_protocol_existential() {
    rejects(
        "protocol Vecish:\n    fn add(self, other: Self) -> Self\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\nfn plus(a: Vecish, b: Vecish) -> Vecish:\n    return a.add(b)\n",
        "not callable through the protocol value Vecish",
    );
}

/// A protocol value cannot WITNESS a generic type param whose bound needs a `Self`-parameterized
/// method — two slots of one param would then hold two different witnesses, which is the same hole
/// the operator arms have, reached by a different route. Rejected whether the `Self` method is the
/// protocol's OWN or arrives through an EMBED, and whichever protocol the bound names.
#[test]
fn a_self_parameterized_protocol_value_cannot_witness_a_type_param() {
    let v = "import Add from std.ops\nprotocol Vecish:\n    Add\n    fn tag(self) -> str\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\n    fn tag(self) -> str:\n        return \"v\"\n";
    // `Self` via an EMBED — the spelling an own-methods-only guard misses.
    rejects(
        &format!(
            "import Add from std.ops\n{v}fn sum2[T: Add](a: T, b: T) -> T:\n    return a + b\nfn f(a: Vecish, b: Vecish) -> Vecish:\n    return sum2(a, b)\n"
        ),
        "cannot use the protocol value Vecish as type parameter 'T'",
    );
    // …and when the bound names the embedding protocol itself.
    rejects(
        &format!(
            "{v}fn sum2[T: Vecish](a: T, b: T) -> T:\n    return a + b\nfn f(a: Vecish, b: Vecish) -> Vecish:\n    return sum2(a, b)\n"
        ),
        "cannot use the protocol value Vecish as type parameter 'T'",
    );
    // `Self` declared directly on the protocol is the same rejection.
    rejects(
        "protocol Vecish:\n    fn add(self, other: Self) -> Self\nfn sum2[T: Vecish](a: T, b: T) -> T:\n    return a.add(b)\nfn f(a: Vecish, b: Vecish) -> Vecish:\n    return sum2(a, b)\n",
        "cannot use the protocol value Vecish as type parameter 'T'",
    );
}

/// REGRESSION — the object-safety guard must NOT reach plain assignability. Passing a `Vecish` value
/// to a `Vecish` parameter pairs nothing and is sound; an earlier placement of the guard (inside
/// `satisfies_args_d`, which cannot tell a bound from an annotation) rejected it with the absurd
/// `expected Vecish, found Vecish`, and did so for EVERY protocol carrying a `Self` method.
#[test]
fn a_self_parameterized_protocol_value_still_passes_to_its_own_annotation() {
    ok(
        "protocol Vecish:\n    fn add(self, other: Self) -> Self\n    fn tag(self) -> str\nstruct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\n    fn tag(self) -> str:\n        return \"v\"\nfn takes(p: Vecish) -> str:\n    return p.tag()\nfn forward(p: Vecish) -> str:\n    return takes(p)\nfn main():\n    a: Vecish = V(1)\n    print(takes(a))\n    print(forward(a))\nmain()\n",
    );
    // …and it still satisfies a protocol it EMBEDS (the interface-to-interface case), which also
    // routes through `satisfies_args_d` and so would have been collateral of the same misplacement.
    ok(
        "protocol Named:\n    fn name(self) -> str\nprotocol Person:\n    Named\n    fn add(self, other: Self) -> Self\nstruct Dev:\n    n: str\n    fn name(self) -> str:\n        return self.n\n    fn add(self, other: Dev) -> Dev:\n        return self\nfn only_named(n: Named) -> str:\n    return n.name()\nfn take(p: Person) -> str:\n    return only_named(p)\nfn main():\n    print(take(Dev(\"ada\")))\nmain()\n",
    );
}

/// TWO DIFFERENT protocols are not an operand pair either.
#[test]
fn operator_rejects_mismatched_protocol_operands() {
    rejects(
        "protocol Vecish:\n    fn add(self, other: Self) -> Self\nprotocol Other:\n    fn add(self, other: Self) -> Self\nfn plus(a: Vecish, b: Other) -> Vecish:\n    return a + b\n",
        "cannot apply + to Vecish and Other",
    );
}

/// A method with `Self` only in the RETURN stays callable — it widens to the existential, which is a
/// legal supertype of whatever the witness returns. The line between this and the rejections above
/// is the whole object-safety rule, so pin it.
#[test]
fn self_in_return_position_stays_callable_on_an_existential() {
    ok(
        "protocol Negish:\n    fn neg(self) -> Self\n    fn tag(self) -> str\nstruct V:\n    x: int\n    fn neg(self) -> V:\n        return V(-self.x)\n    fn tag(self) -> str:\n        return \"v\"\nfn flip(a: Negish) -> Negish:\n    return a.neg()\nfn main():\n    print(flip(V(1)).tag())\nmain()\n",
    );
}

/// A cyclic — or merely diamond-shaped — embed graph must not blow up the checker. Termination is a
/// VISITED SET, not a depth cap: with branching ≥ 2 a depth bound of 64 is 2^64 visits. THREE
/// separate walkers had this shape (`protocol_method_sig_d`, `protocol_provides_d`,
/// `satisfies_args_d`) and a fourth — `flatten_embed_methods` — used a path stack, which detects a
/// cycle but does nothing about SHARING, so it re-walked every shared subtree once per route.
#[test]
fn a_branching_embed_graph_terminates() {
    // On a method MISS through a cyclic graph.
    rejects(
        "protocol A:\n    B\n    C\nprotocol B:\n    A\n    C\nprotocol C:\n    A\n    B\nfn f(x: A) -> int:\n    return x.nope()\n",
        "cyclic",
    );
    // On a legal, ACYCLIC, shared DAG — `Pi: P(i+1), P(i+2)`. Declaration alone hung `check` (and
    // so the LSP) past 25 s at n=42; the cheap `path` stack does not bound this, only `seen` does.
    let n = 42;
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("protocol P{i}:\n"));
        let (a, b) = (i + 1, i + 2);
        if a < n {
            src.push_str(&format!("    P{a}\n"));
        }
        if b < n {
            src.push_str(&format!("    P{b}\n"));
        }
        if a >= n && b >= n {
            src.push_str(&format!("    fn m{i}(self) -> int\n"));
        }
    }
    src.push_str("fn g[T: P0](t: T) -> int:\n    return 1\n");
    ok(&src);
}

/// An embed's type argument naming a type that does not exist resolves to `Ty::Unknown`, and an
/// `Unknown` requirement accepts EVERY operand — `"oops" in b` type-checked on a `Bag` whose
/// `contains` takes an `int`, then faulted at runtime. Same Unknown-as-permissive hazard as the
/// nested case, reached by a typo instead of a nesting, so it is the same hard error.
#[test]
fn an_unknown_type_in_an_embed_arg_is_rejected() {
    rejects(
        "import Contains from std.ops\nprotocol Bag:\n    Contains[T]\nstruct B:\n    n: int\n    fn contains(self, item: int) -> bool:\n        return item + self.n > 0\nfn has(b: Bag) -> bool:\n    return \"oops\" in b\n",
        "unknown type 'T' in the type argument",
    );
    ok("import Contains from std.ops\nprotocol Bag:\n    Contains[int]\n");
}

/// An operator and its method spelling must agree. `Self` in an operator method's RETURN widens to
/// the existential through BOTH — `o[0]` used to leak the raw `Ty::Param("Self")` while
/// `o.index(0)` yielded the protocol.
#[test]
fn self_returning_operator_method_widens_through_the_operator_spelling() {
    let src = "protocol Boxy:\n    fn index(self, k: int) -> Self\n    fn tag(self) -> str\nstruct S:\n    x: int\n    fn index(self, k: int) -> S:\n        return self\n    fn tag(self) -> str:\n        return \"s\"\n";
    ok(&format!(
        "{src}fn read(o: Boxy) -> str:\n    return o[0].tag()\n"
    ));
    ok(&format!(
        "{src}fn read(o: Boxy) -> str:\n    return o.index(0).tag()\n"
    ));
}

/// An owner type param NESTED inside an embed's type argument is substituted like any other: the
/// embed arg is resolved once at the declaration, in its own protocol's scope (TICKET-202), so
/// `Bag[int]` requires `Contains[List[int]]` and `["x"] in b` is rejected.
#[test]
fn a_type_param_nested_in_an_embed_arg_is_substituted() {
    ok(
        "import Contains from std.ops\nprotocol Bag[T]:\n    Contains[List[T]]\nfn has(b: Bag[int]) -> bool:\n    return [1] in b\n",
    );
    rejects(
        "import Contains from std.ops\nprotocol Bag[T]:\n    Contains[List[T]]\nfn has(b: Bag[int]) -> bool:\n    return [\"x\"] in b\n",
        "cannot test membership of List[str]",
    );
}

/// Unary `-` on an existential whose protocol declares `neg` — routed through `satisfies`, so it
/// needs no operator-arm change of its own, only this fence.
#[test]
fn unary_neg_on_a_protocol_existential() {
    ok(
        "protocol Negish:\n    fn neg(self) -> Self\nstruct V:\n    x: int\n    fn neg(self) -> V:\n        return V(-self.x)\nfn flip(a: Negish) -> Negish:\n    return -a\nfn main():\n    print(flip(V(1)))\nmain()\n",
    );
}

/// Indexing and slicing recover `K`/`V`/`R` off a protocol existential's own args.
#[test]
fn index_and_slice_on_a_protocol_existential() {
    ok(
        "import Index from std.ops\nstruct S:\n    xs: List[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\nfn read(o: Index[int, int]) -> int:\n    return o[0]\nfn main():\n    print(read(S([7])))\nmain()\n",
    );
    ok(
        "import Slice from std.ops\nstruct S:\n    xs: List[int]\n    fn slice(self, r: Range) -> List[int]:\n        return self.xs\nfn cut(o: Slice[List[int]]) -> List[int]:\n    return o[0..1]\nfn main():\n    print(cut(S([7])))\nmain()\n",
    );
    // The key type still has to match.
    rejects(
        "import Index from std.ops\nstruct S:\n    xs: List[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\nfn read(o: Index[int, int]) -> int:\n    return o[\"a\"]\n",
        "index must be int, found str",
    );
}

/// Drift guard (editor hover): EVERY builtin callable must have a `builtin_sig` entry, so a future
/// builtin can't silently lose hover. The set is derived (`builtin_callables`): the `PRELUDE` rows
/// and the builtin types with an opcode constructor (`Channel`, `Shared`, `Executor`, …).
#[test]
fn builtin_callables_all_have_builtin_sig() {
    // `builtin_sig` is now a Checker METHOD (phase 3a): the nine migrated universe builtins source
    // their sig from the always-linked std/prelude.chz, seeded here via `seed_native_prelude_sigs`
    // (the single-module `check` path does the same). `print` + the container/runtime ctors stay
    // hard-coded, so they resolve without the seed too.
    let mut c = Checker::new();
    c.seed_native_prelude_sigs();
    for name in builtin_callables() {
        assert!(
            c.builtin_sig(name).is_some(),
            "reserved callable builtin '{name}' has no builtin_sig entry → it would lose editor hover"
        );
    }
    // Sanity: a non-reserved name has no builtin sig (no accidental over-coverage).
    assert!(c.builtin_sig("Ok").is_none());
    assert!(c.builtin_sig("totally_not_a_builtin").is_none());
}

/// Native-prelude DRIFT GUARD: [`Builtin`] is the one table of the builtins a bare name reaches.
/// The checker's views (`prelude_fn`, `is_firstclass_builtin_fn`, `builtin_sig`) and the
/// `std/prelude.chz` `native` decls must agree with it.
#[test]
fn prelude_table_is_single_source_of_truth() {
    use std::collections::BTreeSet;

    // (1) The first-class builtins are the four universe fns.
    let firstclass: BTreeSet<&str> = Builtin::ALL
        .into_iter()
        .filter(|b| b.first_class())
        .map(Builtin::name)
        .collect();
    assert_eq!(firstclass, BTreeSet::from(["print", "ord", "chr", "panic"]));
    for name in builtin_callables() {
        assert_eq!(
            is_firstclass_builtin_fn(name),
            prelude_fn(name).is_some_and(Builtin::first_class),
            "'{name}': is_firstclass_builtin_fn must be the table's first_class view"
        );
    }
    assert!(!is_firstclass_builtin_fn("List"));
    assert!(prelude_fn("int").is_some() && !is_firstclass_builtin_fn("int"));
    // A builtin with an allocation opcode is reached through its `BuiltinType`, never the prelude.
    assert!(prelude_fn("Channel").is_none());

    // (2) Every prelude builtin has a checker `builtin_sig` (editor hover, value-position typing).
    let mut chk = Checker::new();
    chk.seed_native_prelude_sigs();
    let prelude: Vec<Builtin> = Builtin::ALL
        .into_iter()
        .filter(|b| b.lowering() != Lowering::Opcode)
        .collect();
    for b in &prelude {
        assert!(
            chk.builtin_sig(b.name()).is_some(),
            "'{}' has no builtin_sig",
            b.name()
        );
    }

    // (3) The `std/prelude.chz` native decls: a `native fn` is first-class, a `native ctor` is not,
    //     and the decls are every prelude builtin but the three generic container ctors (a native
    //     decl cannot state their generic identity).
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let graph =
        crate::resolver::build_graph(&entry).expect("resolve should always-link std.prelude");
    let prelude_mod = graph
        .modules
        .iter()
        .find(|m| m.dotted == ["std", "prelude"])
        .expect("std.prelude must be always-linked into the graph");
    let mut parsed_names: BTreeSet<&str> = BTreeSet::new();
    for s in &prelude_mod.ast.stmts {
        if let StmtKind::Native(d) = &s.kind {
            let b =
                prelude_fn(&d.name).unwrap_or_else(|| panic!("'{}' is no prelude builtin", d.name));
            let fc = matches!(d.kind, crate::ast::NativeKind::Fn);
            assert_eq!(
                b.first_class(),
                fc,
                "'{}': native fn <=> first_class",
                d.name
            );
            parsed_names.insert(b.name());
        }
    }
    let want: BTreeSet<&str> = prelude
        .iter()
        .map(|b| b.name())
        .filter(|n| !["List", "Map", "Set"].contains(n))
        .collect();
    assert_eq!(parsed_names, want);

    // Each parsed FnSig must equal its HISTORICAL hand-built shape (byte-identical migration). Read the
    // sigs through a prelude-seeded checker (same path production uses).
    let mut sc = Checker::new();
    sc.seed_native_prelude_sigs();
    let hist: &[(&str, Vec<Ty>, Ty)] = &[
        ("ord", vec![Ty::Str], Ty::Int),
        ("chr", vec![Ty::Int], Ty::Str),
        ("panic", vec![Ty::Str], Ty::Unknown),
        ("int", vec![Ty::Unknown], Ty::Int),
        ("float", vec![Ty::Unknown], Ty::Float),
        ("str", vec![Ty::Unknown], Ty::Str),
        ("bytes", vec![Ty::Unknown], Ty::Bytes),
        ("bytearray", vec![Ty::Unknown], Ty::ByteArray),
    ];
    for (name, params, ret) in hist {
        let sig = sc
            .builtin_sig(name)
            .unwrap_or_else(|| panic!("'{name}' has no builtin_sig after prelude seed"));
        assert_eq!(
            &sig.params, params,
            "'{name}' params drifted from historical sig"
        );
        assert_eq!(&sig.ret, ret, "'{name}' return drifted from historical sig");
    }
}

/// Phase 3a — a `native fn`/`native ctor` decl is PRELUDE/STD-ONLY: in a user (non-stdlib) module it
/// is a clear checker error (a user can't bind a name to a nonexistent intrinsic — a footgun guard).
#[test]
fn native_decl_in_user_file_rejected() {
    rejects(
        "native fn foo(x: int) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native ctor bar(x) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
    // Phase 4f — flipping parse_native to parse_params(true) makes a DEFAULTED native-fn param PARSE
    // in a user file too; the checker's stdlib-only hoist rejection must still fire (the default never
    // matters — the decl itself is rejected). Guards against the parser-strictness relaxation
    // weakening the user-file guard.
    rejects(
        "native fn baz(x: int = 0) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
}

/// Phase 4a — a `native struct` decl is likewise PRELUDE/STD-ONLY: in a user (non-stdlib) module it is
/// a clear checker error (a user can't declare a native type whose layout the runtime doesn't know).
#[test]
fn native_struct_in_user_file_rejected() {
    rejects(
        "native struct Foo:\n    a: int\n",
        "native struct declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native struct Foo:\n    a: int\n",
        "native struct declarations are only allowed in standard-library modules",
    );
}

/// Phase 5b — a `native enum` decl is likewise PRELUDE/STD-ONLY: in a user (non-stdlib) module it is a
/// clear checker error (a user can't declare a reserved builtin enum's variant shape). The ONLY native
/// enums are `std/prelude.chz`'s `Option`/`Result`.
#[test]
fn native_enum_in_user_file_rejected() {
    rejects(
        "native enum Foo:\n    A\n    B\n",
        "native enum declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native enum Foo:\n    A\n    B\n",
        "native enum declarations are only allowed in standard-library modules",
    );
}

/// TICKET-218 — `std/ffi.chz`'s `native type` decls are the whole type import surface of `std.ffi`:
/// every declared name imports, an undeclared width does not, and `ptr` stays opaque.
#[test]
fn ffi_declared_types_are_the_import_surface() {
    let names: Vec<&str> = crate::native::ffi::declared_types()
        .iter()
        .map(|d| d.name.as_str())
        .collect();
    assert!(names.contains(&"uint16"), "declared: {names:?}");
    for n in names {
        let errs = check_entry(&format!("import {n} from std.ffi\n"));
        assert!(errs.is_empty(), "import {n} from std.ffi: {errs:?}");
    }
    rejects_entry("import int128 from std.ffi\n", "int128");
    rejects_entry("import ptr from std.ffi\np: ptr = 5\n", "ptr");
}

/// TICKET-218 — a native module's constants are `NAME: const T = value` lets in its std file,
/// harvested into `ModuleSig::members` as const values (no Rust const table).
#[test]
fn native_module_consts_come_from_the_file() {
    let math = native_module_sig_via_graph("math");
    for c in ["pi", "e", "inf", "nan"] {
        let m = math
            .members
            .get(c)
            .unwrap_or_else(|| panic!("math.{c} missing"));
        assert_eq!(m.ty, Ty::Float, "math.{c}");
        assert!(m.is_const, "math.{c} must be const");
        assert!(m.certain_fn.is_none(), "math.{c} is a value");
    }
    rejects_entry(
        "import std.math\nmath.pi = 1.0\n",
        "it is declared const in module 'math'",
    );
}

/// TICKET-218 — a native std file's top-level binding must be a typed const let; a `:=` or a
/// non-const typed let is rejected by the harvest, a const let is not.
#[test]
fn native_module_mutable_global_rejected() {
    for (src, bad) in [
        ("x := 1\n", true),
        ("x: int = 1\n", true),
        ("x: const int = 1\n", false),
    ] {
        let ast = crate::parser::parse(crate::lexer::tokenize(src).unwrap()).unwrap();
        let mut c = Checker::new();
        let mut sig = ModuleSig::default();
        c.harvest_native_module(&ast, &mut sig);
        let hit = c.errors.iter().any(|e| {
            e.message.contains(
                "a standard-library module's top-level binding must be NAME: const T = value",
            )
        });
        assert_eq!(hit, bad, "{src:?}: {:?}", c.errors);
        assert_eq!(sig.members.contains_key("x"), !bad, "{src:?}");
    }
}

/// TICKET-218 — a C width is a tag on a SLOT type (`Ty::Width`): every value read through it is the
/// plain scalar, so arithmetic, comparison, formatting, indexing, iteration and map keys work as on
/// `int`/`float`. The width shows in a slot's diagnostics, and two embedded protocols that disagree
/// on a param's width conflict (Go: `duplicate method M`).
#[test]
fn width_ty_is_a_tag_on_its_scalar() {
    let prefix = "import std.ffi\nimport int8, int32, uint32, float32 from std.ffi\n";
    // TICKET-262: a width type is legal only in a C signature or a struct field's own type, so a
    // body that writes one anywhere else is a rule-1 error; a struct field and an extern stay clean.
    for body in [
        "struct P:\n    a: int8\np := P(1)\nq := p.a + 1\n",
        "extern \"libc.so.6\":\n    fn abs(x: int32) -> int32\nprint(abs(-3) + 1)\n",
    ] {
        let errs = check_entry(&format!("{prefix}{body}"));
        assert!(errs.is_empty(), "{body:?}: {errs:?}");
    }
    for body in [
        "x: int8 = 5\ny: int = x + 1\nb := x < y\ns := \"{x:4d}\"\nt := str(x)\n",
        "l: List[int8] = [1, 2]\nn: int = l[0] + 1\nfor v in l:\n    n = n + v\n",
        "m: Map[uint32, str] = {1: \"a\"}\nprint(m[1])\n",
        "fn f(x: int8) -> int8:\n    return x\ny := f(3) + 1\n",
        "g: float32 = 1.5\nh := g + 1.0\n",
        "struct Box[T]:\n    v: T\nbx: Box[int8] = Box(1)\nw := bx.v + 1\n",
        "x: int8 = \"a\"\n",
        "x: int8 = 5\ny := x\ny = \"s\"\n",
        "protocol A:\n    fn m(self, x: int8) -> None\nprotocol B:\n    fn m(self, x: int) -> None\nprotocol C:\n    A + B\n",
    ] {
        rejects_entry(&format!("{prefix}{body}"), "describes the C boundary only");
    }
}

/// TICKET-218 — a width NESTED in a slot type (a payload, a tuple element, a list element) reads as
/// its scalar too: a literal or range sub-pattern matches it, and `bytes`/`bytearray`/`extend` take a
/// `List[<w>]` as a `List[int]`. Each program checks clean on base, where a width resolved to `int`.
#[test]
fn nested_width_reads_as_its_scalar() {
    let prefix = "import std.ffi\nimport int8, uint8 from std.ffi\n";
    // TICKET-262: only the struct field is a C position; every other body writes a width type under
    // a payload, an optional, a carrier, a tuple or a list, which is a rule-1 error.
    let body = "struct P:\n    a: int8\n    b: int\np := P(1, 2)\nmatch p:\n    P(1, b): print(b)\n    _: print(0)\n";
    let errs = check_entry(&format!("{prefix}{body}"));
    assert!(errs.is_empty(), "{body:?}: {errs:?}");
    for body in [
        "enum E:\n    A(int8)\ne := E.A(1)\nmatch e:\n    E.A(1): print(1)\n    E.A(2..5): print(2)\n    E.A(_): print(3)\n",
        "o: int8? = ?1\nmatch o:\n    ?1: print(1)\n    ?2..5: print(2)\n    _: print(3)\n",
        "r: int8!str = ?1\nmatch r:\n    ?1: print(1)\n    ?_: print(2)\n    !e: print(e)\n",
        "t: (int8, int) = (1, 2)\nmatch t:\n    (1, _): print(1)\n    _: print(2)\n",
        "l: List[uint8] = [1, 2]\nb := bytes(l)\n",
        "l: List[uint8] = [1, 2]\nba := bytearray(l)\nba.extend(l)\n",
    ] {
        rejects_entry(&format!("{prefix}{body}"), "describes the C boundary only");
    }
    // Must still fail: a non-int literal or a non-int element is still rejected, naming the slot.
    rejects_entry(
        &format!("{prefix}o: int8? = ?1\nmatch o:\n    ?\"a\": print(1)\n    _: print(2)\n"),
        "describes the C boundary only",
    );
    rejects_entry(
        &format!("{prefix}o: str? = ?\"a\"\nmatch o:\n    ?1..3: print(1)\n    _: print(2)\n"),
        "range pattern cannot match a value of type str",
    );
    rejects_entry(
        &format!("{prefix}l: List[str] = [\"a\"]\nb := bytes(l)\n"),
        "bytes() expects a bytes, a bytearray, or a List[int], got List[str]",
    );
    rejects_entry(
        &format!("{prefix}l: List[float] = [1.0]\nba := bytearray(l)\n"),
        "got List[float]",
    );
    rejects_entry(
        &format!("{prefix}ba := bytearray(1)\nl: List[str] = [\"a\"]\nba.extend(l)\n"),
        "extend() expects a bytes, a bytearray, or a List[int], got List[str]",
    );
}

/// TICKET-218 — a constant outside a C width is a compile error at every value-into-slot site, type
/// arguments, literals and branches included; its in-range neighbours check clean. Go, run 2026-10-06:
/// `cannot use 300 (untyped int constant) as int8 value in variable declaration (overflows)`, and the
/// same for argument, return, array/slice, map and struct literals; Rust: `literal out of range for
/// `i8`` for `Some(300)` and `if c { 300 } else { 1 }`.
#[test]
fn ffi_width_constant_grid() {
    use crate::native::cffi::{CType, width_ctype};
    // `{w}` is the width, `{k}` the constant. TICKET-262: a width is legal at the C positions only (an
    // extern signature, a struct field's own type), and the constant meets its slot there.
    let c_sites: &[&str] = &[
        "extern \"libc.so.6\":\n    fn idw(x: {w}) -> {w}\nidw({k})\n",
        "struct S:\n    f: {w}\ns := S({k})\n",
        "struct S:\n    f: {w}\ns := S(0)\ns.f = {k}\n",
        "struct S:\n    f: {w} = {k}\n",
        "ffi.store_{w}(ffi.null(), {k})\n",
    ];
    // Every other position rejects the width type itself, whatever the constant.
    let sites: &[&str] = &[
        "fn f(x: {w}) -> None:\n    pass\nf({k})\n",
        "struct S:\n    n: int\n    fn m(self, x: {w}) -> None:\n        pass\ns := S(1)\ns.m({k})\n",
        "protocol P:\n    fn m(self, x: {w}) -> None\nfn h(q: P) -> None:\n    q.m({k})\n",
        "protocol P:\n    fn m(self, x: {w}) -> None\nfn g[T: P](t: T) -> None:\n    t.m({k})\n",
        "enum E:\n    A({w})\ne := E.A({k})\n",
        "enum G[T]:\n    B({w}, T)\ng := G.B({k}, 1)\n",
        "l: List[{w}] = [0]\nl[0] = {k}\n",
        "x: {w} = {k}\n",
        "x: {w} = 0\nx = {k}\n",
        "fn g(x: {w} = {k}) -> None:\n    pass\n",
        "fn setg() -> None:\n    G = {k}\nG: {w} = 0\n",
        "fn r() -> {w}:\n    return {k}\n",
        "fn r() -> {w}: {k}\n",
        "import Iterator from std.iter\nfn gen() -> Iterator[{w}]:\n    yield {k}\n",
        "c := fn() -> {w}: {k}\n",
        "c := fn(x: {w}) -> int: 0\nc({k})\n",
        "l: List[{w}] = [{k}]\n",
        "l: List[{w}] = []\nl.push({k})\n",
        "m: Map[str, {w}] = {\"a\": {k}}\n",
        "t: ({w}, int) = ({k}, 1)\n",
        "o: {w}? = ?({k})\n",
        "r: {w}!str = ?({k})\n",
        "struct Box[T]:\n    v: T\nb := Box[{w}]({k})\n",
        "l := List[{w}]([{k}])\n",
        "m := Map[str, {w}]([(\"a\", {k})])\n",
        "c := true\nx: {w} = if c: {k} else: 0\n",
        "c := true\nx: {w} = if c: 0 else: {k}\n",
        "n := 1\nx: {w} = match n:\n    1: {k}\n    _: 0\n",
        "o: int? = None\nx: {w} = o ?? {k}\n",
    ];
    // Integer-only: `Set` (a float is not Hashable). A compound operand meets no slot (TICKET-258).
    let int_sites: &[&str] = &["s := Set[{w}]([{k}])\n"];
    let mut red: Vec<String> = Vec::new();
    for d in crate::native::ffi::declared_types() {
        if d.scalar.is_none() {
            continue;
        }
        let w = d.name.as_str();
        let ct = width_ctype(w).unwrap();
        let fail = |k: &str| format!("constant {k} does not fit {w} {}", ct.range_text());
        // (constant text, expected rejection needle or None for clean)
        let ks: Vec<(String, Option<String>)> = match &ct {
            CType::Float32 => vec![
                ("3.4028234663852886e38".into(), None),
                ("1e39".into(), Some(fail("1e39"))),
                ("-1e39".into(), Some(fail("-1e39"))),
            ],
            CType::Int64 => vec![
                ("-9223372036854775808".into(), None),
                ("9223372036854775807".into(), None),
            ],
            CType::UInt64 => vec![
                ("0".into(), None),
                ("9223372036854775807".into(), None),
                ("-1".into(), Some(fail("-1"))),
            ],
            _ => {
                let (lo, hi) = ct.int_range().unwrap();
                vec![
                    (lo.to_string(), None),
                    (hi.to_string(), None),
                    ((lo - 1).to_string(), Some(fail(&(lo - 1).to_string()))),
                    ((hi + 1).to_string(), Some(fail(&(hi + 1).to_string()))),
                ]
            }
        };
        let float = ct == CType::Float32;
        let all: Vec<(&&str, bool)> = c_sites
            .iter()
            .map(|s| (s, true))
            .chain(
                sites
                    .iter()
                    .chain(int_sites.iter().filter(|_| !float))
                    .map(|s| (s, false)),
            )
            .collect();
        for (site, at_c) in all {
            for (k, want) in &ks {
                let rule1 = Some("describes the C boundary only".to_string());
                let want = if at_c { want } else { &rule1 };
                // A float site with a `0` default/branch needs a float zero.
                let body = site.replace("{w}", w).replace("{k}", k);
                let body = if float {
                    body.replace("= 0\n", "= 0.0\n")
                        .replace("else: 0\n", "else: 0.0\n")
                        .replace("if c: 0 else", "if c: 0.0 else")
                        .replace("_: 0\n", "_: 0.0\n")
                        .replace("[0]\n", "[0.0]\n")
                        .replace("S(0)\n", "S(0.0)\n")
                        .replace("int?", "float?")
                } else {
                    body
                };
                let src = format!("import std.ffi\nimport {w} from std.ffi\n{body}");
                let errs = check_entry(&src);
                match want {
                    None if !errs.is_empty() => {
                        red.push(format!("{src}\n  want clean, got {errs:?}"))
                    }
                    Some(n) if !errs.iter().any(|e| e.message.contains(n.as_str())) => {
                        red.push(format!("{src}\n  want {n:?}, got {errs:?}"))
                    }
                    _ => {}
                }
            }
        }
    }
    // A qualified ctor across modules, and a local inferred from a width value stays a plain int.
    let two = [
        (
            "main.chz",
            "import lib\nimport int8 from std.ffi\ns := lib.S(300)\n",
        ),
        (
            "lib.chz",
            "import int8 from std.ffi\nstruct S:\n    f: int8\n",
        ),
    ];
    let errs = check_files(&two);
    if !errs.iter().any(|e| {
        e.message
            .contains("constant 300 does not fit int8 (-128..127)")
    }) {
        red.push(format!("qualified ctor: got {errs:?}"));
    }
    let errs = check_entry("import int8 from std.ffi\nx: int8 = 5\ny := x\ny = 1000\n");
    if !errs
        .iter()
        .any(|e| e.message.contains("describes the C boundary only"))
    {
        red.push(format!("inferred local: got {errs:?}"));
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// TICKET-218 — a `native type` decl is STD-ONLY like `native struct`/`native enum`.
#[test]
fn native_type_is_std_only() {
    rejects(
        "native type t = int\n",
        "native type declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native type t\n",
        "native type declarations are only allowed in standard-library modules",
    );
}

/// The prelude's `native enum Option[T]` / `native enum Result[T, E]` decls ARE the registered
/// carrier enums (TICKET-229): harvested and seeded, `variants_of` answers the declared shape for
/// the carrier types from the ordinary enum tables, with no method.
#[test]
fn native_enum_option_result_register_as_ordinary_enums() {
    let path = crate::resolver::std_root().join("prelude.chz");
    let src = std::fs::read_to_string(&path).expect("read std/prelude.chz");
    let toks = crate::lexer::tokenize(&src).expect("tokenize prelude");
    let module = crate::parser::parse(toks).expect("parse prelude");
    let mut c = Checker::new();
    c.current_module_is_stdlib = true;
    c.carrier_seeds = c.harvest_native_enums(&module);
    c.seed_carrier_enums();
    let t = || Ty::Param("T".to_string());
    let e = || Ty::Param("E".to_string());
    assert_eq!(
        c.enums["Option"],
        vec!["Some".to_string(), "None".to_string()]
    );
    assert_eq!(c.enums["Result"], vec!["Ok".to_string(), "Err".to_string()]);
    assert!(c.enum_methods["Option"].is_empty() && c.enum_methods["Result"].is_empty());
    let opt = c.variants_of(&Ty::option(t())).expect("Option variants");
    assert_eq!(
        opt,
        HashMap::from([
            ("Some".to_string(), vec![t()]),
            ("None".to_string(), vec![])
        ])
    );
    let res = c
        .variants_of(&Ty::result_e(t(), e()))
        .expect("Result variants");
    assert_eq!(
        res,
        HashMap::from([
            ("Ok".to_string(), vec![t()]),
            ("Err".to_string(), vec![e()])
        ])
    );
}

/// Phase 3a — the migrated builtins keep their historical first-classness through the `.chz` decls.
/// `native fn` (ord/chr/panic) stays a first-class VALUE (binds to a name, types `Ty::Func`);
/// `native ctor` (int/str/…) stays NON-first-class (value-position use is a checker error). This is
/// the graph path (prelude always-linked), complementing the single-module `ok()`-based tests.
#[test]
fn native_prelude_firstclassness_preserved_on_graph_path() {
    entry_ok("fn w():\n    f := ord\n    print(f(\"a\"))\nw()\n");
    entry_ok("fn w():\n    p := panic\n    p(\"boom\")\nw()\n");
    // A `native ctor` (scalar constructor) is NOT first-class — value-position use is rejected
    // (same fall-through path as `f := List`; the message names the offending `int`).
    entry_rejects("fn w():\n    f := int\nw()\n", "int");
}

/// TICKET-160 — `Map.items()` and a shallow `copy()` on `List`/`Map`/`Set` type-check with the
/// CPython-shaped result types.
#[test]
fn container_items_and_copy_type_check() {
    ok(r#"
m := {"a": 1}
ps: List[(str, int)] = m.items()
xs := [1, 2]
ys: List[int] = xs.copy()
s := {1, 2}
t: Set[int] = s.copy()
n: Map[str, int] = m.copy()
"#);
}

/// Phase 5a-containers BEHAVIOR-PRESERVING GUARD: every `List`/`Map`/`Set` method sig harvested from
/// `std/prelude.chz`'s `native struct` decls (looked up via `native_handle_method` with the value's
/// element/key/value type substituted) must BYTE-MATCH the sig the retired bespoke `list_method_sig`/
/// `map_method_sig`/`set_method_sig` arms produced. `Map` uses distinct `Int`/`Str` for K/V so a K↔V
/// swap in the declaration order would be caught. A mismatch here is a behavior change (the whole port
/// is behavior-preserving), so this pins the ported surface exactly.
#[test]
fn container_method_sigs_byte_match() {
    let c = prelude_container_checker();
    // Assert one method's harvested+substituted sig equals the hand-written expected (params/ret/
    // min_params) — the retired-arm shape. `Map` uses distinct `Int`/`Str` for K/V so a K↔V swap in the
    // declaration order would be caught.
    let chk = |ty: &str, method: &str, targs: &[Ty], exp_params: Vec<Ty>, exp_ret: Ty| {
        let sig = c
            .native_handle_method(ty, method, targs)
            .unwrap_or_else(|| panic!("{ty}.{method} must resolve via the harvested table"));
        assert_eq!(sig.params, exp_params, "{ty}.{method} params");
        assert_eq!(sig.ret, exp_ret, "{ty}.{method} ret");
        assert_eq!(
            sig.min_params,
            exp_params.len(),
            "{ty}.{method} min_params (all required)"
        );
    };
    let li = || Ty::list(Ty::Int);
    let int = || vec![Ty::Int];
    let kv = || vec![Ty::Int, Ty::Str]; // Map targs: K=Int, V=Str.
    // List[Int] — the 9 flat methods (map/filter/fold/sort/sort_by/sort_by_key stay residual).
    chk("List", "len", &int(), vec![], Ty::Int);
    chk("List", "push", &int(), vec![Ty::Int], Ty::Nil);
    chk("List", "pop", &int(), vec![], Ty::option(Ty::Int));
    chk("List", "reverse", &int(), vec![], Ty::Nil);
    chk("List", "contains", &int(), vec![Ty::Int], Ty::Bool);
    chk("List", "index_of", &int(), vec![Ty::Int], Ty::Int);
    chk("List", "concat", &int(), vec![li()], li());
    chk("List", "extend", &int(), vec![li()], Ty::Nil);
    chk("List", "sum", &int(), vec![], Ty::Int);
    chk("List", "copy", &int(), vec![], li());
    // `sort` is now file-backed (`native fn sort(self) -> nil where T: Comparable`): it resolves via
    // the harvested table with a nil return, and carries a `where T: Comparable` bound (enforced at
    // the call site by the Ty::List arm's `enforce_bounds`).
    chk("List", "sort", &int(), vec![], Ty::Nil);
    let sort_sig = c.native_handle_method("List", "sort", &int()).unwrap();
    assert_eq!(
        sort_sig.where_bounds.len(),
        1,
        "List.sort carries one where bound"
    );
    assert_eq!(sort_sig.where_bounds[0].name, "T");
    assert_eq!(sort_sig.where_bounds[0].bounds[0].name, "Comparable");
    // `sum` gains a `where T: Add` annotation (documentation; the numeric check-gate is the real
    // enforcement — see `list_sum_struct_with_add_still_rejected_at_check`).
    let sum_sig = c.native_handle_method("List", "sum", &int()).unwrap();
    assert_eq!(
        sum_sig.where_bounds.len(),
        1,
        "List.sum carries one where bound"
    );
    assert_eq!(sum_sig.where_bounds[0].bounds[0].name, "Add");
    // Map[Int, Str] — K=Int, V=Str (distinct so the K/V order is pinned).
    chk("Map", "len", &kv(), vec![], Ty::Int);
    chk("Map", "contains", &kv(), vec![Ty::Int], Ty::Bool);
    chk("Map", "get", &kv(), vec![Ty::Int], Ty::option(Ty::Str));
    chk("Map", "keys", &kv(), vec![], Ty::list(Ty::Int));
    chk("Map", "values", &kv(), vec![], Ty::list(Ty::Str));
    chk("Map", "remove", &kv(), vec![Ty::Int], Ty::option(Ty::Str));
    chk(
        "Map",
        "merge",
        &kv(),
        vec![Ty::map(Ty::Int, Ty::Str)],
        Ty::map(Ty::Int, Ty::Str),
    );
    chk(
        "Map",
        "update",
        &kv(),
        vec![Ty::map(Ty::Int, Ty::Str)],
        Ty::Nil,
    );
    chk(
        "Map",
        "items",
        &kv(),
        vec![],
        Ty::list(Ty::Tuple(vec![Ty::Int, Ty::Str])),
    );
    chk("Map", "copy", &kv(), vec![], Ty::map(Ty::Int, Ty::Str));
    // Set[Int] — the flat methods.
    chk("Set", "len", &int(), vec![], Ty::Int);
    chk("Set", "contains", &int(), vec![Ty::Int], Ty::Bool);
    chk("Set", "add", &int(), vec![Ty::Int], Ty::Nil);
    chk("Set", "remove", &int(), vec![Ty::Int], Ty::Bool);
    chk(
        "Set",
        "union",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    chk(
        "Set",
        "intersection",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    chk(
        "Set",
        "difference",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    chk("Set", "copy", &int(), vec![], Ty::set(Ty::Int));
    // `map`/`filter`/`fold`/`sort_by`/`sort_by_key` are now file-backed too (the closure-return
    // loop-back landed), so they resolve via the harvested table. `map`/`fold`/`sort_by_key` carry
    // their own `[U]`/`[K]` type param (routed through `infer_generic_method`); `filter`/`sort_by` are
    // flat. `get` is genuinely not a List method (it stays a must-not).
    let map_sig = c.native_handle_method("List", "map", &[Ty::Int]).unwrap();
    assert_eq!(map_sig.type_params.len(), 1, "List.map carries its own [U]");
    assert_eq!(map_sig.type_params[0].name, "U");
    let fold_sig = c.native_handle_method("List", "fold", &[Ty::Int]).unwrap();
    assert_eq!(fold_sig.type_params[0].name, "U");
    let sbk_sig = c
        .native_handle_method("List", "sort_by_key", &[Ty::Int])
        .unwrap();
    assert_eq!(sbk_sig.type_params[0].name, "K");
    assert_eq!(sbk_sig.type_params[0].bounds[0].name, "Comparable");
    // `filter`/`sort_by` are flat (no own type param) but still harvested (was residual).
    assert!(
        c.native_handle_method("List", "filter", &[Ty::Int])
            .is_some()
    );
    assert!(
        c.native_handle_method("List", "sort_by", &[Ty::Int])
            .is_some()
    );
    assert!(
        c.native_handle_method("List", "get", &[Ty::Int]).is_none(),
        "List.get is not a method"
    );
}

/// TICKET-278 fact 10: `std/*.chz` owns the method set of a builtin type, and `DECLARED_METHODS`
/// (generated from those decls by `src/base/build_methods.rs`) is the table every reader derives from.
/// Three cells over every declared type:
/// (a) the checker's harvested method table holds exactly the declared names, both directions (a std
///     module's published sig seals its `_` names, `ModuleSig::seal_std_internals`, so the cell
///     compares the names without a leading `_`);
/// (b) the hover (`builtin_type_doc`) lists every declared name that does not start with `_`;
/// (c) the types whose hover carries a `methods:` line, found by asking `builtin_type_doc` about every
///     type name the CHECKER knows, equal the declared types, so a declared type with no hover arm and
///     a hover methods line with no decl both go red. The VM side is its dispatchers' exhaustive
///     `match` on the generated enums, which fails the build.
#[test]
fn builtin_method_grid_declared_checked_and_hovered() {
    use crate::base::builtin_method::DECLARED_METHODS;
    use std::collections::BTreeSet;
    let cc = prelude_container_checker();
    let sigs: Vec<ModuleSig> = ["concurrency", "net", "io", "fs"]
        .iter()
        .map(|m| native_module_sig_via_graph(m))
        .collect();
    let harvested = |ty: &str| -> Option<BTreeSet<String>> {
        if let Some(s) = cc.structs.get(ty) {
            return Some(s.methods.keys().cloned().collect());
        }
        sigs.iter()
            .find_map(|sig| sig.struct_defs.get(ty))
            .map(|d| d.methods.keys().cloned().collect())
    };
    for (ty, bodyless, bodied) in DECLARED_METHODS {
        let declared: BTreeSet<String> = bodyless
            .iter()
            .chain(bodied.iter())
            .filter(|m| !m.starts_with('_'))
            .map(|m| m.to_string())
            .collect();
        let table: BTreeSet<String> = harvested(ty)
            .unwrap_or_else(|| panic!("{ty} has no harvested method table"))
            .into_iter()
            .filter(|m| !m.starts_with('_'))
            .collect();
        for m in declared.difference(&table) {
            panic!("{ty}.{m} is declared in std/ but the checker's table has no such method");
        }
        for m in table.difference(&declared) {
            panic!("{ty}.{m} is in the checker's table but not declared in std/");
        }
        let doc = builtin_type_doc(ty).unwrap_or_else(|| panic!("{ty} has no hover text"));
        for m in &declared {
            assert!(
                doc.contains(&format!("methods: {m}")) || doc.contains(&format!(", {m}")),
                "{ty}.{m} is declared but the hover does not list it: {doc}"
            );
        }
    }
    let mut known: BTreeSet<String> = cc.structs.keys().cloned().collect();
    for sig in &sigs {
        known.extend(sig.struct_defs.keys().cloned());
    }
    for extra in ["Range", "tuple", "Result", "Option", "Iterator"] {
        known.insert(extra.to_string());
    }
    let hovered: BTreeSet<String> = known
        .iter()
        .filter(|n| builtin_type_doc(n).is_some_and(|d| d.contains("\nmethods: ")))
        .cloned()
        .collect();
    let declared: BTreeSet<String> = DECLARED_METHODS
        .iter()
        .map(|(ty, _, _)| ty.to_string())
        .collect();
    let only_hover: Vec<&String> = hovered.difference(&declared).collect();
    let only_decl: Vec<&String> = declared.difference(&hovered).collect();
    assert!(
        only_hover.is_empty() && only_decl.is_empty(),
        "hover-only types {only_hover:?}, declared-only types {only_decl:?}"
    );
}

/// TICKET-179 grid 1: the receiver-write table (`crate::native::is_mutating_native_kind`, the one owner
/// the checker's `mutates_receiver` reads) is checked against BEHAVIOR. Every harvested native method
/// of List/Map/Set/bytearray/str/bytes runs on a fresh value, and "did the receiver change" must equal
/// the table. A harvested method with no sample, or a sample naming an unharvested method, also fails.
#[test]
fn receiver_write_table_matches_observed_behavior() {
    let c = prelude_container_checker();
    let rows: [(&str, &str, &[&str]); 6] = [
        (
            "List",
            "x := [3, 1, 1, 2]",
            &[
                "len()",
                "push(9)",
                "pop()",
                "reverse()",
                "contains(1)",
                "index_of(1)",
                "concat([4])",
                "extend([4])",
                "sort()",
                "sum()",
                "map(fn(a): a + 1)",
                "filter(fn(a): a > 1)",
                "fold(0, fn(acc, a): acc + a)",
                "sort_by(fn(a, b): a - b)",
                "sort_by_key(fn(a): 0 - a)",
                "min()",
                "max()",
                "min_by(fn(a): a)",
                "max_by(fn(a): a)",
                "first()",
                "last()",
                "reversed()",
                "insert(0, 9)",
                "remove_at(0)",
                "unique()",
                "dedup()",
                "chunk(2)",
                "windows(2)",
                "take_while(fn(a): a > 2)",
                "drop_while(fn(a): a > 2)",
                "count(fn(a): a == 1)",
                "position(fn(a): a == 1)",
                "copy()",
            ],
        ),
        (
            "Map",
            r#"x := {"a": 1}"#,
            &[
                "len()",
                r#"contains("a")"#,
                r#"get("a")"#,
                "keys()",
                "values()",
                r#"remove("a")"#,
                r#"merge({"b": 2})"#,
                r#"update({"c": 3})"#,
                "items()",
                "copy()",
            ],
        ),
        (
            "Set",
            "x := {1, 2}",
            &[
                "len()",
                "contains(1)",
                "add(3)",
                "remove(1)",
                "union({9})",
                "intersection({1})",
                "difference({1})",
                "copy()",
            ],
        ),
        (
            "bytearray",
            "x := bytearray()\nx.push(97)\nx.push(98)",
            &[
                "len()",
                "push(1)",
                "pop()",
                "decode()",
                r#"extend(b"c")"#,
                "copy()",
                "reverse()",
                "reversed()",
            ],
        ),
        (
            "str",
            r#"x := "a,b""#,
            &[
                "len()",
                "upper()",
                "lower()",
                "trim()",
                "message()",
                r#"split(",")"#,
                "chars()",
                r#"join(["x", "y"])"#,
                r#"starts_with("a")"#,
                r#"contains("a")"#,
                r#"ends_with("b")"#,
                "encode()",
                r#"replace("a", "z")"#,
                "repeat(2)",
                "reversed()",
                r#"pad_left(5, "*")"#,
                r#"pad_right(5, "*")"#,
                r#"index_of("b")"#,
                r#"count("a")"#,
                r#"strip_prefix("a")"#,
                r#"strip_suffix("b")"#,
                "split_lines()",
                "strip()",
                "to_int()",
                "to_float()",
                "parse_int()",
                "parse_float()",
            ],
        ),
        (
            "bytes",
            r#"x := b"ab""#,
            &["decode()", "decode_lossy()", "len()", "reversed()"],
        ),
    ];
    let mut bad: Vec<String> = Vec::new();
    for (kind, setup, calls) in rows {
        let mut harvested: Vec<String> = c
            .structs
            .get(kind)
            .unwrap_or_else(|| panic!("seeded {kind} struct"))
            .methods
            .keys()
            .cloned()
            .collect();
        harvested.sort();
        harvested.dedup();
        let sampled: Vec<&str> = calls
            .iter()
            .map(|call| call.split('(').next().unwrap_or(call))
            .collect();
        for m in &harvested {
            if !sampled.contains(&m.as_str()) {
                bad.push(format!("{kind}.{m}: harvested but has no sample"));
            }
        }
        for (call, method) in calls.iter().zip(&sampled) {
            if !harvested.iter().any(|m| m == method) {
                bad.push(format!("{kind}.{method}: sampled but not harvested"));
                continue;
            }
            let src =
                format!("{setup}\nbefore := \"{{x}}\"\nx.{call}\nprint(\"{{x}}\" != before)\n");
            let want = crate::native::is_mutating_native_kind(kind, method);
            match crate::compiler::run_for_tests::run_capture(&src) {
                Ok(out) => {
                    let got = out.trim() == "true";
                    if got != want {
                        bad.push(format!("{kind}.{method}: table {want}, observed {got}"));
                    }
                }
                Err(e) => bad.push(format!("{kind}.{method}: {}", e.message)),
            }
        }
    }
    assert!(bad.is_empty(), "receiver-write table vs behavior: {bad:#?}");
}

/// TICKET-179 grid 2, compile-time cells (the runtime cells are
/// `tests/chz/spec/airlock_crossing_grid_test.chz`). A write the parent could observe and that every
/// path reaches is rejected; a read, and a write only some paths reach (left to layer C), are ok.
#[test]
fn airlock_crossing_grid_compile_cells() {
    for write in ["xs.push(9)", "xs[0] = 9", "p.x = 9", "n += 1"] {
        rejects(
            &format!(
                "struct P:\n    x: int\nfn main():\n    xs := [1]\n    p := P(1)\n    n := 0\n    parallel:\n        spawn:\n            {write}\n    print(xs, p.x, n)\nmain()\n"
            ),
            "is this task's copy",
        );
    }
    ok(
        "struct P:\n    x: int\nfn main():\n    xs := [1]\n    p := P(1)\n    n := 0\n    parallel:\n        spawn:\n            _ := xs.len() + p.x + n\n    print(xs, p.x, n)\nmain()\n",
    );
    rejects(
        "g := 0\nfn always():\n    g = 1\nfn main():\n    parallel:\n        spawn: always()\n    print(g)\nmain()\n",
        "is this task's copy",
    );
    // Layer A declines each of these: the write is reached on some paths only.
    ok(
        "g := 0\nfn maybe(flag: bool):\n    if flag:\n        g = 1\nfn main():\n    parallel:\n        spawn: maybe(false)\n    print(g)\nmain()\n",
    );
    ok(
        "g := 0\nfn maybe(flag: bool):\n    if not flag:\n        return\n    g = 1\nfn main():\n    parallel:\n        spawn: maybe(false)\n    print(g)\nmain()\n",
    );
    ok(
        "g := 0\nfn step(o: int?) -> int?:\n    v := o?\n    g = v\n    return ?v\nfn main():\n    parallel:\n        spawn: _ := step(None)\n    print(g)\nmain()\n",
    );
    ok(
        "g: List[int] = []\nfn each(xs: List[int]):\n    for x in xs:\n        g.push(x)\nfn main():\n    parallel:\n        spawn: each([])\n    print(g)\nmain()\n",
    );
    let w = "g := 0\nfn w() -> bool:\n    g = 1\n    return true\nfn wi() -> int:\n    g = 1\n    return 1\n";
    let spawned = "fn main():\n    parallel:\n        spawn: f(false)\n    print(g)\nmain()\n";
    for body in [
        "    _ := flag and w()",
        "    _ := flag or w()",
        "    o: int? = ?1\n    _ := o ?? wi()",
        "    while flag:\n        g = 1",
        "    match flag:\n        true: g = 1\n        false: print(1)",
    ] {
        ok(&format!("{w}fn f(flag: bool):\n{body}\n{spawned}"));
    }
    // Must still fail: a write after a conditional, and a writer in the first `if` condition, run on
    // every path.
    for body in [
        "    if flag:\n        print(1)\n    g = 1",
        "    if w():\n        print(1)",
    ] {
        rejects(
            &format!("{w}fn f(flag: bool):\n{body}\n{spawned}"),
            "is this task's copy",
        );
    }
}

/// Drift guard (editor hover, Tier C): every `(module, fn)` named in an authored module-fn doc slice
/// MUST exist in that module's `native_module_sig`, so the module-fn hover doc can only annotate a
/// function that is really exported. A renamed/removed native fn fails here.
#[test]
fn module_fn_docs_all_resolve() {
    for (module, docs) in crate::checker::graph_pass::MODULE_FN_DOCS {
        // Build the EFFECTIVE sig via the graph: math/io/os are file-backed (phase 4d), so their fns
        // are harvested from `std/<M>.chz` and the docs are re-attached by `attach_native_module_metadata`
        // — the graph is the only place their sig exists.
        let bare = module.strip_prefix("std.").unwrap_or(module);
        let sig = native_module_sig_via_graph(bare);
        for (fname, doc) in *docs {
            let f = sig.certain_fn(fname).unwrap_or_else(|| {
                panic!("{module} doc slice lists fn '{fname}' but the module has no such function")
            });
            assert_eq!(
                f.doc.as_deref(),
                Some(*doc),
                "{module}.{fname} hover doc not attached after phase-4d migration"
            );
        }
    }
}

/// The builtin × operator-protocol grid, written out (the table `builtin_arith_kind` held before it
/// was derived from `HOOKS` + `INTRINSIC_PROTO_METHODS`). One row per receiver kind.
#[test]
fn builtin_operator_protocol_grid() {
    const ARITH: [&str; 11] = [
        "Add", "Sub", "Mul", "Div", "Mod", "Neg", "BitAnd", "BitOr", "BitXor", "Shl", "Shr",
    ];
    let grid: Vec<(Ty, &[&str])> = vec![
        (Ty::Int, &ARITH),
        (Ty::Float, &["Add", "Sub", "Mul", "Div", "Mod", "Neg"]),
        (Ty::Str, &["Add"]),
        (Ty::Bytes, &["Add"]),
        (Ty::ByteArray, &["Add"]),
        (Ty::List(Box::new(Ty::Int)), &["Add"]),
        (
            Ty::Set(Box::new(Ty::Int)),
            &["Sub", "BitAnd", "BitOr", "BitXor"],
        ),
        (Ty::Bool, &[]),
        (Ty::Map(Box::new(Ty::Str), Box::new(Ty::Int)), &[]),
        (Ty::Tuple(vec![Ty::Int, Ty::Int]), &[]),
        (Ty::Option(Box::new(Ty::Int)), &[]),
        (Ty::Nil, &[]),
    ];
    for (ty, yes) in &grid {
        for p in ARITH {
            assert_eq!(
                Checker::builtin_arith_kind(p, ty),
                yes.contains(&p),
                "{ty} {p}"
            );
        }
        // A non-arithmetic protocol never passes through this table, even with a grant row.
        for p in ["Contains", "Index", "Eq", "Comparable", "Iterable"] {
            assert!(!Checker::builtin_arith_kind(p, ty), "{ty} {p}");
        }
    }
}

/// Every `Hook` has exactly one `HOOKS` row, which `Hook::row` relies on.
#[test]
fn every_hook_has_one_row() {
    for row in HOOKS {
        assert_eq!(
            HOOKS.iter().filter(|r| r.hook == row.hook).count(),
            1,
            "{:?}",
            row.hook
        );
    }
    use crate::ast::BinaryOp as B;
    for op in [B::Add, B::Lt, B::Eq, B::In, B::Shr] {
        let hook = Hook::of_binary(op).expect("an operator hook");
        assert_eq!(binary_protocol(op), Some(hook.row().protocol));
    }
    assert_eq!(binary_protocol(B::And), None);
}

/// TICKET-269: `bracket_is_index` probes a member head's receiver and the call path infers it again,
/// so a chain of n `recv.name[T](args)` links infers the first receiver 2^n times. The probe infers
/// each value receiver once, so 12 links cost at most twice 6 links plus a constant. Counted, not
/// timed (DEC-233: a growth claim counts the receivers `bracket_is_index` infers).
#[test]
fn type_arg_call_chain_checks_in_linear_time() {
    let receivers = |links: usize| {
        let chain = format!("Box(0){}", ".map[int](inc)".repeat(links));
        let src = format!(
            "struct Box:\n    v: int\n    fn map[U](self, f: fn(int) -> int) -> Box:\n        return Box(f(self.v))\nfn inc(x: int) -> int:\n    return x + 1\nfn main():\n    print({chain}.v)\n"
        );
        let tokens = lexer::tokenize(&src).expect("lex should succeed");
        let module = parser::parse(tokens).expect("parse should succeed");
        let (res, receivers) = check_counting_bracket_receivers(&module);
        assert!(res.is_ok(), "{links} links: {res:?}");
        receivers
    };
    let (short, long) = (receivers(6), receivers(12));
    assert!(
        long <= 2 * short + 4,
        "a 6-link recv.name[T](args) chain inferred {short} receivers, a 12-link one {long}"
    );
}

/// TICKET-269: a member call head's value receiver is inferred once and both bracket readings take
/// it. Receiver kind (bare name, call value) x reading (type argument, index), plus the two error
/// cells the handoff touches: the index reading's type-shaped skip counts the receiver's errors
/// (`mk("s").fs[int](3)` adds no `'int' is a type, not a value`), and a receiver's errors come
/// before the type argument's, in source order.
#[test]
fn a_value_receiver_feeds_both_bracket_readings() {
    let prelude = "struct Box:\n    v: int\n    fn map[U](self, f: fn(int) -> int) -> Box:\n        return Box(f(self.v))\nstruct H:\n    fs: List[fn(int) -> int]\nfn inc(x: int) -> int:\n    return x + 1\nfn mk(x: int) -> H:\n    return H([inc])\n";
    let cells: [(&str, &[&str]); 6] = [
        ("b := Box(0)\n    print(b.map[int](inc).v)", &[]),
        ("h := mk(0)\n    print(h.fs[k](1))", &[]),
        ("print(Box(0).map[int](inc).v)", &[]),
        ("print(mk(0).fs[k](1))", &[]),
        (
            "print(mk(\"s\").fs[int](3))",
            &["argument 1 of 'mk': expected int, found str"],
        ),
        (
            "print(Box(0).nomethod(1).map[Nope](inc).v)",
            &[
                "type Box has no method 'nomethod'",
                "unknown type 'Nope'",
                "method 'map' takes no type argument(s)",
            ],
        ),
    ];
    for (line, want) in cells {
        let src = format!("{prelude}fn main():\n    k := 0\n    {line}\n");
        let got = check_src(&src);
        assert!(
            got.len() == want.len() && got.iter().zip(want).all(|(g, w)| g.message.contains(w)),
            "{line}: expected errors {want:?} in this order, got {got:?}"
        );
    }
}
