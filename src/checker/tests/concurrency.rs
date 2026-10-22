//! Checker tests: spawn, Channel, wait, Shared/RwShared/Atomic, timer, Executor, sendability, net
//! timeouts.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- concurrency C1: spawn / parallel: nursery scoping -----

#[test]
fn spawn_at_function_scope_ok() {
    // M-C implicit nurseries: every function body is an implicit nursery, so a bare `spawn`
    // (no enclosing `parallel:`) is legal anywhere in a function. It joins at the function's end.
    ok("fn w():\n    print(1)\nfn main():\n    spawn w()\nmain()\n");
}

#[test]
fn spawn_at_module_toplevel_ok() {
    // M-C: the module top level is itself an implicit nursery (joins at program exit).
    ok("fn w():\n    print(1)\nspawn w()\n");
}

#[test]
fn spawn_inside_parallel_ok() {
    ok(
        "fn w():\n    print(1)\nfn main():\n    parallel:\n        spawn w()\n        spawn w()\nmain()\n",
    );
}

#[test]
fn nested_parallel_ok() {
    ok(
        "fn w():\n    print(1)\nfn main():\n    parallel:\n        parallel:\n            spawn w()\nmain()\n",
    );
}

#[test]
fn spawn_block_form_ok() {
    ok("fn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n");
}

// ----- concurrency C2: Channel[T] + sendability -----

#[test]
fn channel_construct_and_methods_ok() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    x := ch.recv()\n    n := ch.len()\n    print(x + n)\nmain()\n",
    );
}

#[test]
fn channel_bounded_capacity_and_cap_method_ok() {
    // `Channel[T](cap)` accepts an int capacity; `cap()` infers int on both bounded and unbounded.
    ok(
        "fn main():\n    b := Channel[int](4)\n    print(b.cap())\n    u := Channel[int]()\n    print(u.cap())\nmain()\n",
    );
}

#[test]
fn channel_capacity_must_be_int_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int](\"x\")\n    print(ch.len())\nmain()\n",
        "Channel capacity must be int",
    );
}

#[test]
fn channel_too_many_ctor_args_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int](1, 2)\n    print(ch.len())\nmain()\n",
        "takes an optional capacity",
    );
}

#[test]
fn channel_send_wrong_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn channel_needs_element_type() {
    rejects(
        "fn main():\n    ch := Channel()\n    print(ch.len())\nmain()\n",
        "Channel() needs an element type",
    );
}

#[test]
fn channel_protocol_element_is_sendable() {
    // Task 2 (option a): a protocol existential element type is now SENDABLE (Go `chan interface`
    // parity) — the erased witness crosses by value. A witness that genuinely can't serialize (an
    // FFI/native handle) is rejected at the RUNTIME airlock, not here — see
    // `vm::golden_tests::ffi_handle_cannot_cross_airlock_three_engine`. (Was rejected under the old
    // Error-only sendable-bounded rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nfn main():\n    ch := Channel[NS]()\n    print(ch.len())\nmain()\n",
    );
}

// ----- concurrency §6d: `wait` (select) -----

#[test]
fn wait_binds_element_type_in_arm() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    wait:\n        v := ch.recv(): print(v + 1)\nmain()\n",
    );
}

#[test]
fn wait_assign_target_typechecks() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    n := 0\n    wait:\n        n = ch.recv(): print(n)\nmain()\n",
    );
}

#[test]
fn wait_send_arm_typechecks() {
    // A bare `ch.send(v):` arm is legal — `v` must match the channel element type.
    ok(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.send(9): print(\"sent\")\n        else: print(\"full\")\nmain()\n",
    );
}

#[test]
fn wait_send_arm_wrong_value_type_rejected() {
    // `send` value must match the element type (`int`), reusing the ordinary channel-send check.
    rejects(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.send(\"x\"): print(1)\nmain()\n",
        "int",
    );
}

#[test]
fn wait_bare_non_send_arm_rejected() {
    // A bare arm that is not `ch.send(v)` (here `try_send`) lists the legal arm forms.
    rejects(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.try_send(1): print(1)\nmain()\n",
        "a wait arm must be a recv (`x := ch.recv()`), a send (`ch.send(v)`)",
    );
}

#[test]
fn wait_bare_arbitrary_expr_arm_rejected() {
    rejects(
        "fn f() -> int:\n    return 0\nfn main():\n    wait:\n        f(): print(1)\nmain()\n",
        "a wait arm must be a recv",
    );
}

#[test]
fn wait_discard_and_else_ok() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        _ := ch.recv(): print(1)\n        else: print(0)\nmain()\n",
    );
}

#[test]
fn wait_arm_bound_var_wrong_use_rejected() {
    // `v: int` bound by the arm — using it as a str is a type error.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    wait:\n        v := ch.recv(): print(v + \"x\")\nmain()\n",
        "int",
    );
}

#[test]
fn wait_assign_wrong_target_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    s := \"a\"\n    wait:\n        s = ch.recv(): print(s)\nmain()\n",
        "int",
    );
}

#[test]
fn wait_arm_binding_is_arm_local() {
    // `v` is scoped to its arm — referencing it after the `wait` is undeclared.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\n    print(v)\nmain()\n",
        "v",
    );
}

#[test]
fn wait_non_channel_arm_rejected() {
    rejects(
        "fn main():\n    f := fn() -> int: 1\n    wait:\n        v := f.recv(): print(v)\nmain()\n",
        "Channel",
    );
}

#[test]
fn wait_send_arm_non_channel_receiver_rejected() {
    // A send-arm receiver that merely HAS a `send` method (a user struct) but is not a `Channel[T]`
    // MUST be rejected — the compiler lowers a send-arm as a raw channel op, so a non-channel
    // receiver panics the VM (`channel_core on non-channel`) at runtime. Mirrors the recv-arm guard.
    rejects(
        "struct Box:\n    n: int\n    fn send(self, v: int): print(v)\nfn main():\n    b := Box(0)\n    wait:\n        b.send(5): print(\"x\")\n        else: print(\"idle\")\nmain()\n",
        "Channel",
    );
}

#[test]
fn channel_close_ok() {
    ok("fn main():\n    ch := Channel[int]()\n    ch.close()\nmain()\n");
}

#[test]
fn channel_close_rejects_args() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.close(1)\nmain()\n",
        "close",
    );
}

#[test]
fn channel_try_send_returns_bool() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    sent: bool = ch.try_send(1)\n    print(sent)\nmain()\n",
    );
}

#[test]
fn channel_try_send_wrong_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.try_send(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn for_over_channel_binds_element() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    for v in ch:\n        print(v + 1)\nmain()\n",
    );
}

