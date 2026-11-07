## Language v1 — feature set

**Core:** `int float bool str`, `List[T]`, `Map[K,V]`, `Set[T]`, `tuple`, `fn`, `struct`, `enum`,
`if/else`, `for/while`, `T!E` & `T?` + `?`, closures (`fn(x): x*2`), built-in generics
(`List`/`Map`/`Set`/`T!E`).

**Carriers.** `T?` and `T!E` are ordinary enums that the prelude declares. User code writes the types
`T?` / `T!E`, the values `x`, `?x`, `!e`, `None` and the patterns `?v`, `!e`, `None`, so patterns and
exhaustiveness treat them like a user enum. Any module can import the variants of its own enum with
`import Red, Green from Color` ([`syntax.md` §12 "Variant import"](../syntax/16-imports.md)).
- `T!E` has two params, and `T!` = `T!Error` (E defaults to the built-in `Error` protocol).
  Diagnostics print types in this sugar (`int?`, `str!IoErr`, `int!`, `None!E`).
- `None` as a type means "returns nothing". A `None!E` fn (also written `!E`) succeeds by a bare
  `return` or by falling off the end.
- A plain success value at ANY typed slot (binding, assignment, argument, field, element, return,
  yield, default) wraps implicitly (`syntax.md` §9). Prefix `!e` builds an error value (`return !e`;
  `e` must satisfy `Error`) and prefix `?x` a present/success value of the expected carrier.
- A value that is already a carrier is never re-wrapped, there is no int→float step, and a slot that
  mentions an unpinned type parameter does not wrap.

**Included:**
- **Pattern matching**: `match` on enums (also int/float/str/bool + tuple + **struct** scrutinees; a float literal
  matches by `==`),
  exhaustiveness-checked.
  - A struct destructures **positionally** (`Point(x, y)` binds the fields in declaration order); a
    struct has one constructor, so a lone all-binding arm is irrefutable (no `_`). `S(..)` only sets
    the fields; a module `fn` named after the struct is a second binding of the name and an error
    (`syntax.md` §7a), so a constructor with logic is a static `S.new(..)`.
  - Nested patterns (incl. nested nullary variants like `?None`) + **or-patterns** (`p1 | p2`; every
    alternative must bind the same variables). A full enum or-pattern is exhaustive without `_`, and so
    is `true | false` on a bool scrutinee; the open int/float/str domains still require a `_`.
  - User-enum variants are **scoped under their enum** and must be written **qualified** as
    `Enum.Variant` (value, constructor, or `match` arm); a bare user-variant name is a compile error.
    Two enums may share a variant name (`Color.Red` / `Light.Red`). The built-in `?x`/`!e`/`None`
    stay bare.
