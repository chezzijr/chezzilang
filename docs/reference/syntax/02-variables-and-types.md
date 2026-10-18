## 3. Variables & types

```chezzi
x := 5                 # declare + infer  (type = int)
name: str = "chezzi"   # declare with explicit type
count := 0
count += 1             # compound assignment — see below
a, b = b, a            # tuple swap — multi-target assignment, see below
```

- **Local inference:** inside function bodies `:=` infers the type.
- **Explicit annotation** (`name: T = ...`) is allowed anywhere and **required on function signatures** (§5).

**Evaluation order: the right-hand side runs first (Python semantics).** In `place = value`, `value`
runs before the target's sub-expressions: before `obj` and `i` in `obj[i] = value`, before `obj` in
`obj.f = value`, before `k` in `m[k] = value`. So `xs[bump()] = xs[bump()] + 10` on `[1, 2, 3]`, with
a `bump` that returns `0` then `1`, gives `[1, 11, 3]` (CPython's answer). A **compound** assignment
reads the target first, because `x OP= v` is `x = x OP v`: `xs[k()] += g()` calls `k` then `g`, as in
Python.

**Compound assignment.** `x OP= v` is exactly `x = x OP v`, on variables, list elements, struct
fields and map values.

- The set is `+= -= *= /= %=` (numeric; `+=` also concatenates `str`) and `&= |= ^= <<= >>=` (`int`,
  and `& | ^` on two sets).
- It accepts a struct/enum whose operator overload (`add`/`sub`/`mul`/…) makes `x = x OP v`
  type-check, for example `a += V(10)` for a `struct V` with `add`. It is rejected exactly when
  `x OP v` is a type error.
- No implicit widening: `int /= float` is a type error.
- The index expression runs exactly once (`t[f()] += 1` calls `f` once, as in Python).
- On a user index target (`obj[k] += v`, §7b) the read goes through `index`, so its type is
  `index`'s return, and it must agree with `set_index`'s `val`. An incoherent pair (`index -> str`
  with `set_index(_, val: int)`) is a check-time error (`type S does not satisfy IndexSet (…)`). A
  plain `obj[k] = v` only checks against `set_index`'s `val`.
- `//=` and `**=` do not exist (there is no `//`/`**` operator).

```chezzi
x *= 3        # x = x * 3
xs[0] <<= 1   # xs[0] = xs[0] << 1
p.n |= 4      # p.n = p.n | 4
```

**Multi-target (tuple) assignment.** `a, b = b, a` evaluates the whole right-hand side first, then
stores into each target left to right, so a swap is correct even when one place is on both sides.
Targets may be variables, list elements or struct fields. The RHS is a same-arity value list or one
tuple-valued expression (`a, b = f()`). Only plain `=` is allowed (a compound op with several targets
is a parse error).

```chezzi
a, b = b, a                              # swap two variables
data[0], data[1] = data[1], data[0]      # swap two list elements
p, q, r = r, p, q                        # three-way rotation (RHS evaluated first)
a, b = compute()                         # compute() returns (int, int)
```

### `const T` — immutable bindings

`const T` sits in the type slot of a single-name typed let and freezes the **name**: the checker
rejects any later reassignment. It is an immutable binding (JS `const`, Java `final`), not a
compile-time constant: the RHS is any runtime expression, evaluated once.

```chezzi
PI: const float = 3.14159
ANSWER: const int = 6 * 7     # runtime RHS is fine — const ≠ constexpr
PI = 3.0                      # ✗ type error: cannot reassign const binding 'PI'
ANSWER += 1                   # ✗ every compound form is caught too
```

A `const List[int]` rejects `xs += ys` too. Extend it in place with `xs.extend(ys)`: `const` is shallow.

- **Shallow.** `const` freezes the binding, not the object. A `const` container's contents stay mutable:

  ```chezzi
  xs: const List[int] = [1, 2]
  xs.push(3)                    # ✓ mutating THROUGH the const is fine
  xs[0] = 9                     # ✓ (index/field assignment is object mutation, not a rebind)
  xs = [4]                      # ✗ rebinding the NAME is the error
  ```
- **Where it is allowed.** Locals and module globals, on a single-name typed let only. `const` on a
  parameter or on a `:=`/destructuring binding is a parse error.
- **No re-declaration.** A live const cannot be re-declared in the same scope: `PI := 9.0` or a second
  `PI: float = 9.0` after `PI: const float = 3.14` is a type error (at module level:
  `variable 'PI' is already defined`). An inner-scope shadow (a nested block/fn) is fine.
- **Across modules.** A `const` module global (and a native constant like `math.pi`/`e`/`inf`/`nan`)
  stays const for importers: `import PI from m; PI = x` and `m.PI = x` both report *"it is declared
  const in module 'm'"*. Any imported global is read-only anyway (a from-import is a snapshot copy);
  the const mark only sharpens the message. `import PI from m; PI := 9.0` is `'PI' is already
  imported` (an inner-scope shadow is fine).
- A keyword call through a let-only closure of another module (`g := fn(a, b)` in `m`, then
  `m.g(a=1, b=2)`) is rejected with "need a binding that holds one known function", because `m` may
  reassign `g`. The same call inside `m` is accepted.

#### The blank identifier `_`

`_` is Go's blank identifier in `:=`, `=` and destructuring, at every scope:

- `_ := e` and `_ = e` evaluate `e` and discard it, any number of times, with any types.
- A `_` element of a multi-target `=` discards its element: `a, _ = (1, 2)` and `_, b[0] = t`.
- `_` is never declared. Reading it is an error: `cannot use '_' as a value` (Go: `cannot use _ as value or type`).
- A `for` loop variable or a comprehension variable named `_` skips the slot too
  (`for _ in xs: print(_)` is `cannot use '_' as a value`). A parameter or match pattern named `_` binds.
- `_ := f()` is a `let`, so it does not fire the discarded-`T!E` warning.

#### Unused locals warn

A fn-local or block-local `:=`/`let` binding, or a `for` loop variable, that is never read emits a
**warning** (never an error; Rust's model):

```chezzi
fn total(xs: List[int]) -> int:
    total := 0
    for x in xs:
        total := total + x    # `:=` typo instead of `=` — a new binding each iteration
    return total
print(total([1, 2, 3]))       # 0, silently wrong, before this rule
```

`chezzi check` reports `unused variable 'total': it is never read; prefix it with '_' if that is
intended` at the inner `total := total + x` (`"severity":"warning"` in `--errors=json`; exit code
unchanged). Prefix the name with `_` (or name it `_`) to opt out.

These never warn:
- a **parameter** (Go does not warn either)
- `x += e`: a compound assign reads `x`. A plain `x = e` is a write only, and only `:=`/`let`/`for`
  make `x` a candidate
- a `match` / `wait:` arm binding, or a comprehension `for` variable
- a name read only inside a closure, `defer:`, `spawn:`/`parallel:` body, a nested `fn`, or an
  interpolated fragment (`"{x}"`, `"{s:<{w}}"`): all are reads
- a module-scope global (an importer may read it)
- anything inside a `std.*` module

#### Declaring one name twice at module level

A module binds each name once, as Go (`x redeclared in this block`) and Rust (E0428, E0255) do. A
top-level `:=` or typed let declares; `=` reassigns. A type (`struct`, `enum`, `type`, `protocol`)
and a value (`fn`, `extern` fn, `:=`, an import) never share a name (owner 2026-10-16; Rust: E0428
for a callable type). A second declaration of a name is an error, in either source order. A
constructor with logic is a static `S.new(...)` (§7a):

```chezzi
x := 1
x := 2          # ✗ variable 'x' is already defined
x = 2           # ✓ reassignment

fn f():
    y := 1
    y := "9"    # ✓ a fn-local re-declare is a FRESH binding (Rust's `let` shadowing)
```

| prior binding | `fn` / `extern` fn / `:=` | `struct` / `enum` / `type` / `protocol` |
|---|---|---|
| fn, extern fn, top-level `:=`, value import | ✗ already defined / imported | ✗ already defined / imported |
| type, type import | ✗ already defined / imported | ✗ already defined / imported |
| whole-module or variant import | ✗ already imported | ✗ already imported |
| builtin (`ord`, `str`) | ✓ shadows | ✓ shadows |

A binding inside a fn or block body (a param, a `:=`, a nested `fn`, a top-level `if:` body) shadows
every row. Where Go and Rust split, Chezzi follows Rust. `tests/redeclaration_grid.rs` runs every cell.

#### What a module-level binding means

- **Module scope and initialization order.** A function body (top-level `fn`, method, nested `fn`,
  closure) sees every top-level binding, including one declared *below* it, as Go's package scope and
  Python's module globals do: its type, its `const` mark, and whether it holds one known function (so
  keyword arguments work through it). Top-level *statements* stay lexical: `print(x)` above `x := 5`
  is `unknown name 'x'`, and `X = 7` above it is `cannot assign to undeclared variable 'X'`.

  ```chezzi
  fn f() -> int:
      return x        # ✓ x is declared below
  print(f())          # ✗ runtime error: 'x' is read before its initialization at line 4
  x := 5
  print(f())          # 5
  ```

  - A body that reads a global before its initializing statement has *run* faults
    `'x' is read before its initialization at line N` (CPython's `NameError`). It is a runtime fault
    because the read may sit on a branch the call does not take.
  - A body that *writes* the global first initializes the slot; the let rebinds it when it runs.
  - Global types are computed once, before any body is checked, in dependency order. A typing cycle
    (`x := f()` where `f` returns `x`) is a compile error naming the `initialization cycle`; annotate
    `x` (`x: int = f()`) or give `f` a return type. An annotated cycle checks, and its read faults.
  - An un-annotated global with an open type (`xs := []`, `z := None`) is an error on its own line
    (see "Inference" in §10). A body above it that reads it also reports
    `'xs' is declared below this function and its type is not known here`.
  - An `import` below a body that uses it is rejected (`'pi' is used before its import`, Go's rule).
  - A body never decides a global's type: with `fn w(): z = ?"ab"` above `z := None`, the error is at
    `z := None`. Write `z: str? = None`; the body then checks against `str?`.
  - A module-level `return` is `'return' outside a function` (CPython's `SyntaxError`).
  - A keyword call through a module `fn` binds that fn's labels.
  - A let named like a type, a builtin or a builtin ctor (`P := fn..` beside `struct P`,
    `ord := fn..`) is an ordinary global: a body sees it in both orders, and a call before the let
    runs faults as for any other global.
- **A fn-local (or block-local) re-declare is a fresh binding**: it may change type, and a closure made
  earlier keeps the old one (Rust's `let` shadowing). A top-level `if:`/`for:`/`while:` body is an
  inner scope too.
- **The first declaration must be complete on its own line**: `x := []`, `y := {}` and `z := None` are
  each an error (``cannot infer the element type of `x` ``); write `x: List[int] = []`.
- **A name is bound once per target and per parameter list.** `a, a := (1, 2)`, `for a, a in xs` and
  `[a for a, a in xs]` are `identifier 'a' is bound more than once in this pattern` (the same error as
  a match pattern `(a, a)`); `fn g(a: int, a: int)` and `fn(a: int, a: int)` are
  `identifier 'a' is bound more than once in this parameter list`. Go and Rust reject these too. `_` may repeat.
- **Reading an imported name above its own `import` is an error:** *"'x' is used before its `import`
  on line 3"*. Move the `import` above the line that uses it.
  - Imports are hoisted, so `print(COUNT)` above `import COUNT from lib` would work; Chezzi rejects
    it because CPython (`NameError`) and Go both refuse it.
  - A from-imported **fn** follows the same rule in both spellings (`g := h` and `h()`). A same-module
    top-level `fn` is position-independent.
  - The rule is about value/callable reads only. A *type* name used above its import is fine, and an
    `import` may sit anywhere at top level if nothing above reads its binding.
  - A name that **shadows** the import (a parameter, a fn-local `:=`, a loop variable, a block local)
    is legal anywhere: `fn circumference(pi: float, r: float)` above `import pi from std.math` is fine.
- **A deferred read counts as a read.** A top-level `fn` body that reads an imported name above the
  `import` is rejected too. CPython accepts it; Go refuses it. Chezzi follows Go.

### Closure capture — uniformly by reference

A closure (and a `spawn:` / `parallel:` / `defer:` block) shares the *closest binding* of each
captured name: reads see later writes, and a write through the capture is visible in the defining
scope and in sibling closures. A plain local and a global behave the same.

| binding | captured as | `x := 10; f := fn() -> int: x; x = 20; f()` |
|---|---|---|
| plain local | shared (live) | `20` |
| global | shared (live) | `20` |

A closure captures only the names its body references (its free variables). An unrelated
non-sendable value in scope (another closure, a live generator) does not block a `spawn` /
`parallel:` crossing; only names the task uses are checked for sendability.

1. **Capture is by reference.** A captured **loop variable** rebinds into a fresh cell each iteration
   (Go ≥1.22), so `for i in [0,1,2]: fns.push(fn() -> int: i)` gives closures that return `0`, `1`,
   `2`. A variable declared outside the loop stays one shared cell.
2. **The task boundary is the one place sharing stops.** Across `spawn` / `parallel:` (a real OS
   thread) a captured local is **snapshot-copied** into a per-task cell.
   - There is one cell per **binding**, so sibling closures that cross in one task still share it (two
     closures over one local, sent through a `Channel` or passed to one `spawn` as two args, read and
     write the same cell, as in Go). Two separate tasks each get their own snapshot. One task that
     reaches a cell through a captured local and a module global shares it.
   - **A write inside the task to a copy (the captured cell, or a container/struct it points into) is
     a runtime fault.** `x := 0; parallel: spawn: x = x + 1; print(x)` faults in the `spawn:` body:
     `'x' is this task's copy: a write to it would be lost at the join; share it through
     Shared/Channel, or make a task-local copy with .copy()`. `recover:` catches it. This is the one
     divergence from Go, and it is the memory-safety line.
   - For shared mutation use `Shared[T]` / `RwShared[T]` / `Atomic` / `Channel[T]`, which cross by
     reference (see [`concurrency.md`](../concurrency/01-model.md)); writes through them never fault.
   - `.copy()` of a copy is unmarked: `ys := xs.copy(); ys.push(v)` in a task is a task-local write.

**The checker rejects visible writes before the program runs.** A direct write inside a `spawn:` task
to a captured binding or module global is a compile-time error with the runtime fault's message. This
covers reassignment, compound assignment, field/index stores, and mutating native methods on the
binding or a declared-type projection. A `defer:` inside the task is inside the task. The checker
infers, to a fixed point, which user methods store through `self` or call such a method, so
`spawn: counter.bump()` is rejected when `bump` writes `self`.

```chezzi
results: List[str] = []
parallel:
    spawn:
        results = ["ok", "ok"]     # compile error: 'results' is this task's copy: ...
for r in results:
    assert r == "ok"
```

- The checker also rejects a closure known to write a capture when a task runs it: a captured call
  in `spawn:`, `spawn g()`, or `Executor.submit(g)` / an inline literal.
- It reports proven writes by a directly called named function (nested `fn`, or through another named
  function), through a parameter, a capture or a module global. The error points at the task-side call
  and names the write in help.
- `spawn run(g)` arguments and `Channel.send(g)` are not execution (the receiver may never call `g`).
- Calls through function values, unresolved projections and protocol/generic receivers rely on the
  runtime fault. `spawn f(args)` evaluates `args` in the parent.
- Valid: reads, parent-side writes, task-created values, `.copy()` then mutation, handle writes, and a
  task-local `:=` shadow. The checker never reports an error unless the program would fault at runtime.

**Mutating a captured local.** A closure body is a single expression, so `fn(): n = n + 1` is a parse
error. Three ways to write through a captured binding:

- a **method call** (an expression): `bump := fn(): xs.push(2)`
- a **`defer:` / `spawn:` block** (a statement body): `defer: n = n + 1`
- a **nested `fn` declaration** (below): `fn bump(): n = n + 1`

To freeze a value, capture a fresh copy: `snap := x`, then capture `snap`.
Runnable demo: [`examples/closure_capture_scopes.chz`](../../../examples/closure_capture_scopes.chz).

### Nested function declarations

A `fn` statement inside a function body (or any block) is a **first-class local function**: a closure
with a name and a statement body.

- **Lexical nearest-scope.** A nested `fn f(x: int)` shadows a top-level `fn f()`. Its body is fully
  type-checked, and a call site is checked against the nested signature (`f()` with zero args is a
  check-time arity error).
- **Recursion.** A nested fn may call itself (`fn fact(n: int) -> int: … fact(n - 1)`).
- **By-reference capture,** as for any closure. Its statement body can reassign a captured local
  (`fn bump(): x = x + 1`). Across `spawn` / `parallel:` the two rules above apply
  ([`adr-d4-airlock.md`](../../design/adr-d4-airlock.md)): `x = x + 1` on a captured local, a
  `.push`/index/field store on a captured container, and a write to a module global fault in the task
  with `'<name>' is this task's copy: ...`. Three shapes still lose the write silently (see the
  ceilings in [`concurrency.md`](../concurrency/01-model.md)).
- **Fresh values are the task's own.** A value no parent binding can reach is writable in the task: a
  fresh `spawn` operand (`spawn work([], out)`, a comprehension, `xs.copy()`), every object its
  expression builds below the root (`spawn f([[]])`, `spawn f(S([]))`, a `T?` parameter given `[]`), a
  literal default fill (`acc: List[int] = []`) and a variadic pack, for every callee form (`f()`,
  `obj.m()`, `lib.f()`, `lib.K.f()`, a function value). A NAMED child stays a copy (`spawn f([xs])`);
  `xs.copy()` is fresh for its own level only. A parameter the callee rebinds before writing it
  (`ys = [1, 2]; ys.push(3)`) is not a task copy.

  A builtin slice is a new value, not a place. `grid[0..2]` builds a new list, so a mutating call or a
  store on it changes a dropped copy. It is not a D4 error in a spawned function; the checker warns:

  ```
  fn t(grid: List[List[int]]):
      grid[0..2].push([9])

  grid := [[1], [2], [3]]
  parallel:
      spawn t(grid)
  print(grid)
  ```

  ```
  warning (d4.chz:2:5): 'push' changes a temporary copy; the result is dropped: bind the value to a name first, or call 'push' on the source
  [[1], [2], [3]]
  ```

  A write through an element of the slice (`grid[0..2][0].push(9)`) or through an index
  (`grid[0].push(9)`) is a real write to `grid` and stays the D4 error. A `Map[Range, V]` key
  (`m[0..3].push(9)`) is an index, so it is a place. A user `slice` hook may alias the receiver, so its
  result is never a temporary.

  **A mutating call on a temporary.** The checker warns (as for the `Shared` read temporary,
  [`concurrency.md`](../concurrency/01-model.md) §6) when a mutating builtin call (`push`, `extend`, `sort`,
  `add`, `remove`, `update`, ...) or a field/index store lands on a value no name holds:
  - a slice of a builtin (`g[0..2]`, `g[r]`), `.reversed()`, `.copy()`, or another native
    `List`/`Map`/`Set`/`bytearray` method that returns a container
  - an operator result (`xs + ys`), a comprehension, a literal, a struct or variant constructor
  - a `Shared`/`RwShared`/`Atomic` read (`s.get()`), which keeps its own text

  The text is `'push' changes a temporary copy; the result is dropped: bind the value to a name first,
  or call 'push' on the source`. A write through an element of a shallow copy (`xs.copy()[0].push(1)`)
  is silent (the element is shared with `xs`). Silent too: a user struct method on a temporary, a store
  through a user `set_index` hook, a user `slice` hook, and a builtin constructor call (`List[int]().push(1)`).

`fn` is the only declaration allowed inside a block. `struct`, `enum`, `protocol`, `type`, `test fn`, `import`, `extern` and `native` are top-level only; inside any block each is a parse error (`struct must be a top-level declaration`).

```chezzi
fn main():
    n := 0
    fn fact(k: int) -> int:          # recursive nested fn
        if k <= 1:
            return 1
        return k * fact(k - 1)
    fn bump():                       # statement body → can reassign a captured local
        n = n + 1
    bump()
    bump()
    print(fact(5))                   # 120
    print(n)                         # 2 (write is visible in the enclosing scope)

main()
```

**Limits.**

- `fn` declarations nest at most **100** deep (a top-level `fn`, `test fn` or method is level 1). The
  101st level is a resolve error: `fn 'f100' is nested 101 deep; fn declarations nest at most 100 deep
  (declare it at an outer level)`, the same bound as CPython's indentation limit. Check time is not
  exponential in depth (a 30-deep chain checks in 0.04 s; see
  [`benchmarks.md`](../../contributing/benchmarks.md)).
- A nested fn may not be generic (`fn id[T](x: T)` inside a body is rejected).
- Mutual recursion between sibling nested fns is unsupported: a nested fn is in scope only after its
  declaration, so `a` calling a later `b` is `unknown name 'b'`. Declare such a pair at the top level.
- A nested fn may take any name a local may take, including a builtin (`ord`), a builtin ctor
  (`Channel`) or a same-module struct, and it shadows that name in its scope (as `def ord` in a Python
  function does).

### Built-in types

| Type | Example | Notes |
|------|---------|-------|
| `int` | `42` | 64-bit signed |
| `float` | `3.14` | 64-bit |
| `bool` | `true` | |
| `str` | `"hi"` | UTF-8 |
| `bytes` | `b"\x01AB"` | immutable byte sequence; `b[i]`→int, `b[a..b]`→bytes, iterates int; `Hashable` |
| `bytearray` | `bytearray([1,2])` | MUTABLE byte buffer (constructor-only); `ba[i]`→int, `ba[i]=x`, slice→bytearray, `push`/`pop`/`extend`; NOT `Hashable` |
| `List[T]` | `[1, 2]` | growable |
| `Map[K, V]` | `{"a": 1}` | insertion-ordered hash map; `K` is any `Hashable` type |
| `Set[T]` | `{1, 2, 3}` | deduped, insertion-ordered hash set; `T` any `Hashable` type; empty is `Set()` |
| `tuple` | `(1, "a")` | fixed-arity, immutable |
| `T!E` | `x` / `?x` / `!e` | §9; `T!` means `T!Error`; a `None!E` fn succeeds by falling off the end or by a bare `return` |
| `T?` | `?x` / `None` | §9; shorthand `T?` |

- Diagnostics print a type in shorthand: `int?`, `str!IoErr`, `int!` (E = `Error`), `!E`.
- `T!` is `T!Error` (`Error` is the built-in protocol).
- The prefix type `!E` is `None!E` (a bare `!` is `None!`): a fallible call with no value, as in
  `fn save(p: str) -> !IoErr`.
- As a type, `None` means "returns nothing". It is legal only as a return type (`-> None`,
  `fn() -> None`) and as the success side of `None!E`. Every value position (`List[None]`, `x: None`,
  `int!None`) is `'None' is not a value type`.
- The containers spell type and constructor in PascalCase: `List`/`Map`/`Set` (`List[int]`,
  `Set(xs)`). `tuple` stays lowercase.

**`T!= v` is `T! = v`.** The lexer reads an adjacent `!=` as one token. After a type it is the type's
`!` and then `=`: `y: int!= 6`, `y: const int!= 6`, a parameter `a: int!=5` and a field `a: int!= 5`
mean the spaced form. `g: fn() -> int!= one` has the type `fn() -> int!`. `!=` between two
expressions is unchanged (`x: bool!= a != b`), and `y: int!== 6` is a parse error.

**Success-coercion (implicit wrap).** At every typed slot a plain success value wraps: `T -> T?`
gives a present value, `T -> T!E` a success value. A bare `return` (or falling off the end) in a
`None!E` fn gives its success value.

- Typed slots: a typed binding, an assignment target, an argument, a field, a collection or tuple
  element, a conversion or box ctor element, a comprehension element, `return`, `yield`, an inline or
  closure body, a parameter or field default.
- `None` and error values (`!e`) stay explicit. A value that is already `T?`/`T!E` is never re-wrapped
  (`int??: return ?1` needs `?(?1)`).
- The wrap never chains onto the int→float rule (`float?: return 1` is an error).
- It declines at a slot that mentions a type parameter. An operator operand is not a slot, nor is the
  return of a fn with no `->`.
- Each branch of an if/match expression at a slot wraps on its own (§8): `x: int? = if c: n else: None`
  wraps only `n`. Full table: §9.

**Closure literals and generic slots are typed slots too.** A closure literal reads its expected type
through one carrier layer: `cb: (fn(int) -> int)? = fn(a): a + 1` types `a` as `int` and stores a
present value (at a binding, an argument, a field, an element and a `return`). A generic call's slot is
known when its type is written, by type arguments (`Box[int?](5)`, `opt[int?](5)`, `B[int?].mk(5)`,
`s.gm[int?](5)`) or by the annotation (`c: Box[int?] = Box(5)`); the argument then wraps and types its
closure parameters. With neither written, the type parameter is solved from the arguments and nothing wraps.

**The join rule.** Sibling values (the branches of an `if` / `match` value, the sides of `??`, the
elements of a list / set / map literal) must have one type. A value wraps into a carrier only under a
**written type** at the slot or a **written `None`** beside it.

- A `T!E` sibling types a bare `!e`: `if c: a() else: !"e"` is `a()`'s `int!str`.
- A `T?` or `T!E` beside a plain `T` with neither trigger is an error that names the fix: `[w, 3]`
  with `w: int?` is `list elements differ: int? vs int — a plain int joins int? only under a written
  type: annotate the binding (x: int? = ..., xs: List[int?] = [...])`.
- Two `T!E` values join to `T!E`; no join rewrites the error type.
- A `??` mismatch names the operator: `'??' sides have incompatible types: int and str`.

**A literal `None` joins the value beside it.** A `None` takes its payload type from its own
statement: `z := None` then `z = 7` is an error at `z := None`.

- With the type written, a plain value wraps: `z: int? = None` then `z = 7` stores `?7`;
  `xs: List[int?] = [None]` then `xs.push(7)` / `xs[0] = 3` gives `[3, 7]`.
- A literal `None` beside a value joins to that value's optional: `[None, 7]` is `List[int?]`,
  `{"a": None, "b": 7}` is `Map[str, int?]`, `if c: None else: 7`, a `match` with a `None` arm, and
  the inline body `fn pick(c: bool): if c: None else: 7` are `int?`. `[None, w]` joins to `w`'s type.
- Only the LITERAL `None` joins: with `n: int? = None`, `[n, 7]` is rejected, and so are `[w, 7]` and
  `[[None], [7]]`.
- `xs += [7]` on a `List[int?]` never wraps (write `xs += [?7]`).
- A generic call argument never wraps: `put(b, 7)` with `fn put[T](b: Box[T], x: T)` and
  `b: Box[int?]` is rejected.
- A set element is never a carrier, and an open `!e` is not inferred this way.
- `z := None` with no value beside it and no annotation is an error (see "Inference" in §10).

**No `int`→`float` widening at any slot.** An `int` expression, literal or not, is never accepted
where a `float` is expected: a typed binding, a reassignment / index / field assign, a call / method /
constructor / enum-payload argument, a `return` (a closure's too), a parameter or field default, a
collection element or map value, a `Channel[float]`/`Shared[float]`/`Atomic[float]` send or
constructor, a `yield`, a generic type-argument binding (`mx(1, 2.5)` with one `T`), or an
`if`/`match` branch join. The message names the fix:

```
x: float = 1              # cannot assign int to variable of type float — write 1.0 (or float(x))
l: List[float] = [1.5]
l.push(3)                 # argument 1 of 'push': expected float, found int — write 1.0 (or float(x))
```

- Write `1.0` for a constant (Rust's `let x: f64 = 1;` is E0308 too), or `float(i)` for a value.
- `float`→`int` is a type error too (lossy). A sink spelled through an alias (`type F = float`,
  `type LF = List[float]`) follows the same rule.
- Arithmetic and comparison on values still mix: `2 * 1.5` (`3.0`), `i * f`, `x + 1` with `x: float`,
  `f += 1`, `i < f`, `1 == 2.3` give `float` / `bool` (the VM promotes by runtime tag).
- A mixed literal (`[1, 2.5]`, `if c: 1 else: 2.5`) is an `int` and a `float` in one slot, so it is
  an error (`list elements differ: int vs float — write 1.0 (or float(x))`). A `float` slot always
  holds a real float, so `x: float = 3.0` makes `x / 2 == 1.5`.
- An `Any` slot keeps an `int` an `int` (`x: Any = if c: 1 else: 2.5` stores `1`;
  `xs: List[Any] = [1, -2.5]` keeps the `1`), as Go's `var x any = 1` does.
- *Protocol* widening is unrelated: `b: Box[Named] = Box(A())` binds `T` to a protocol the argument satisfies.