#[test]
fn for_over_channel_two_vars_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    for k, v in ch:\n        print(k)\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
}

#[test]
fn close_on_non_channel_rejected() {
    rejects("fn main():\n    x := 1\n    x.close()\nmain()\n", "close");
}

#[test]
fn comprehension_over_channel_checks() {
    // A Channel[T] is an Iterable[T] (owner U6): a comprehension drains it until it is closed.
    ok("fn main():\n    ch := Channel[int]()\n    xs := [v for v in ch]\n    print(xs)\nmain()\n");
    // The element type is the channel's: a str slot rejects an int element.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    xs: List[str] = [v for v in ch]\n    print(xs)\nmain()\n",
        "",
    );
}

#[test]
fn spawn_capture_free_closure_arg_ok() {
    // B3.3 (Task 2a): a CAPTURE-FREE closure passed as a spawn arg is sendable — it crosses the
    // airlock by value — so it is accepted (was rejected under the old "Func non-sendable" rule).
    ok(
        "fn run(f: fn() -> int):\n    print(f())\nfn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn run(g)\nmain()\n",
    );
}

#[test]
fn spawn_arg_closure_capturing_protocol_local_ok() {
    // Task 2 (option a): a closure ARG capturing a protocol-typed local `p: NS = Boxy(0)` is now
    // ACCEPTED — a protocol existential is sendable (its witness `Boxy` crosses by deep value copy).
    // The capture gate routes through `self.sendable()`, so this flipped when protocols became
    // sendable; the old rejection was a false positive. A capture that genuinely can't cross (an
    // FFI/native handle) is still rejected at the RUNTIME airlock — see
    // `vm::golden_tests::ffi_handle_cannot_cross_airlock_three_engine`.
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(f: fn() -> int):\n    print(f())\nfn main():\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn run(g)\nmain()\n",
    );
}

#[test]
fn spawn_keyword_arg_closure_capturing_protocol_local_ok() {
    // Task 2 (option a): a closure capturing a protocol-typed local, passed by LABEL or positionally
    // through a function VALUE, is now ACCEPTED — a protocol existential is sendable (witness `Boxy`
    // crosses by deep value copy). Both spellings agree (the capture gate routes both through
    // `self.sendable()`). Genuine non-sendability (FFI handle) is caught at the runtime airlock.
    let prelude = "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(f: fn() -> int):\n    print(f())\n";
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn h(f=g)\nmain()\n"
    ));
    // The positional form of the SAME call is accepted identically — parity of the two spellings.
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn h(g)\nmain()\n"
    ));
}

#[test]
fn spawn_protocol_value_keyword_arg_ok() {
    // Task 2 (option a): a protocol-typed value passed by label OR positionally to a spawned function
    // value is now ACCEPTED — a protocol existential is sendable (its witness `Boxy` crosses by deep
    // value copy, agrees serial==M:N). Both spellings agree. (Was rejected under the old rule.)
    let prelude = "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(r: NS):\n    print(r.tag())\n";
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    b: NS = Boxy(0)\n    parallel:\n        spawn h(r=b)\nmain()\n"
    ));
    // Positional form of the same call — accepted identically.
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    b: NS = Boxy(0)\n    parallel:\n        spawn h(b)\nmain()\n"
    ));
}

#[test]
fn spawn_sendable_args_ok() {
    ok(
        "fn worker(id: int, prefix: str, out: Channel[str]):\n    out.send(\"{prefix}-{id}\")\nfn main():\n    ch := Channel[str]()\n    parallel:\n        spawn worker(1, \"t\", ch)\nmain()\n",
    );
}

#[test]
fn spawn_firstclass_builtin_accepted_like_defer() {
    // The universe fns print/ord/chr/panic are first-class values that cross the airlock by name,
    // so `spawn print(...)` is accepted — symmetric with `defer print(...)`. A non-first-class
    // builtin/ctor still must be wrapped.
    ok("fn main():\n    parallel:\n        spawn print(\"hi\")\nmain()\n");
    rejects(
        "fn main():\n    parallel:\n        spawn int(1)\nmain()\n",
        "spawn requires a function or method call",
    );
}

#[test]
fn spawn_bad_arg_reports_one_error() {
    // The sendability gate must not double-report a type error already raised by inferring the call.
    let errs = check_src(
        "fn w(x: int):\n    print(x)\nfn main():\n    parallel:\n        spawn w(nope)\nmain()\n",
    );
    let dups = errs
        .iter()
        .filter(|e| e.message.contains("unknown name 'nope'"))
        .count();
    assert_eq!(
        dups, 1,
        "expected exactly one 'unknown name' error, got: {errs:?}"
    );
}

#[test]
fn channel_protocol_struct_field_is_sendable() {
    // Task 2 (option a): a struct with a protocol-typed field is now a SENDABLE channel element — the
    // field's erased witness crosses by deep value copy. (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Holder:\n    f: NS\nfn main():\n    ch := Channel[Holder]()\n    print(ch.len())\nmain()\n",
    );
}

#[test]
fn spawn_protocol_struct_field_arg_ok() {
    // Task 2 (option a): a struct carrying a protocol-typed field is a SENDABLE spawn arg — the whole
    // aggregate (`Holder{f: Boxy(0)}`) crosses by deep value copy. (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nstruct Holder:\n    f: NS\nfn run_it(h: Holder):\n    print(h.f.tag())\nfn main():\n    h := Holder(Boxy(0))\n    parallel:\n        spawn run_it(h)\nmain()\n",
    );
}

#[test]
fn sendable_recursive_struct_ok() {
    // A self-referential struct of sendable fields must terminate (cycle guard) and be sendable.
    ok(
        "struct Node:\n    val: int\n    next: Node\nfn use_it(n: Node):\n    print(n.val)\nfn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n",
    );
}

#[test]
fn reassign_captured_local_in_spawn_block_ok() {
    // Uniform by-reference capture (F1): a `spawn:` task gets its OWN per-task copy of a captured
    // LOCAL (the airlock deep-copies its cell), so reassigning it is allowed — the write mutates the
    // isolated copy and is not visible to the parent (the one deliberate divergence from Go).
    rejects(
        "fn main():\n    counter := 0\n    parallel:\n        spawn:\n            counter = counter + 1\nmain()\n",
        "'counter' is this task's copy",
    );
}

// ----- B3: in-place mutation of a captured MODULE GLOBAL aggregate is frozen too -----