- **String interpolation**: `"hi {name}, sum {a+b}"`. Python-style **format specifiers** follow a
  `:`: `{expr:[[fill]align][sign][z][#][0][width][.precision][type]}`, e.g. `{name:>10}`, `{f:.2f}`,
  `{n:04d}`, `{pct:.1%}`, `{255:x}`. Type chars: `d f x X b o e E g G %` (`f` and `e`/`E` are
  CPython-style with default precision 6; `e`/`E`'s exponent is signed and zero-padded to ≥2 digits).
  Plain float `print()` and `"{f}"` match CPython `repr` (scientific when the decimal exponent is `< -4`
  or `>= 16`). **Width and precision are capped at 4096** (a larger spec is a parse error). String `.N`
  truncates. An unknown type char or a type/value mismatch is reported before any output
  (runtime-prefixed; `check` does not catch it). A bare interpolated ternary works; parenthesize it to
  give it a spec (`{(if b: 1 else: 2):>5}`). One module (`src/fmtspec.rs`) parses and formats specs.
  Full grammar in [`syntax.md` §10](../syntax/14-strings.md).
- **Literal forms**: int (`42`, `0xFF`/`0b1010`/`0o17`, `_` separators); float (`3.14`, scientific
  `6.022e23`/`1e3`/`1.5e-9`; any exponent ⇒ float); str in `"…"` or `'…'` (same escapes and
  interpolation), also **triple-quoted** `"""…"""` / `'''…'''` (unescaped quotes allowed inside), with
  escapes `\n \t \r \\ \" \' \0` and `\u{HEX}` (1-6 hex digits); **raw** `r"…"` / `r'…'` / `r"""…"""`
  (verbatim: NO escapes, NO interpolation, braces literal). See `syntax.md §2/§10`.
- **Membership & assignment ops**:
  - `x in xs` (`bool`; list/set element, map **key**, str substring; a user struct/enum through the
    **`Contains[Item]`** protocol's `contains(self, item) -> bool`).
  - Compound assignment `+= -= *= /= %= &= |= ^= <<= >>=` means `x = x OP v` and type-checks exactly
    as that; the bitwise forms are `int`, and `& | ^` also work on two sets.
  - An operator returns a new value and never mutates an operand, so `xs := [1]; ys := xs; xs += [2]`
    leaves `ys` at `[1]`, and `s |= {2}` leaves an alias of `s` unchanged. In-place change is a method
    (`xs.push(v)`, `xs.extend(ys)`). CPython's `list +=` mutates; Chezzi's does not. A user type's
    operator hook may still mutate `self`, but by convention it should not.
  - Multi-target / tuple-swap assignment: `a, b = b, a`.
  - **Every** assignment form evaluates its right-hand side first (Python semantics), a plain
    `obj[i] = v` / `obj.f = v` included. See `syntax.md §3/§4`.
- **Struct methods**: `fn dist(self)` on structs. Composition + structural `protocol`s, no classes or
  inheritance.
- **Tuples**: `(1, "a")`, fixed-arity, immutable; nestable in patterns.
- **Transparent type aliases**: `type UserId = int`.
- **Bitwise ops**: `& | ^ << >>` (int-only).
- **`recover:` block**: panic-recovery boundary → `T!Error` that catches any runtime fault beneath it.
- **`panic(msg: str)`**: user-raised recoverable fault (bottom-typed; unwinds, runs `defer`s, caught by
  `recover:` as `!e`, else aborts).

**Also in the language:** user-defined generics + structural protocols (`Comparable`, `Eq`,
`Stringable`, `Hashable`, the operator protocols, protocol embedding, `Num`); generic enums and enum
methods; static methods and static protocol requirements through a bound; the `Iterator[T]` /
`Iterable[T]` protocols and `yield` generators; match guards + range patterns; slicing + indexing
protocols (`xs[a..b]`, negative indexing, `Index`/`IndexSet`/`Slice`); concurrency and `defer`; an
in-language test framework (`assert`, `test fn`, `chezzi test`, see [`syntax.md §9c`](../syntax/13-testing-and-entry.md)).
Milestone history: [`PROGRESS.md`](../../../PROGRESS.md).

**Non-goals (by design):** classes & inheritance (Chezzi is composition-only with structural
`protocol`s, like Rust/Go); variadic GENERICS (`Foo[T...]`; generics are always fixed-arity);
**spread/unpack syntax** (`[*a, *b]`, `{**m}`, `f(*args)`; list concatenation and map merge are plain
methods/operators); bignum.

**Variadic arguments** (`fn f(...xs: T)`, Go/Swift `T...` style): a variadic param collects the surplus
trailing positional args into a `List[T]`, sugar over "pass an explicit `list`".
- At most one variadic per signature; it must carry an element type and may not carry a default.
- Everything after it is **keyword-only** (a defaulted post-variadic param is an optional keyword arg, a
  defaultless one is required-by-keyword, like Python's `*args`).
- The desugar pass collapses the call (a synthesized `List` literal), so the backend sees an ordinary
  positional call.
- Used as a first-class **value** through a binding certain to hold that one fn, a variadic fn packs
  its surplus and fills its defaults like a direct call; any other value takes the collapsed `List[T]`
  slot (the fixed-value-form rule of `print`).

**`Any`**: the top type, an empty structural protocol satisfied by every type, scalars included. It is
the element type of a universal display slot (`print(...args: Any)`); it carries no methods, so it is
not dynamic typing. Any empty protocol (`protocol Name:` with a lone `pass` body) is an accept-all top
type the same way; `Any` is defined like that in the prelude. A checked downcast off `Any`,
`cast[T](val: Any) -> T?`, is **deferred** (it needs runtime type tags; see
[`roadmap.md`](../../design/roadmap.md)).

**Named arguments through a function value** (Swift-style labels: a `fn(...)` type carries its
parameter labels, so `g := greet; g(name="Bob")` works):
- Only through a binding that holds **one known function** (a `:=` of a fn or closure literal, or a
  nested `fn`, never reassigned). A keyword call through a HOF parameter, list slot or reassigned
  binding is a compile error (the callee's parameter names are not statically certain; pass
  positionally).
- Labels are **surface-only** (SE-0111): `fn(str)->None` ≡ `fn(name:str)->None`, so HOF/callback/
  protocol typing is unaffected.
- A value call must supply every parameter: declaration-site **defaults do not fill through a value**
  (a direct call still fills them), and built-in fn values take no keywords.
- The checker rewrites the keyword call to positional, so the runtime ABI stays positional.

**Concurrency.** Go's shared-heap model: `spawn` cheap tasks that share one heap + a `parallel:`
structured-concurrency nursery, `Channel[T]` (a send hands over the value by handle; unbounded `Channel[T]()`, rendezvous
`Channel[T](0)`, or bounded `Channel[T](cap)` with backpressure; `close`/`for v in ch`/`try_send`/
`cap`), `Shared[T]`, and `Executor`. `chezzi run` uses the OS-thread engine, the only engine (size its
worker pool with `--threads=N` / `CHEZZI_THREADS`, `0` = all cores). It is an **M:N work-stealing
scheduler** with reduction-counting preemption, a dirty/blocking pool for opaque blocking natives, and
an epoll/kqueue netpoller for non-blocking `std.net` TCP. Every function body and the module top level
is an **implicit nursery** that joins at its `return`/end, so a bare `spawn` is legal anywhere (an
explicit `parallel:` is an inner sub-nursery for earlier joins). Full design in
[`concurrency.md`](../concurrency/01-model.md).

**Cancellation semantics.** A cancel (a sibling's fault, an `os.exit`, a scope teardown) arrives at
**cancellation points**: **loop back-edges**, **blocking/park ops** (`recv`, `wait:`, socket ops,
blocking natives) and **native→user-code re-entries** (a `map`/`filter`/`fold`/`sort` callback). Not
at every instruction.
- A **task always runs its straight-line prologue**, so a `defer` it registers is always registered
  before anything can kill it, and always runs on the cancel unwind. Every spawned task starts, even
  into an already-cancelled scope.
- A CPU loop stays promptly cancellable (the back-edge is a checkpoint); **loop-free recursion is not a
  checkpoint** and runs to completion first (Trio's model). A run-wide halt (`os.exit`, a
  deadlock verdict) also lands at every function entry, so it stops a
  recursion at its next call.
- A **`defer` is never itself cancelled**: no checkpoint fires inside a deferred call, so every
  registered `defer` runs in full (LIFO). A `recover:` *inside* a defer body catches faults raised
  beneath it, so a panic in cleanup step 1 does not skip cleanup step 2.
- Cancelling a scope also cancels its **nested** scopes. A `recover:` *outside* the defer in a cancelled
  task never catches the cancel.
- `std.os.exit` is the one thing that skips `defer`s by design: it runs no `defer` in any task and cuts
  a `defer` body that is running (Go `os.Exit`).
- **Cross-task stdout order is nondeterministic** (one `print` = one locked, line-atomic write); the
  line *set*, the exit code and whether a `defer` ran are guaranteed.

**Still deferred:** macros, a package registry, and a native backend (a Cranelift AOT/JIT is the
stretch end-game). Dynamic C-ABI FFI (`extern "lib":`) is partly shipped; see
[Native FFI](04-semantics.md#native-ffi) and the open design in
[`ffi-and-packaging.md`](../../design/ffi-and-packaging.md).

**`yield` / generators.** Any `fn` that uses `yield` is a generator; calling it returns a suspendable
generator usable anywhere an `Iterator[T]` is (`for` loops, `Iterator[T]` bounds).
- The `-> Iterator[T]` return annotation is **required**, and every `yield` must be assignable to `T`.
  A generator with no `->` errors
  ``a generator function (one that uses `yield`) must declare a return type of `Iterator[T]` ``.
- A **numeric mix** under a written `Iterator[float]` (`yield 1`) is rejected: there is no
  `int`→`float` coercion at a `yield`.
- A **live** generator is one object in every task that holds it: passed or captured into a
  `spawn`, submitted to an `Executor` (M3.4), stored in a `Channel`/`Shared`/`RwShared`/`Atomic`
  (M3.2, M3.3), or reached through a module global (M3.1). A task drives the parent's generator, so
  the parent's `next()` continues where the task left it (CPython: a thread drives the caller's
  generator). Two tasks that drive it at once race (below).
- A generator is **not re-entrant**: resuming one that is *already running* (a `.next()` or `for` over
  it from inside its own body) raises the catchable `generator already running` (Python's
  `ValueError: generator already executing`), never a panic. The guard clears on every unwind path, so
  a generator never stays marked "running". A generator whose body faulted is **closed**: a later
  `.next()` → `None`.

Adapter structs over `Iterator[T]` (Rust's `Map`/`Take` model) remain the recommended way to write lazy
sequences.

**`Iterable[T]` protocol + `.iter()`.** `Iterable[T]` promises `.iter() -> Iterator[T]` (a fresh
composable cursor); `Iterator[T]` also promises `.next()`, so every `Iterator` IS `Iterable` (its
`iter()` returns self).
- Every built-in collection (`list`/`set`/`map`→keys/`str`→char/`bytes`/`bytearray`→int) has
  `.iter()`, which returns a cursor: a frozen snapshot of the collection plus a read position, typed as
  `Iterator[T]`, with `.next() -> T?` (a present value, then `None` on every later call). A plain
  `list` can therefore feed the same adapter pipeline as a hand-written iterator
  (`examples/iterable.chz`).
- A generator, a user `next`-struct, and a struct with only `iter(self) -> Iterator[E]` all satisfy
  `[S: Iterable[T]]`.
- `Iterable[T]` is usable in **type position** as well as a bound: `fn f(xs: Iterable[int])` accepts
  any of those, and its body may `for`/comprehend/`List()` it. Like every protocol existential it is
  strictly invariant in its args (`List[int]` → `Iterable[Any]` is rejected).
- The cursor is one object in every task that holds it (M3.4), like a `list`.
- There is **no** compile-time multi-pass safety (it needs move/ownership): each `.iter()` is a fresh
  cursor, but an exhausted one yields nothing.

**Capture is by reference.** Closure/`defer:`/`spawn:` capture is **uniformly by reference**: a
capturing frame shares the closest binding of a captured name, and reads and writes are live. A
captured **loop variable** gets a fresh cell per iteration (Go ≥1.22). A spawned task shares the
binding too (M3.4, Go): `spawn:` captures, `spawn f(xs)` arguments, a `spawn obj.m()` receiver and
an `Executor` job's captures are the parent's values, by handle. A module global is the run's one
copy (M3.1). Two tasks that write one value without a join, a `Channel` or a box (`Shared[T]`)
between them race, and a race is the program's bug. See `syntax.md` "Closure capture".

**A task's write lands on the parent's value** (Go; the old copy rule is history,
[ADR D4](../../design/adr-d4-airlock.md)): a captured local, a captured container, a spawn argument
and a spawned receiver are the parent's own, so the write is seen after the join, as in Go. No
compile-time error or run-time fault guards it.

### Syntax sketch

```chezzi
fn add(a: int, b: int) -> int:        # explicit params; '-> T' optional (inferred from body)
    return a + b

name := "chezzi"
print("hi {name}")                     # interpolation

struct Point:
    x: int
    y: int
    fn dist(self) -> float:            # struct method (needs: import std.math)
        return math.sqrt((self.x*self.x + self.y*self.y).to_float())

enum Shape:
    Circle(int)
    Square(int)

fn area(s: Shape) -> float:
    match s:                           # pattern matching, exhaustive
        Circle(r): return 3.14 * r * r
        Square(n): return (n * n).to_float()

fn safe_div(a: int, b: int) -> int!:
    if b == 0:
        return !"divide by zero"
    return ?(a / b)

fn main() -> int!:                     # must return `T!E` or `T?` to use `?` — no `fn main` exception
    r := safe_div(10, 2)?              # ? propagates the error to main's return
    nums := [x * 10 for x in [1, 2, 3, 4] if x % 2 == 0]   # list comprehension: [20, 40]
    print(nums)
    return ?0

main()                                 # no auto-entry — `main` is a normal fn you call yourself
                                       #   (its returned error would auto-raise at top level, rc=1)
```

**`pass`** is a reserved keyword that does nothing (so it cannot be a name).
- As a **statement** it is valid in any statement position (empty fn/method body, `if`/`for`/`while`
  body, statement `match` arm, concurrency block); a lone-`pass` body is identical to a lone `return`.
  It is not valid in a closure or an expression-match arm, EXCEPT a closure body right after `:`
  (`fn(): pass` is the no-op closure).
- As the **sole line** of a `protocol` body it makes a zero-method **accept-all top type** (how `Any` is
  defined).
- As the sole line of a `struct` body it makes a **zero-field struct** whose ctor `Name()` takes no args
  (it prints as `Name()`, equals another `Name()`, and is no map key unless it defines `hash` and `eq`; it still
  heap-allocates).
- An **empty enum** is not supported: an enum body with no variant is rejected (`pass`, methods only or `test` blocks only); an enum needs ≥1 variant.

**Multi-line literals.** Inside `[]`, `{}`, and `()` the lexer suppresses layout (newlines /
indentation), so collection literals, call arguments, and parameter lists may span lines. A single
optional trailing comma is accepted before the closer (`[1, 2,]` ≡ `[1, 2]`); a lone comma is still
an error. `(x)` is grouping; `(x,)` is a one-element tuple. (See [`syntax.md` §2](../syntax/01-lexical-and-literals.md) and the
collection/`<params>`/`<argList>` productions in [`grammar.bnf`](../grammar.bnf).)

**Entry model.** Programs run top-to-bottom; there is no automatic `main`.
- Only a top-level `?` that hits an `!e`/`None`, or a manifest `module:function` entrypoint whose entry
  fn *returns* `!e`/`None`, exits the program with `unhandled error: …` and a non-zero code.
- **A bare discarded `T!E`/`T?`** (at module top level, inside a function, or inside a `spawn:`/`defer:`
  block) is thrown away at run time, in every position alike. `chezzi check` **warns** on exactly those
  positions (Rust's `unused_must_use`; `r := …` / `_ := …` are the escapes). See
  [`syntax.md` §9](../syntax/12-errors.md) for the position table.
- `?` is valid at module top level (the runtime unwinds the propagated `!e`/`None` at the program
  boundary) and inside a `T!E`/`T?`-returning fn. A **None-returning fn (a `main` you write included)
  may not use `?`**: it would swallow the error.
- A bare `chezzi run` (no file argument) runs the project manifest's `[project] entrypoint`: a **dotted
  module path**, optionally suffixed with **`:function`** (e.g. `"src.main:main"`). The module runs
  top-to-bottom; with a `:function` suffix the entry function is then **called**, so the source needs
  no trailing call. The function part must be one name. `chezzi check` and bare `chezzi run` reject a
  name that the entry module never binds before any code runs (an error naming `chezzi.toml`); a name
  bound to a non-function is a run-time error.
- An entry function may be `-> T!` and use `?`; if it returns `!e`/`None`, `chezzi run` reports
  `unhandled error: …` (rc=1). The fault names the entry function's own declaration (the entry file
  for an ordinary `fn`, or wherever a module-global alias `main := helper` points); an entrypoint bound
  to a closure or a native fn keeps the bare `line 1, col 1`.
- Without the suffix the module runs top-to-bottom and no entry function is called. An explicit file
  (`chezzi run <file>`) is always top-level-only.