#[test]
fn mutate_fn_local_aggregate_in_spawn_block_ok() {
    // A fn-LOCAL aggregate captured into a spawn: is deep-copied per task (agrees serial==M:N), so its
    // in-place mutation stays ACCEPTED — only MODULE-GLOBAL roots are frozen.
    rejects(
        "fn main():\n    xs := [1, 2, 3]\n    parallel:\n        spawn:\n            xs.push(99)\n    print(xs.len())\nmain()\n",
        "'xs' is this task's copy",
    );
    rejects(
        "fn main():\n    m := {1: 2}\n    parallel:\n        spawn:\n            m[1] = 9\nmain()\n",
        "'m' is this task's copy",
    );
}

#[test]
fn read_method_on_captured_module_global_in_spawn_ok() {
    // A non-mutating method (`.len()`, `.get()`, `.map()`) on a captured module global is a READ — it
    // must stay accepted (guards against over-listing the mutator set).
    ok(
        "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            print(xs.len())\nmain()\n",
    );
}

#[test]
fn mutate_module_global_aggregate_sequentially_ok() {
    // Outside any spawned task the module global is not frozen — a top-level `.push` stays fine.
    ok("xs := [1, 2, 3]\nxs.push(4)\nprint(xs.len())\n");
}

#[test]
fn shared_update_on_captured_module_global_in_spawn_ok() {
    // The collision guard: `Shared.update` shares the mutator name `update` with `Map`, but the gate is
    // typed on the receiver so a captured module-global Shared stays accepted (that IS the cross-task box).
    entry_ok(
        "import Shared from std.concurrency\ng := Shared(0)\nfn main():\n    parallel:\n        spawn:\n            g.update(fn(x): x + 1)\nmain()\n",
    );
}

#[test]
fn task_local_binding_in_spawn_block_assignable() {
    // A binding declared *inside* the task body is task-local, not a capture — assignable.
    ok(
        "fn main():\n    parallel:\n        spawn:\n            x := 0\n            x = x + 1\n            print(x)\nmain()\n",
    );
}

#[test]
fn spawn_in_plain_fn_ok() {
    // M-C: a `spawn` in a function with no explicit `parallel:` is legal — the function body is an
    // implicit nursery that joins at the function's end. The function-boundary rule still holds at
    // runtime (the task binds to *this* function's nursery, never the caller's), enforced by the
    // compiler/VM emitting a per-function implicit nursery.
    ok(
        "fn w():\n    spawn other()\nfn other():\n    print(1)\nfn main():\n    parallel:\n        w()\nmain()\n",
    );
}

// ----- concurrency C3: Shared[T], the cross-task mutable box -----

#[test]
fn shared_construct_and_methods_ok() {
    // `Shared(v)` infers its element type from the value (no `[T]` type arg, unlike Channel).
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(5)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn shared_get_returns_element_type() {
    // `get()` yields `T`, so it must compose where a `T` is expected (here, str concat).
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(\"hi\")\n    msg := s.get() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn shared_set_wrong_type_rejected() {
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn shared_update_fn_arity_rejected() {
    // `update` takes `fn(T) -> T`; a two-param closure must not type-check.
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x, y): x + y)\nmain()\n",
        "argument 1 of 'update'",
    );
}

#[test]
fn shared_accepts_turbofish() {
    // `Shared[T](v)` — the turbofish is OPTIONAL (value-first), and when present pins the element
    // type. It must agree with the value's inferred type.
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[int](0)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn rwshared_list_view_methods_accept_on_list_element() {
    // The zero-copy read-view methods (`len`/`at`/`slice`/`for_each`/`fold`) are valid when the box
    // element is a list. `fold`'s R is inferred from the concrete `init` accumulator.
    entry_ok(
        "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\nfn main():\n    box := ConcurrentList(concurrency.RwShared([1, 2, 3]))\n    print(box.len())\n    print(box.at(0))\n    print(box[0..2])\n    box.for_each(fn(x): print(x))\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
    );
    // R is not pinned to the element type — a str accumulator folds a list of ints to a str.
    entry_ok(
        "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\nfn main():\n    box := ConcurrentList(concurrency.RwShared([1, 2, 3]))\n    s := box.fold(\"\", fn(a, x): a + str(x))\n    print(s)\nmain()\n",
    );
}

#[test]
fn rwshared_list_view_methods_reject_on_non_list_element() {
    // Element-shape gate at the CHECKER (no check-OK-then-run-fault): the read-view methods are gated
    // to a list element, so a non-list `RwShared[int]` cleanly reports "no method".
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
        "type RwShared[int] has no method 'fold'",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.at(0))\nmain()\n",
        "type RwShared[int] has no method 'at'",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.len())\nmain()\n",
        "type RwShared[int] has no method 'len'",
    );
}

#[test]
fn rwshared_readview_gate_rejects_non_container_and_wrong_method() {
    // Constructor-kind gate: a scalar/tuple element head is not a recognized container, so every
    // read-view method cleanly reports "no method" (no check-OK-then-run-fault). A Map/Set-only method
    // name on the wrong container head also misses.
    // int element — Set/Map methods miss.
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.contains(3))\nmain()\n",
        "type RwShared[int] has no method 'contains'",
    );
    // str element — fold_entries (a Map method) misses.
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared(\"hi\")\n    print(box.fold_entries(0, fn(a, k, v): a))\nmain()\n",
        "has no method 'fold_entries'",
    );
    // Tuple element is heterogeneous — EXCLUDED entirely: len/fold both miss.
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared((1, \"a\"))\n    print(box.len())\nmain()\n",
        "has no method 'len'",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared((1, \"a\"))\n    print(box.fold(0, fn(a, x): a))\nmain()\n",
        "has no method 'fold'",
    );
    // A List element rejects Map/Set-specific method names (fold_entries/contains) — head branches first.
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared([1, 2, 3])\n    print(box.fold_entries(0, fn(a, k, v): a))\nmain()\n",
        "has no method 'fold_entries'",
    );
    // A Map element rejects the List-only `fold`/`at` names (Map uses fold_entries).
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared({\"a\": 1})\n    print(box.fold(0, fn(a, x): a))\nmain()\n",
        "has no method 'fold'",
    );
}

#[test]
fn rwshared_map_readview_methods() {
    // Map read-view: len/get_key/has/for_each_entry/fold_entries. K/V arm-recovered from the concrete
    // Map[K,V]; fold_entries's R pins from the concrete init.
    entry_ok(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1, \"b\": 2}))\n    print(box.len())\n    print(box.contains(\"a\"))\n    box.for_each_entry(fn(k, v): print(k + str(v)))\n    print(box.fold_entries(0, fn(a, k, v): a + v))\nmain()\n",
    );
    // get_key returns Option[V] — matchable, V concrete (no unbound Param escapes).
    entry_ok(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    match box.get(\"a\"):\n        ?v: print(v + 1)\n        None: print(-1)\nmain()\n",
    );
    // Nesting: V = List[int] recovered.
    entry_ok(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": [1, 2]}))\n    match box.get(\"a\"):\n        ?v: print(v.len())\n        None: print(-1)\nmain()\n",
    );
    // fold_entries R is not pinned to V — a str accumulator over a Map[str,int] folds to str.
    entry_ok(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    s := box.fold_entries(\"\", fn(a, k, v): a + k)\n    print(s)\nmain()\n",
    );
    // Wrong key type on get_key/has is rejected (K = str; int arg mismatches).
    entry_rejects(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    print(box.get(3))\nmain()\n",
        "expected",
    );
}

#[test]
fn rwshared_map_write_entry_methods() {
    // TICKET-192: the single-entry writers of a `RwShared[Map[K, V]]`.
    entry_ok(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    _ := box.insert(\"a\", 2)\n    _ := box.remove(\"a\")\n    print(box.get_or_insert(\"a\", 3) + 1)\nmain()\n",
    );
    // Wrong key type, wrong value type.
    entry_rejects(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    _ := box.insert(1, 2)\nmain()\n",
        "expected",
    );
    entry_rejects(
        "import std.concurrency\nimport ConcurrentMap from std.concurrency.collection\nfn main():\n    box := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n    _ := box.insert(\"a\", \"x\")\nmain()\n",
        "expected",
    );
    // A List element has no map writers.
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    box := RwShared([1])\n    box.set_key(0, 1)\nmain()\n",
        "has no method 'set_key'",
    );
}

#[test]
fn rwshared_set_readview_methods() {
    // Set read-view: len/contains/for_each/fold. E arm-recovered from the concrete Set[E].
    entry_ok(
        "import std.concurrency\nimport ConcurrentSet from std.concurrency.collection\nfn main():\n    box := ConcurrentSet(concurrency.RwShared(Set([1, 2, 3])))\n    print(box.len())\n    print(box.contains(2))\n    box.for_each(fn(x): print(x))\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
    );
    // contains with the wrong element type is rejected (E = int; str arg mismatches).
    entry_rejects(
        "import std.concurrency\nimport ConcurrentSet from std.concurrency.collection\nfn main():\n    box := ConcurrentSet(concurrency.RwShared(Set([1, 2, 3])))\n    print(box.contains(\"x\"))\nmain()\n",
        "expected",
    );
    // Set fold R not pinned to E — str accumulator over Set[int] folds to str.
    entry_ok(
        "import std.concurrency\nimport ConcurrentSet from std.concurrency.collection\nfn main():\n    box := ConcurrentSet(concurrency.RwShared(Set([1, 2, 3])))\n    s := box.fold(\"\", fn(a, x): a + str(x))\n    print(s)\nmain()\n",
    );
}

#[test]
fn shared_is_sendable() {
    // A `Shared[T]` handle crosses the airlock — both spawned tasks reach the same box.
    entry_ok(
        "import Shared from std.concurrency\nfn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\nfn main():\n    s := Shared(0)\n    parallel:\n        spawn bump(s)\n        spawn bump(s)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn shared_handle_sendable_regardless_of_element() {
    // The asymmetry vs Channel: a `Shared` handle is sendable even when its element type isn't
    // (the value never crosses the airlock — only the handle does). Locks in the intent.
    entry_ok(
        "import Shared from std.concurrency\nfn use_it(s: Shared[fn() -> int]):\n    f := s.get()\n    print(f())\nfn main():\n    g := fn() -> int: 1\n    s := Shared(g)\n    parallel:\n        spawn use_it(s)\nmain()\n",
    );
}

// ----- concurrency: RwShared[T], the cross-task read-write box -----

#[test]
fn rwshared_construct_and_methods_ok() {
    // `RwShared(v)` infers its element type from the value (value-first, like `Shared`).
    entry_ok(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    r.set(5)\n    r.write(fn(x): x + 1)\n    print(r.read(fn(x): x))\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_read_returns_closure_result_type() {
    // `read(f: fn(T) -> R) -> R` — R is the closure's return type, distinct from T here (int -> str).
    entry_ok(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    msg := r.read(fn(x): str(x)) + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn rwshared_read_closure_body_error_reported_once() {
    // A type error inside the read() closure body must be reported ONCE. `read` recovers R by
    // re-inferring the closure after `check_args` already inferred it; that recovery must not
    // double-emit the body's errors (regression: `nope` was reported twice).
    let errs = check_entry(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    print(r.read(fn(x): nope + x))\nmain()\n",
    );
    let n = errs.iter().filter(|e| e.message.contains("nope")).count();
    assert_eq!(
        n, 1,
        "expected the 'nope' error exactly once, got: {errs:?}"
    );
}

#[test]
fn rwshared_get_returns_element_type() {
    entry_ok(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(\"hi\")\n    msg := r.get() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn rwshared_set_wrong_type_rejected() {
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    r.set(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn rwshared_write_fn_arity_rejected() {
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    r.write(fn(x, y): x + y)\nmain()\n",
        "argument 1 of 'write'",
    );
}

#[test]
fn rwshared_unknown_method_rejected() {
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared(0)\n    r.update(fn(x): x + 1)\nmain()\n",
        "has no method 'update'",
    );
}

#[test]
fn rwshared_accepts_turbofish() {
    entry_ok(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared[int](0)\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_is_sendable() {
    entry_ok(
        "import RwShared from std.concurrency\nfn bump(r: RwShared[int]):\n    r.write(fn(x): x + 1)\nfn main():\n    r := RwShared(0)\n    parallel:\n        spawn bump(r)\n        spawn bump(r)\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_annotation_with_map_ok() {
    entry_ok(
        "import RwShared from std.concurrency\nfn put(r: RwShared[Map[str, int]], k: str):\n    r.write(fn(m): m)\nfn main():\n    r := RwShared({\"a\": 1})\n    put(r, \"b\")\n    print(r.get())\nmain()\n",
    );
}

// ----- Atomic[T]: the generic atomic box -----

#[test]
fn atomic_construct_and_methods_ok() {
    // `Atomic(v)` infers its element type from the value (value-first, like `Shared`).
    entry_ok(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(0)\n    a.store(5)\n    n := a.add(1)\n    m := a.sub(2)\n    old := a.exchange(9)\n    ok := a.cas(9, 10)\n    print(a.load())\nmain()\n",
    );
}

#[test]
fn atomic_load_returns_element_type() {
    // `load()` yields `T`, so it composes where a `T` is expected (here, str concat).
    entry_ok(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(\"hi\")\n    msg := a.load() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn atomic_cas_returns_bool() {
    // `cas(expected, new)` reports whether the swap happened.
    entry_ok(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(0)\n    if a.cas(0, 1):\n        print(\"swapped\")\nmain()\n",
    );
}

#[test]
fn atomic_store_wrong_type_rejected() {
    entry_rejects(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(0)\n    a.store(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn atomic_add_non_numeric_rejected() {
    // `add`/`sub` are arithmetic — only `int`/`float` boxes have them.
    entry_rejects(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(\"x\")\n    a.add(1)\nmain()\n",
        "no method 'add'",
    );
}

#[test]
fn atomic_accepts_turbofish() {
    // `Atomic[T](v)` — turbofish optional (value-first); when present it pins the element type.
    entry_ok(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic[int](0)\n    print(a.load())\nmain()\n",
    );
}

#[test]
fn concurrency_ctor_turbofish_checks_value() {
    // A turbofish element type that disagrees with the value's inferred type is rejected.
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[str](0)\nmain()\n",
        "expected element type str, found int",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared[str](0)\nmain()\n",
        "expected element type str, found int",
    );
    entry_rejects(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic[str](0)\nmain()\n",
        "expected element type str, found int",
    );
}

#[test]
fn concurrency_ctor_turbofish_arity() {
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[int, str](0)\nmain()\n",
        "Shared[T]() takes exactly one type argument",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared[int, str](0)\nmain()\n",
        "RwShared[T]() takes exactly one type argument",
    );
    entry_rejects(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic[int, str](0)\nmain()\n",
        "Atomic[T]() takes exactly one type argument",
    );
}

#[test]
fn atomic_is_sendable() {
    // An `Atomic[T]` handle crosses the airlock — both spawned tasks reach the same box.
    entry_ok(
        "import Atomic from std.concurrency\nfn bump(a: Atomic[int]):\n    a.add(1)\nfn main():\n    a := Atomic(0)\n    parallel:\n        spawn bump(a)\n        spawn bump(a)\n    print(a.load())\nmain()\n",
    );
}

// ----- timer(ms): one-shot timeout channel -----

#[test]
fn timer_returns_channel_bool() {
    // `timer(ms)` yields a `Channel[bool]`; `recv()` on it composes where a `bool` is expected.
    // `timer` now requires `import std.time` (gated builtin) — see `timer_requires_import`.
    entry_ok(
        "import timer from std.time\nfn main():\n    t := timer(50)\n    if t.recv():\n        print(\"fired\")\nmain()\n",
    );
}

#[test]
fn timer_arg_must_be_int() {
    entry_rejects(
        "import timer from std.time\nfn main():\n    t := timer(\"x\")\n    print(t.recv())\nmain()\n",
        "expected int",
    );
}

#[test]
fn import_alias_nil_from_accepted() {
    // BOUNDARY: `nil` is an ordinary name (TICKET-231), so it is a legal alias target.
    let t = TmpDir::new();
    t.write("lib.chz", "fn who() -> int:\n    return 1\n");
    let entry = t.write(
        "main.chz",
        "import who as nil from lib\nfn main():\n    print(1)\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    // THIS guard must not reject `nil`; if some UNRELATED path errors, at least it's not `reserved`.
    assert!(
        !errs.iter().any(|e| e.message.contains("reserved")),
        "None alias must not be rejected as reserved, got: {errs:?}"
    );
}

#[test]
fn import_alias_nonreserved_helper_from_accepted_and_usable() {
    // OVER-REJECT BOUNDARY: a fresh non-reserved alias must still bind and be usable (the widened
    // guard must not short-circuit the bind logic for a legit alias).
    let t = TmpDir::new();
    t.write("lib.chz", "fn who() -> int:\n    return 1\n");
    let entry = t.write(
        "main.chz",
        "import who as Helper from lib\nfn main():\n    print(Helper())\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    assert!(
        check_graph(&graph).is_ok(),
        "non-reserved alias Helper must check clean and be usable"
    );
}

#[test]
fn reserved_name_local_shadow_still_ok() {
    // BOUNDARY: only the IMPORT-ALIAS binding target is gated. Value-level local shadowing of a
    // reserved name (Python-style) MUST still check clean — it goes through `declare`, not
    // `bind_import`.
    ok("range := 5\nprint(range)\n");
    ok("fn f(range: int) -> int:\n    return range\nprint(f(3))\n");
    // A reserved *member* imported UN-aliased (bind == member) must still pass — the `a != member`
    // guard keeps `import Shared from std.concurrency` legal.
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    print(s.get())\nmain()\n",
    );
}

// ----- C5: the `Executor` escape hatch -----

#[test]
fn executor_construct_and_methods_ok() {
    entry_ok(
        "import Executor from std.concurrency\nfn job():\n    print(1)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): job())\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn executor_cancel_ok() {
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor()\n    ex.cancel()\nmain()\n",
    );
}

#[test]
fn executor_defer_shutdown_ok() {
    entry_ok(
        "import Executor from std.concurrency\nfn job():\n    print(1)\nfn main():\n    ex := Executor()\n    defer ex.shutdown()\n    ex.submit(fn(): job())\nmain()\n",
    );
}

#[test]
fn executor_type_arg_rejected() {
    rejects(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor[int]()\nmain()\n",
        "takes no type arguments",
    );
}

#[test]
fn executor_unknown_method_rejected() {
    entry_rejects(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor()\n    ex.run()\nmain()\n",
        "has no method 'run'",
    );
}

#[test]
fn executor_is_sendable() {
    // The handle crosses the airlock like Channel/Shared — submitting from a spawned task is legal.
    entry_ok(
        "import Executor from std.concurrency\nfn use_ex(ex: Executor):\n    ex.submit(fn(): print(1))\nfn main():\n    ex := Executor()\n    parallel:\n        spawn use_ex(ex)\n    ex.shutdown()\nmain()\n",
    );
}

// ----- Task 4: the four runtime concurrency ctors require `import std.concurrency` -----

#[test]
fn bare_concurrency_type_with_import_needs_type_arg() {
    // A BARE `Shared`/`RwShared`/`Atomic` annotation WITH the import is licensed but missing its
    // type argument — it must report the missing-type-arg error (matching the user-generic
    // precedent), NOT "unknown type".
    for (src, name) in [
        (
            "import Shared from std.concurrency\nfn f(s: Shared):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Shared",
        ),
        (
            "import RwShared from std.concurrency\nfn f(r: RwShared):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "RwShared",
        ),
        (
            "import Atomic from std.concurrency\nfn f(a: Atomic):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Atomic",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter().any(|e| e
                .message
                .contains(&format!("type '{name}' expects 1 type argument(s), got 0"))),
            "expected missing-type-arg error for licensed bare {name}, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.message.contains("unknown type")),
            "licensed bare {name} must not report unknown type, got: {errs:?}"
        );
    }
}

#[test]
fn atomic_int_arg_must_be_int() {
    entry_ok(
        "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(0)\n    a.add(1)\n    print(a.load())\nmain()\n",
    );
    entry_rejects(
        "import AtomicInt from std.concurrency\nfn main():\n    a := AtomicInt(3.5)\n    print(a.load())\nmain()\n",
        "expected int",
    );
}

#[test]
fn reserved_typeparam_fix_does_not_overreject() {
    // BOUNDARY (must-NOT-over-reject): the reserved-type-param guard checks param NAMES only. A
    // normal `[T]`, multi-param `[K, V]`, a word param, a free generic fn, and a protocol-BOUNDED
    // param `[T: Comparable]` (the bound — not the param name — is the reserved protocol) must all
    // still type-check clean. Stays GREEN both before and after the fix.
    entry_ok("struct Box[T]:\n    v: T\n");
    entry_ok("struct Pair[K, V]:\n    k: K\n    v: V\n");
    entry_ok("struct Box[Item]:\n    v: Item\n");
    entry_ok("fn id[T](x: T) -> T:\n    return x\n");
    entry_ok(
        "import Comparable from std.cmp\nfn pick[T: Comparable](a: T, b: T) -> T:\n    return a\n",
    );
}

#[test]
fn concurrency_from_import_ok() {
    // A from-import binds each named type, in value and annotation positions.
    entry_ok(
        "import Atomic, Executor, RwShared, Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    r := RwShared(0)\n    a := Atomic(0)\n    ex := Executor()\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import Atomic, Executor, RwShared, Shared from std.concurrency\nfn f(s: Shared[int], r: RwShared[int], a: Atomic[int], ex: Executor):\n    print(s.get())\nfn main():\n    print(1)\nmain()\n",
    );
}

// ----- namespace name-leak: builtin TYPE names reserved at declaration -----

#[test]
fn net_type_with_import_ok() {
    // A whole-module `import std.net` licenses both bare net handles in annotation position.
    entry_ok(
        "import Listener, Socket from std.net\nfn f(s: Socket, l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
    );
    // A selective per-name import licenses each named handle.
    entry_ok(
        "import Socket from std.net\nimport Listener from std.net\nfn f(s: Socket, l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn net_handle_bare_construction_rejected_after_import() {
    // Phase 4c-net regression — `Socket`/`Listener` now carry a `sig.struct_defs` entry (for their
    // harvested METHOD tables), but they are NOT constructible nominal structs. A whole-module
    // `import std.net` must NOT make bare `Socket(...)`/`Listener(...)` a from-nothing constructor
    // (a value comes only from `connect`/`listen`/`accept`). Guarded because the import struct-defs
    // loop otherwise seeds them into `struct_names`.
    for name in ["Socket", "Listener"] {
        let errs = check_entry(&format!(
            "import std.net\nfn main():\n    x := {name}()\n    print(1)\nmain()\n"
        ));
        assert!(
            !errs.is_empty(),
            "bare {name}() construction must be rejected after import std.net, got no errors"
        );
    }
}

// ----- C5 refinement #1: a non-sendable value merely *read* inside a `spawn:` block -----

#[test]
fn read_captured_capturefree_closure_in_spawn_block_ok() {
    // B3.3 (Task 2a): capturing a CAPTURE-FREE closure and calling it inside a task is now ACCEPTED —
    // `sendable(Func)` is permissive because a closure crosses the airlock BY VALUE (runs on both
    // engines). A closure that ITSELF captures a `ref` is a runtime-backstop concern (Task 2b); a
    // directly-captured `ref` is still rejected (`ref_binding_captured_in_spawn_rejected`).
    ok(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            print(g())\nmain()\n",
    );
}

#[test]
fn read_captured_int_in_spawn_block_ok() {
    // A sendable capture (int) gets its own copy — reading it freely is the whole point.
    ok("fn main():\n    n := 42\n    parallel:\n        spawn:\n            print(n)\nmain()\n");
}

#[test]
fn read_captured_channel_in_spawn_block_ok() {
    // A Channel handle is sendable — capturing and using it inside a task is fine.
    ok(
        "fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            ch.send(1)\nmain()\n",
    );
}

#[test]
fn imported_module_used_in_spawn_block_ok() {
    // Regression: a whole-module import is bound at module scope (a global namespace resolvable in
    // every task, like a free function), not a per-task value capture — the read gate must not flag
    // it even though `Ty::Module` is non-sendable.
    entry_ok(
        "import std.math\nfn main():\n    parallel:\n        spawn:\n            print(math.floor(2.7))\nmain()\n",
    );
}

// ----- M24-5: a module-qualified call is a spawn TARGET, not a spawn on a receiver -----

/// A module is a NAMESPACE, not a receiver value, so `spawn lib.helper(3)` is a plain call — the
/// same shape `defer lib.helper(3)` has always accepted, and the one Go accepts as `go pkg.F(x)`.
/// It used to be refused with "cannot spawn on a non-sendable receiver of type module lib" because
/// the receiver sweep asked whether the MODULE could cross the airlock. The RUNNING proof (a user
/// module, an alias, a bare `spawn`, and a native module) is
/// `tests/chz/spec/spawn_module_target_test.chz`.
#[test]
fn spawn_on_module_qualified_fn_ok() {
    entry_ok("import std.math\nfn main():\n    parallel:\n        spawn math.abs(-3)\nmain()\n");
    entry_ok("import std.math as m\nfn main():\n    parallel:\n        spawn m.abs(-3)\nmain()\n");
    // …and without a `parallel:` nursery — the function's implicit nursery owns the task.
    entry_ok("import std.math\nfn main():\n    spawn math.abs(-3)\nmain()\n");
}

/// NON-REGRESSION for the fix above, RE-PINNED for TICKET-021: `Ty::Module` is now rejected one
/// level up, by the module-in-value-position rule in `Checker::infer_value` — a module can no
/// longer reach a container or a local at all, so all four programs below now fail at the module
/// READ (`[math]` / `math := math`) rather than at the spawn receiver sweep. The receiver sweep
/// itself is unchanged and NOT dead code — it still guards any future non-sendable `Ty`; if you add
/// one, restore a real receiver case here. The four programs stay byte-identical to the pre-TICKET-021
/// binary's so the sweep is re-exercised the moment it becomes reachable again.
#[test]
fn spawn_on_non_sendable_receiver_still_rejected() {
    entry_rejects(
        "import std.math\nfn main():\n    xs := [math]\n    parallel:\n        spawn xs.clear()\nmain()\n",
        "module 'math' is not a value",
    );
    // A local SHADOWING the module name is a real receiver, not a namespace.
    entry_rejects(
        "import std.math\nfn main():\n    math := [math]\n    parallel:\n        spawn math.clear()\nmain()\n",
        "module 'math' is not a value",
    );
    // …and so is a local BOUND to the module (`m := math`): the type is `Ty::Module`, but the head
    // is a value, not a namespace, so the compiler lowers a `SpawnRecv` on the module HANDLE and
    // the airlock refuses it at run time. Keying the skip on the type alone let this one through
    // `chezzi check` and fault at run time instead.
    entry_rejects(
        "import std.math\nfn main():\n    m := math\n    parallel:\n        spawn m.abs(-3)\nmain()\n",
        "module 'math' is not a value",
    );
    // Same shape re-bound under the module's OWN name — the skip must resolve the SCOPE, not just
    // the spelling.
    entry_rejects(
        "import std.math\nfn main():\n    math := math\n    parallel:\n        spawn math.abs(-3)\nmain()\n",
        "module 'math' is not a value",
    );
}

/// NON-REGRESSION for the fix above: the ARGUMENT sweep is untouched for a BARE module — a
/// non-sendable value handed directly to a spawned call is still refused there. The container case
/// (TICKET-021 RE-PINNED): `Ty::Module` no longer reaches a container at all (the module-value read
/// itself now errors), so `xs := [math]` fails at the read, not at the argument sweep.
#[test]
fn spawn_non_sendable_argument_still_rejected() {
    entry_rejects(
        "import std.math\nfn take(m: int) -> int:\n    return m\nfn main():\n    parallel:\n        spawn take(math)\nmain()\n",
        "cannot pass a non-sendable value of type module math to a spawned task",
    );
    entry_rejects(
        "import std.math\nfn take(m: int) -> int:\n    return m\nfn main():\n    xs := [math]\n    parallel:\n        spawn take(xs)\nmain()\n",
        "module 'math' is not a value",
    );
}

#[test]
fn top_level_closure_used_in_spawn_block_ok() {
    // Regression: a top-level (module-scope) binding is a global, not a per-task capture — reading
    // it inside a `spawn:` block is fine even when it's non-sendable.
    ok(
        "g := fn() -> int: 7\nfn main():\n    parallel:\n        spawn:\n            print(g())\nmain()\n",
    );
}

#[test]
fn read_captured_capturefree_closure_through_nested_closure_in_spawn_block_ok() {
    // B3.3 (Task 2a): a capture-free function-local closure reached through a NESTED closure inside a
    // `spawn:` block is now ACCEPTED — both closures are sendable (they cross by value; runs on both
    // engines). Previously rejected under the old "Func non-sendable" rule. The only remaining hole —
    // a nested closure that ITSELF captures a `ref` — is caught by the runtime backstop (Task 2b).
    ok(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            h := fn() -> int: g()\n            print(h())\nmain()\n",
    );
}

// ----- A3b (B3.6): Executor.submit gates its closure captures like `spawn` -----

#[test]
fn submit_protocol_capture_ok() {
    // Task 2 (option a): a submitted closure capturing a protocol-typed binding is now ACCEPTED — a
    // protocol existential is sendable (witness `Boxy` crosses by deep value copy to the pool thread).
    // (Was rejected under the old rule; the submit capture gate routes through `self.sendable()`.)
    entry_ok(
        "import Executor from std.concurrency\nprotocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn main():\n    p: NS = Boxy(0)\n    ex := Executor()\n    ex.submit(fn(): print(p.tag()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_capturefree_closure_ok() {
    // B3.3 (Task 2a): submitting a closure that captures a CAPTURE-FREE sibling closure is now
    // ACCEPTED — closures cross the airlock by value. A submitted closure that
    // captures a `ref` is still rejected (`submit_non_sendable_capture_rejected`).
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_captured_channel_ok() {
    // A Channel handle is sendable — capturing it in a submitted task is fine.
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    ch := Channel[int]()\n    ex := Executor()\n    ex.submit(fn(): ch.send(1))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_captured_int_ok() {
    // A sendable capture (int) gets its own copy — reading it in the task is the whole point.
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    n := 42\n    ex := Executor()\n    ex.submit(fn(): print(n))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_capturefree_closure_through_nested_closure_ok() {
    // B3.3 (Task 2a): a capture-free closure reached through a NESTED closure inside a submitted task
    // is now ACCEPTED (both closures cross by value). Previously rejected under
    // the old "Func non-sendable" rule; a nested closure that captures a `ref` is Task 2b's backstop.
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print((fn() -> int: g())()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn top_level_closure_submitted_ok() {
    // Regression pin (mirrors `top_level_closure_used_in_spawn_block_ok`): a module-scope binding is a
    // global, not a per-task capture — submitting a closure that reads it is fine even when it's
    // non-sendable (the `is_local_capture` scope-0 exclusion). Locks the intentional gap so a future
    // tightening of the gate can't silently flip it without a test failing.
    entry_ok(
        "import Executor from std.concurrency\ng := fn() -> int: 7\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n",
    );
}

// ----- B3.3 (Task 2a): capture-sendability gate at the spawn CALLEE + ARG sites -----
// A closure/nested-fn value crosses the airlock BY VALUE (`sendable(Func)` is permissive), so the
// bare `fn` type type-checks; the per-closure capture check moves to the airlock SITES. A captured
// NON-SENDABLE LOCAL (a protocol existential etc.) at a spawn callee/arg is a clean compile error,
// matching the `spawn:` block form. A MODULE-GLOBAL non-sendable is a read-only global, NOT a
// capture — never gated. (The probe is a protocol existential — a genuinely non-sendable value.)

#[test]
fn protocol_local_captured_by_spawn_callee_closure_ok() {
    // Task 2 (option a): a function-local protocol-typed value captured by a closure VALUE that is the
    // spawn CALLEE is now ACCEPTED — a protocol existential is sendable (witness `Boxy` crosses by
    // deep value copy). (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn main():\n    p: NS = Boxy(0)\n    grab := fn() -> int: p.tag()\n    parallel:\n        spawn grab()\nmain()\n",
    );
}

#[test]
fn module_global_nonsendable_spawn_callee_ok() {
    // CRITICAL NON-REGRESSION: a non-sendable value declared at MODULE scope is a read-only global
    // resolvable in every task (like a free fn), NOT a per-task local capture — the scope-0 exclusion
    // must keep it out of the gate even when a task READS it.
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\ng: NS = Boxy(42)\nfn work(ch: Channel[int]):\n    ch.send(g.tag())\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn work(ch)\n    print(ch.recv())\nmain()\n",
    );
}

#[test]
fn spawn_callee_capturing_sendable_closure_ok() {
    // A callee whose captured environment holds a CAPTURE-FREE sibling closure (`double`) is sendable
    // (it crosses by value) — must NOT be gated (example #1, already runs; pin the check side).
    ok(
        "fn main():\n    double := fn(x: int) -> int: x * 2\n    ch := Channel[int]()\n    work := fn(): ch.send(double(21))\n    parallel:\n        spawn work()\n    print(ch.recv())\nmain()\n",
    );
}

#[test]
fn channel_of_closures_typechecks() {
    // `sendable(Func)` permissive → a `Channel[fn() -> int]` (closures as data) now type-checks; the
    // producer sends a capture-free closure and the consumer calls it (runs 42 — see parity test).
    ok(
        "fn producer(ch: Channel[fn() -> int]):\n    ch.send(fn() -> int: 42)\nfn main():\n    ch := Channel[fn() -> int]()\n    parallel:\n        spawn producer(ch)\n    f := ch.recv()\n    print(f())\nmain()\n",
    );
}

#[test]
fn closure_returned_across_task_typechecks() {
    // A capturing closure returned from a factory and sent over a `Channel[fn(int) -> int]` — the
    // closure carries a SENDABLE capture (an int `n`), so it crosses by value (runs 105 — parity test).
    ok(
        "fn adder(n: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + n\nfn producer(ch: Channel[fn(int) -> int]):\n    ch.send(adder(100))\nfn main():\n    ch := Channel[fn(int) -> int]()\n    parallel:\n        spawn producer(ch)\n    f := ch.recv()\n    print(f(5))\nmain()\n",
    );
}

// ----- G1 (B3.3b), RETIRED: these now just assert the checker ACCEPTS every shape. The read-only rule
// was deleted in 2026-07-21 (module globals deep-copy per task, so a task write lands on
// its own copy); W6-2 then made the copy fresh per nursery. Kept as accept-regressions. -----

#[test]
fn sequential_global_mutation_ok() {
    // Flow-scoped: the same mutation reached only from sequential (non-spawn) code stays legal.
    ok("n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump()\n    print(n)\nmain()\n");
}

#[test]
fn spawn_local_shadows_global_ok() {
    // A spawn-reachable function whose local shadows the global name mutates the LOCAL, not the
    // global — it must not be flagged.
    ok(
        "n := 0\nfn work():\n    n := 5\n    n = n + 1\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n",
    );
}

#[test]
fn spawn_reads_global_ok() {
    // Reading a (post-init constant) global from a task is fine; only mutation is gated.
    ok(
        "n := 7\nfn work():\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n",
    );
}

#[test]
fn shared_update_in_spawn_ok() {
    // The prescribed cross-task mutation path: a global `Shared`, mutated via `update()` in a task.
    entry_ok(
        "import Shared from std.concurrency\nc := Shared(0)\nfn bump():\n    c.update(fn(x): x + 1)\nfn main():\n    parallel:\n        spawn bump()\n    print(c.get())\nmain()\n",
    );
}

#[test]
fn spawn_callee_shadowed_by_local_ok() {
    // A local binding shadowing a free function's name at the spawn site means `spawn bump()`
    // targets the LOCAL (inert) closure, not the global-mutating free fn — must not be flagged.
    ok(
        "n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump := fn(): 1\n    parallel:\n        spawn bump()\nmain()\n",
    );
}

// `Ref` is now an ORDINARY identifier (the `ref` keyword and `Ref[T]` box were removed): a user
// `struct Ref` is legal, no longer reserved.
#[test]
fn user_struct_named_ref_is_legal() {
    entry_ok(
        "struct Ref:\n    val: int\nfn main():\n    r := Ref(5)\n    print(str(r.val))\nmain()\n",
    );
}

// ----- D6c: optional `timeout_ms` on net socket read/accept/write -----
// NOTE: bare `Socket`/`Listener` now require `import std.net` (Hole B), so these moved off the
// single-module `ok`/`rejects` helpers (which don't resolve native imports) onto the graph helpers
// `entry_ok`/`check_entry` with an `import std.net` prefix + a trivial `main` tail (the `use_*` fn is
// type-checked though never called). `check_entry` also desugars, so `?` lowers correctly.

#[test]
fn socket_read_with_timeout_type_checks() {
    // `read(n)` and `read(n, timeout_ms)` both type-check (the trailing int is optional).
    entry_ok(
        "import Socket from std.net\nfn use_sock(s: Socket) -> str!:\n    a := s.read(64)?\n    b := s.read(64, 100)?\n    return ?(a + b)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn socket_write_with_timeout_type_checks() {
    entry_ok(
        "import Socket from std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.write(\"x\")?\n    b := s.write(\"x\", 100)?\n    return ?(a + b)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn listener_accept_with_timeout_type_checks() {
    // `accept()` and `accept(timeout_ms)` both type-check.
    entry_ok(
        "import Listener from std.net\nfn use_listener(l: Listener) -> int!:\n    l.accept()?\n    l.accept(100)?\n    return ?0\nfn main():\n    print(1)\nmain()\n",
    );
}

/// Assert `check_entry(src)` reports `needle` AND did not leak an `unknown type` (i.e. the `import
/// std.net` licensed the handle, so the real method-arity/type error surfaces — not a swallowed
/// `Socket → Ty::Unknown` cascade).
fn check_entry_rejects_net(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("unknown type")),
        "a leaked 'unknown type' means the net handle was not licensed, got: {errs:?}"
    );
}

#[test]
fn socket_read_with_non_int_timeout_rejected() {
    // A non-int `timeout_ms` is a type error.
    check_entry_rejects_net(
        "import Socket from std.net\nfn use_sock(s: Socket):\n    s.read(64, \"x\")\nfn main():\n    print(1)\nmain()\n",
        "expected int",
    );
}

#[test]
fn socket_read_with_too_few_args_rejected() {
    // `read()` (zero args) is below the 1–2 arg range.
    check_entry_rejects_net(
        "import Socket from std.net\nfn use_sock(s: Socket):\n    s.read()\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}

#[test]
fn socket_read_with_too_many_args_rejected() {
    // `read(n, t, extra)` exceeds the 1–2 arg range.
    check_entry_rejects_net(
        "import Socket from std.net\nfn use_sock(s: Socket):\n    s.read(64, 100, 1)\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}

#[test]
fn listener_accept_with_too_many_args_rejected() {
    check_entry_rejects_net(
        "import Listener from std.net\nfn use_listener(l: Listener):\n    l.accept(100, 1)\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}
