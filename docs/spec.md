# Chezzi — Language Design Spec

A fast, statically-typed, Python-feel scripting language. Hand-built in Rust.

> **Status:** core language **implemented through M21 (still evolving; M19 perf in progress)** (scalars + collections, generics +
> structural protocols, exhaustive `match`, closures/HOF, modules, `Iterator[T]`, slicing/indexing,
> `defer` + `recover:`); **concurrency shipped through Tier-D** (`spawn` / `parallel:` nursery,
> `Channel`/`Shared`/`Executor`, real OS-thread M:N scheduler + netpoller + `std.net`). **M19** (a
> behavior-preserving perf track) is in progress — ~1630 tests passing. This doc is the source of truth
> for the *language design*; live build status lives in `PROGRESS.md` and the roadmap at the bottom.

## Goals (ranked)

1. **Self-contained** — the guts (lexer, parser, type checker, VM, GC) are hand-built on Rust `std` only: no transpiling, no codegen shortcuts, minimal dependencies.
2. **Usable tool** — bytecode VM (the original M5 baseline was ~10× over the then-existing, now-removed tree-walker); real modules so programs split into files.
3. **LLM-friendly** — static types as guardrails, explicit signatures, machine-readable compiler errors, small orthogonal grammar.

Closest existing cousins (read, don't copy): **Crystal**, **Nim**.

## Locked decisions

| Decision | Choice |
|----------|--------|
| Implementation host | **Rust** |
| Execution model | **Bytecode stack VM** (a tree-walk interpreter was the historical bootstrap, since removed) |
| Type system | **Static, local inference** (explicit param types; inferred locals *and* fn return types) |
| Surface syntax | **Indentation blocks** (Python-feel; lexer emits INDENT/DEDENT) |
| Errors | **Result/Option + `?`** (errors as values, no hidden control flow) |
| Code organization | **Composition, not inheritance** — structs + methods + interfaces (structural `protocol`s), like Rust/Go. No classes, no inheritance. |
| Memory | **Mark-sweep GC** (hand-built; primitives unboxed) |
| Name / ext / binary | **Chezzi** / `.chz` / `chezzi run foo.chz` |

## Language v1 — feature set

**Core:** `int float bool str`, `List[T]`, `Map[K,V]`, `Set[T]`, `tuple`, `fn`, `struct`, `enum`,
`if/else`, `for/while`, `Result[T, E]` & `Option[T]` + `?`, closures (`fn(x): x*2`), built-in generics
(`List`/`Map`/`Set`/`Result`). `Result[T, E]` is two-param: `T!` = `Result[T, Error]`, `T!E` =
`Result[T, E]`, `T?` = `Option[T]` (E defaults to the built-in `Error` protocol).

**Included:**
- **Pattern matching** — `match` on enums (also int/str/bool + tuple + **struct** scrutinees),
  exhaustiveness-checked. A struct destructures **positionally** (`Point(x, y)` binds the fields in
  declaration order); a struct has one constructor, so a lone all-binding arm is irrefutable (no `_`).
  Nested patterns (incl. nested nullary variants like `Some(None)`) + **or-patterns** (`p1 | p2`; every
  alternative must bind the same variables; a full enum or-pattern is exhaustive without `_`, but the
  open int/str/bool domains — including `true | false` — still require a `_`). User-enum variants are
  **scoped under their enum** and must be written **qualified** as `Enum.Variant` (value, constructor,
  or `match` arm); a bare user-variant name is a compile error. Because variants are per-enum, two
  enums may share a variant name (`Color.Red` / `Light.Red`). The built-in `Ok`/`Err`/`Some`/`None`
  (Result/Option) stay bare.
- **String interpolation** — `"hi {name}, sum {a+b}"`. First-class; string ops are a UX priority.
  Supports Python-style **format specifiers** after a `:` — `{expr:[[fill]align][sign][0][width][.precision][type]}`,
  e.g. `{name:>10}` (right-align width 10), `{f:.2f}` (2 decimals), `{n:04d}` (zero-pad), `{pct:.1%}`
  (percent), `{255:x}` (hex). Type chars: `d f x X b o e E %` (`e`/`E` scientific, CPython-style: default
  precision 6, exponent signed + zero-padded to ≥2 digits). Plain float `str()`/`print()` also matches
  CPython `repr` (scientific when the decimal exponent is `< -4` or `>= 16`). **Width and precision are capped at 4096**
  (a larger spec is a parse error — never a giant allocation). String `.N` truncates; an unknown type
  char or a type/value mismatch is reported before any output (runtime-prefixed; not caught by
  `check`). A bare interpolated ternary works; parenthesize to give it a spec (`{(if b: 1 else: 2):>5}`).
  The spec parser+formatter is shared by both engines (`src/fmtspec.rs`) → byte-identical output for
  well-formed programs. Full grammar in [`syntax.md` §10](syntax.md).
- **Literal forms** — int (`42`, `0xFF`/`0b1010`/`0o17`, `_` separators), float (`3.14`, scientific `6.022e23`/`1e3`/`1.5e-9` — any exponent ⇒ float), str in either `"…"` or `'…'` (interchangeable: same escapes & interpolation), also **triple-quoted** `"""…"""` / `'''…'''` (same escapes/interpolation, but unescaped quotes allowed inside) with escapes `\n \t \r \\ \" \' \0` and `\u{HEX}` unicode (1-6 hex digits), and **raw** `r"…"` / `r'…'` / triple `r"""…"""` (verbatim `str` — NO escapes, NO interpolation, braces literal; the escape hatch for the always-on `{…}`). See `docs/syntax.md §2/§10`.
- **Membership & assignment ops** — `x in xs` membership (`bool`; list/set element, map **key**, str substring; a user struct/enum via the **`Contains[Item]`** protocol's `contains(self, item) -> bool` method); compound assignment `+= -= *= /= %= &= |= ^= <<= >>=` (= `x = x OP v`; bitwise forms int-only); and multi-target / tuple-swap assignment `a, b = b, a` (RHS evaluated first). See `docs/syntax.md §3/§4`.
- **Struct methods** — `fn dist(self)` on structs. Composition + structural `protocol`s, no classes or inheritance (Rust/Go style).
- **Pipe `|>`** — functional chaining. Implemented in M6 (parse-time desugar to a call).
- **Tuples** — `(1, "a")`, fixed-arity, immutable; nestable in patterns.
- **Transparent type aliases** — `type UserId = int` (M10).
- **Bitwise ops** — `& | ^ << >>` (int-only, M8/M11).
- **`recover:` block** — panic-recovery boundary → `Result[T, Error]` catching any runtime fault beneath it (M11).
- **`panic(msg: str)`** — user-raised recoverable fault (bottom-typed; unwinds, runs `defer`s, caught by `recover:` as `Err`, else aborts) (M11).

**Shipped post-v1 (M7–M18):**
- **M7** — user-defined generics + structural protocols (generic fns/structs, `Comparable`; `std.cmp`).
- **M8** — tier-1 stdlib (`std.json`/`process`/`fs`/`time`), the `set` type, iterable strings (`s.chars()`).
- **M9** — tier-2 stdlib (`std.regex`, `std.request`) — first runtime crate deps.
- **M10** — type-system depth: `Stringable`/`Hashable` + operator protocols (`Add`/`Sub`/`Mul`), generic enums, type aliases, multi-bound generics (`T: Add + Mul`), any-`Hashable` map/set keys.
- **M11** — panic recovery (`recover:`) + user-raised `panic(msg: str)` (bottom-typed, unwinds, caught by `recover:` as `Err` else aborts) + Go-style `Result[T, E]` with the built-in `Error` protocol (`message(self) -> str`).
- **M12** — iterator protocol (structs with `next(self) -> Option[T]` iterable in `for`), match guards + range patterns.
- **M13** — `Iterator[T]`: the first **parameterized** protocol bound (`[S: Iterator[T], T]`) — any iterable, element type recovered; lazy adapter structs replace `yield`.
- **M14** — method-level type params (`fn map_to[U](self, …)`) + **user-defined parameterized protocols** (`protocol Container[T]`, concrete-arg bounds `[X: Container[int]]`, and first-class **value/annotation types** `c: Container[int]` — statically witnessed at the store/pass boundary, runtime-erased, strictly invariant, with method-return element recovery) — generalizing the special-cased `Iterator[T]`.
- **M15** — slicing + indexing protocols (Python-style `xs[a:b:c]` + negative indexing; `Index`/`IndexSet`/`Slice` structural protocols, built-ins intrinsic + user structs via `index`/`set_index`/`slice`).
- **M16–M18** — **concurrency** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, real OS-thread M:N engine via `--parallel`, netpoller + `std.net`) and the **`defer`** statement (call + block forms, `recover:`-integrated). See [`docs/concurrency.md`](concurrency.md).
- **M20** — **in-language test framework**: `assert <cond>[, "<msg>"]` (a both-engine statement primitive that faults with its source line), the `test fn` marker (free tests + struct **suites** with `before_all`/`after_all`/`before_each`/`after_each` lifecycle hooks + a shared typed fixture), and `chezzi test [path]` — a Rust-side, VM-only runner over `*_test.chz` files reporting `PASS/FAIL name (file:line) msg` with a non-zero exit on failure. See [`docs/syntax.md §9c`](syntax.md).

**Non-goals (by design, never):** classes & inheritance — Chezzi is composition-only with
structural `protocol`s, like Rust/Go (see *Locked decisions*). (**`yield`/generators** were once
listed here as a non-goal; they have since shipped as a complete VM-only coroutine runtime — see
below.)
**Variadics** — variadic ARGUMENTS **shipped** (`fn f(...xs: T)`, Go/Swift `T...` style): a variadic
param collects the surplus trailing positional args into a `List[T]`, so it is honest sugar over
"pass an explicit `list`". At most one variadic per signature; it must carry an element type and may
not carry a default; everything after it is **keyword-only** (a defaulted post-variadic param is an
optional keyword arg, a defaultless one is required-by-keyword — like Python's `*args`). The collapse
happens in the desugar pass (a synthesized `List` literal), so both engines see an ordinary positional
call. Used as a first-class **value**, a variadic fn takes the collapsed `List[T]` slot (no per-arg
spread through a value — the same fixed-value-form rule as `print`). Variadic GENERICS (`Foo[T...]`)
remain a **non-goal** — generics are always fixed-arity. The **`Any`** top type (an empty structural
protocol satisfied by every type, scalars included) is the honest element type of a universal display
slot (`print(...args: Any)`); it is not dynamic typing (it carries no methods). Empty protocols are a
**general** accept-all top type now that they are expressible (`protocol Name:` with a lone `pass`
body — see the `pass` keyword below): `Any` is defined that way in the prelude and any user empty
protocol behaves identically (the accept-all behaviour is structural, not keyed on the name `Any`). A checked downcast off
`Any` — `cast[T](val: Any) -> Option[T]` — is a **deferred** companion (design + runtime-erasure policy
in `docs/future.md`; parameterized targets like `cast[List[int]]` stay unsound until runtime type tags
exist). Default + named
arguments still cover most ergonomic cases. Named arguments also work through a first-class **function
value** (Swift-style labels: a `fn(...)` type carries its parameter labels, so `g := greet;
g(name="Bob")` and a `fn(name: str)->nil` HOF parameter both accept keywords). Labels are
**surface-only** (SE-0111) — `fn(str)->nil` ≡ `fn(name:str)->nil`, so no impact on HOF/callback/protocol
typing — and a value call is scope-cut: it must supply every parameter (declaration-site **defaults do
not fill through a value**; a direct call still does), and built-in fn values take no keywords.
Resolution is fully static (the checker rewrites the keyword call to positional), so the runtime ABI
stays positional and all engines agree. **Spread/unpack syntax** (`[*a, *b]`, `{**m}`, `f(*args)`)
is likewise dropped — list concatenation and map merge are served by plain methods/operators, not
new syntax.

**Concurrency — SHIPPED (Tiers A–D).** No longer deferred: Chezzi has a shared-nothing actor model
(`spawn` cheap tasks + a `parallel:` structured-concurrency nursery), `Channel[T]` (move-on-send,
unbounded `Channel[T]()` or bounded `Channel[T](cap)` with backpressure,
`close`/`for v in ch`/`try_send`/`cap`), `Shared[T]`, and `Executor`. `chezzi run` defaults to the real
OS-thread engine (size its worker pool with `--threads=N` / `CHEZZI_THREADS`, `0` = all cores);
`--serial` selects the cooperative engine (kept as the byte-identical parity oracle). The OS-thread
engine is a **M:N work-stealing scheduler** (reduction-counting
preemption, a dirty/blocking pool for opaque blocking natives, and an epoll/kqueue netpoller backing
non-blocking `std.net` TCP). **M-C implicit nurseries shipped** — every function body and the module
top level is an implicit nursery that joins at its `return`/end, so a bare `spawn` is legal anywhere
(an explicit `parallel:` is an inner sub-nursery for earlier joins). Full design
in [`docs/concurrency.md`](concurrency.md); phase history in
[`docs/concurrency-tier-d.md`](concurrency-tier-d.md) + [`docs/concurrency-b3.md`](concurrency-b3.md).

**Cancellation semantics (both engines).** A cancel — a sibling's fault, an `os.exit`, a scope teardown
— is delivered at **cancellation points**: **loop back-edges**, **blocking/park ops** (`recv`, `wait:`,
socket ops, blocking natives) and **native→user-code re-entries** (a `map`/`filter`/`fold`/`sort`
callback — that native's per-element Rust loop is its back-edge). Not at every instruction. So a
**task always runs its straight-line prologue**, which means a `defer` it registers is **always**
registered before anything can kill it and **always** runs on the cancel unwind — on the M:N engine and
on `--serial`. Every spawned task starts, even into an already-cancelled scope. A CPU loop stays
promptly cancellable (the back-edge is a checkpoint); **loop-free recursion is not a checkpoint** and
runs to completion first (Trio's model — pure CPU code is not interrupted). A **`defer` is never itself cancelled**: no checkpoint fires inside a deferred call, so every
registered `defer` runs in full (LIFO). Cancelling a scope also cancels its **nested** scopes. A `recover:` *inside* a
cancelled task never catches the cancel (a cancelled task must die). `std.os.exit` is the one thing that
skips `defer`s by design. Genuine deadlock is the one known limit (`docs/gaps.md` N5). **Cross-task
stdout order is nondeterministic on both engines** (one `print` = one locked, line-atomic write); the
line *set*, the exit code and whether a `defer` ran are what both engines agree on.

**Still deferred (YAGNI v1):** macros, package registry, and native backend (a Cranelift AOT/JIT is
the stretch end-game). **Level-3 dynamic C-ABI FFI is now partially shipped** (`extern "lib":` blocks
calling C functions via dlopen+libffi) — v1 marshals scalars (int/float/bool/str→`char*`), the
bidirectional **fixed-width integer** marshalling names `int8`..`uint64` (bind C `int32_t`/`uint32_t`/…;
truncate-on-param, sign-or-zero-extend-on-return; imported per-name from `std.ffi`), plus an
opaque `ptr` (↔ C `void*`, an untyped never-auto-freed handle for `FILE*`/`sqlite3*`-style APIs; the
`std.ffi` module adds `null()`/`is_null`), plus two return-only `str` opt-ins — **`owned_str`** (an
owned `malloc`'d `char*` copied **and** freed, no leak) and **`str?`** (a nullable `char*`, `NULL` →
`None` instead of a fault), plus **flat-scalar structs by value** (a Chezzi `struct` of scalar fields ↔
a C struct passed/returned by value, layout via libffi). `bool` marshals as C `_Bool` (1 byte) — the
int-returning predicate idiom (`isdigit`, …) binds `-> int` and tests `!= 0`. **Sync scalar callbacks
(#4) have shipped**: a function-typed extern param spelled `fn(scalars) -> scalar` (no new grammar)
marshals a Chezzi closure into a libffi trampoline C calls *back* synchronously during the extern call
(scalars only; faults are caught + re-raised — stronger than ctypes). **Pointer-deref builtins**
(`std.ffi` `load_*`/`store_*`) **and the C-buffer alloc layer** (`ffi.alloc`/`alloc_zeroed`/`free`,
libc-backed, manually freed) **have also shipped** — so `qsort`/`bsearch` of a Chezzi list now fully
works (alloc + `store_*` + a callback comparator + `load_*`). Nested structs-by-value, `str`
struct fields, **stored/cross-thread callbacks** (the rest of #4) and **varargs** (#5) — with design
notes + the callback feasibility ladder + a varargs fixed-arity workaround in
[`docs/ffi-and-packaging.md §1b`](ffi-and-packaging.md) — the rich Rust `Box<dyn Any>` userdata handle,
and a custom user-named deallocator (only libc `free` backs `owned_str`) are still deferred. See
the FFI subsection below + [`docs/syntax.md`](syntax.md).
**Forward design** for the remaining FFI deepening (the C `void*` `ptr` handle has **shipped**; the
rich Rust `Box<dyn Any>` "userdata" Value that unlocks compiled-in handle-based Rust libraries like
Burn is the open one) and the package registry (pure-Chezzi vs native packages; the
recompile-the-world / dynamic-plugin / C-ABI-wrapper models; the Rust-has-no-stable-ABI tax vs Python's
pip/wheels) is captured in [`docs/ffi-and-packaging.md`](ffi-and-packaging.md).

**`yield`/generators** were originally a deliberate non-goal (lazy sequences are written as adapter
structs over `Iterator[T]`, Rust's `Map`/`Take` model). They are now a **complete, VM-only**
feature: any `fn` that uses `yield` is a generator; calling it returns a
suspendable generator usable anywhere an `Iterator[T]` is (`for` loops, `Iterator[T]` bounds). The
`-> Iterator[T]` return annotation is **optional** — with no return type the element type `T` is
**inferred from the first `yield`** (strict-first-yield, just like late list `[]` element inference);
every later `yield` must be assignable to that `T`. Two cases are rejected at check time rather than
silently laundered: an **un-inferable** element (`yield []` with nothing pinning it, or a generator
that reaches no `yield`) errors `cannot infer generator element type; annotate the return type as
Iterator[T]`; and a **numeric mix** (`yield 1` then `yield 2.0`) is rejected — there is no `int`→`float`
coercion at a `yield`, so the second yield conflicts with the pinned `int` (annotate `-> Iterator[float]`
to opt in). An explicit `-> Iterator[T]` still overrides inference and validates every yield against `T`.
Generators run on **both** VM engines (serial `--serial` and the default M:N). A **live** generator held
in a frame **local** is now **sendable across a task airlock BY VALUE** (F3 path C): it is serialized —
its `proto`, backing closure, and parked operand-stack/args — and rebuilt as an **independent deep copy**
on the receiver (advancing one copy never affects the other), with every parked slot checked sendable at
serialize time (a non-sendable parked slot rejects at the crossing). A suspension **inside a `recover:`**
(a live handler stack) is ALSO sendable — its handlers are pure plain-data, serialized and rebuilt
coherently so the recover boundary resumes intact. The two remaining rejected shapes are both
**checker-unreachable** (no valid program constructs them) and kept only as defensive guards that reject
cleanly (a byte-identical-on-both-engines error): a suspension **with a pending `defer`** (`defer` is
banned inside a generator) and a **multi-frame** suspension (`yield` fires only in the generator's own
body frame). A generator held in a **module
global** crosses **BY VALUE too** (backlog item B): a task that reaches it gets its own independent
deep copy, exactly like a frame-local one (each task already snapshots every module global per-task, so
a per-task generator copy fits the model). The
adapter-struct model remains the recommended way to write lazy sequences. Live status is tracked in
[`PROGRESS.md`](../PROGRESS.md).

**`Iterable[T]` protocol + `.iter()`** (additive over the `Iterator[T]` iteration model, both
engines parity-clean). `Iterable[T]` promises `.iter() -> Iterator[T]` (a fresh COMPOSABLE cursor);
`Iterator[T]` additionally promises `.next()`, so every `Iterator` IS `Iterable` (its `iter()` returns
self). Every built-in collection (`list`/`set`/`map`→keys/`str`→char/`bytes`/`bytearray`→int) now
exposes `.iter()`, returning a cursor — a frozen snapshot of the collection plus a read position,
typed as the existing `Iterator[T]` existential (no new value type), with `.next() -> Option[T]` (Some,
then idempotent None). This lets a plain `list` flow into the same Take/Mapped adapter pipeline as a
hand-written struct iterator (`examples/iterable.chz`). A generator, a user `next`-struct, and a struct
with only `iter(self) -> Iterator[E]` (driven by a one-time `.iter()`) all satisfy `[S: Iterable[T]]`.
The cursor is **sendable** — it crosses the `spawn`/channel airlock as a deep copy (an independent
snapshot + position on the receiver), exactly like a `list`. A frame-holding **generator** is likewise
**sendable BY VALUE** — whether held in a frame **local** (F3 path C) or in a **module global**
(backlog item B): it crosses **any** task airlock as data — passed/captured into a `spawn`, stored in a
`Channel`/`Shared`/`RwShared`/`Atomic`, submitted to an `Executor`, or reached via a module global — as
an **independent deep copy** (its parked frame rebuilt on the receiver), with each parked slot
recursively wired so a non-sendable slot rejects at the crossing. Because every task already gets its own
frozen per-task copy of every module global, two tasks reaching the same module-global generator each
drive their **own** independent copy (and the parent keeps its own), on both engines byte-identically.
The reject shapes stay: a genuinely non-sendable parked slot (a `Module` handle, or a
>depth-cap acyclic nest), a value cycle threaded through the generator, and the three HARD-ARM parked
shapes (mid-`recover:` is now sendable; pending `defer` and multi-frame are checker-unreachable defensive
guards) all reject cleanly with a graceful, catchable `... cannot be sent across tasks` error, **never** a
panic, identically on both engines. (The earlier **Option-B reach-gate + poison→`nil`** model for
module-global generators is retired — safety is now provided by the by-value deep copy, which rebuilds a
fresh generator on the receiving heap and never shares a cross-heap handle, not by an inert `nil` leaf.)

A generator is likewise **not re-entrant**: resuming one that is *already running* — a `.next()` or a
`for` over the generator currently executing, reached from inside its own body — raises the same shape
of **graceful, catchable** runtime error (`generator already running`, Python's `ValueError: generator
already executing`), **never** a panic, identically on both engines. A live generator must never report
itself EXHAUSTED, so this is a fault, not a `None`. The guard is the resume path's own active-generator
root list, so it clears on every unwind path (yield, exhaustion, an early consumer `break`, a fault in
the body, a fault caught by an enclosing `recover:`) — a generator can never be poisoned as permanently
"running". A generator whose body faulted is **closed** (like Python's): a later `.next()` → `None`.

There is **no** compile-time
multi-pass/single-pass safety (unfixable without move/ownership): each `.iter()` is a fresh cursor, but
reusing an exhausted one yields nothing.

> **Migration — capture is now by reference (2026-07-09).** Closure/`defer:`/`spawn:` capture is
> **uniformly by reference**: a capturing frame shares the closest binding of a captured name and both
> reads and writes are live (was: a plain local snapshotted by value; a global was already live). A
> captured **loop variable** gets a fresh cell per iteration (Go ≥1.22). The one place sharing stops is
> the **task boundary**: a plain captured local — **and every module global** — sent across
> `spawn`/`parallel:` is snapshot-copied into an independent per-task view (F1 — the sole deliberate
> divergence from Go, byte-identical serial vs M:N; module globals are deep-copied per task at the spawn
> boundary on BOTH engines as of 2026-07-21, replacing the earlier frozen-module-global checker rule), so
> cross-task shared mutation still requires `Shared[T]` et al. Internally a captured local is
> boxed into a VM `Obj::Cell` (type-invisible — a boxed `x: int` still types as `int`). This reverses
> the earlier snapshot-by-value decision. See `PROGRESS.md` "Uniform by-reference capture" and `docs/syntax.md`
> "Closure capture".

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
        return math.sqrt(float(self.x*self.x + self.y*self.y))

enum Shape:
    Circle(int)
    Square(int)

fn area(s: Shape) -> float:
    match s:                           # pattern matching, exhaustive
        Circle(r): return 3.14 * r * r
        Square(n): return float(n * n)

fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err("divide by zero")
    return Ok(a / b)

fn main() -> int!:                     # must return Result/Option to use `?` — no `fn main` exception
    r := safe_div(10, 2)?              # ? propagates Err to main's Result
    nums := [1, 2, 3, 4]
        |> iter.filter(fn(x: int) -> bool: x % 2 == 0)   # pipe (needs: import std.iter);
                                                         #   a leading `|>` continues the line
        |> iter.map(fn(x: int) -> int: x * 10)
    print(nums)
    return Ok(0)

main()                                 # no auto-entry — `main` is a normal fn you call yourself
                                       #   (its returned Err would auto-raise at top level, rc=1)
```

**`pass` — the no-op keyword.** `pass` is a reserved keyword that does nothing. As a **statement** it
is a no-op valid in any statement position (empty fn/method body, `if`/`for`/`while` body, statement
`match` arm, concurrency block) — a lone-`pass` body is identical to a lone `return` (runs, falls off
the end, returns `nil`). It is statement-only, so it is not valid in a closure or an expression-match
arm (a no-op closure is `fn(): nil`). As the **sole line** of a `protocol` or `struct` body it is an
empty-body marker: `protocol Name:` + `pass` is a zero-method **accept-all top type** (structural ⇒
satisfied by every type — this is how `Any` itself is defined, and any user empty protocol behaves
identically), and `struct Name:` + `pass` is a **zero-field struct** whose ctor `Name()` takes no args
(it prints as `Name()`, structural-equals another `Name()`, and is intrinsically `Hashable` so it can
key a `Set`/`Map`; it still heap-allocates like every Chezzi value — no zero-size trick). An **empty
enum** is *not* supported (`pass` in an enum body is rejected — an enum needs ≥1 variant). Because it
is a real keyword, `pass` cannot be used as a name.

**Multi-line literals.** Inside `[]`, `{}`, and `()` the lexer suppresses layout (newlines /
indentation), so collection literals, call arguments, and parameter lists may span lines. A single
optional trailing comma is accepted before the closer (`[1, 2,]` ≡ `[1, 2]`); a lone comma is still
an error. `(x)` is grouping; `(x,)` is a one-element tuple. (See [`syntax.md` §2](syntax.md) and the
collection/`<params>`/`<argList>` productions in [`grammar.bnf`](grammar.bnf).)

**Entry model.** Programs run top-to-bottom; there is no automatic `main`. An `Err`/`None` left
unhandled at the top level (a bare expression statement, or a top-level `?`) exits the program with
`unhandled error: …` and a non-zero code. `?` is valid at module top-level (the runtime unwinds the
propagated `Err`/`None` at the program boundary) and inside a `Result`/`Option`-returning fn — but a
**nil-returning fn (including a `main` you write) may not use `?`**: it would silently swallow the
error (there is no `fn main`/entrypoint exception — a fn must return `Result`/`Option`). A bare
`chezzi run` (no file argument) runs the project manifest's `[project] entrypoint` — a **dotted module
path**, optionally suffixed with **`:function`** (e.g. `"src.main:main"`). The module runs
top-to-bottom like any other file; with a `:function` suffix the entry function is then **called** (a
missing/non-function name is a clear error), so the source needs no trailing call. An entry function
may legitimately be `-> T!` and use `?`; if it returns `Err`/`None`, `chezzi run` surfaces it as
`unhandled error: …` (rc=1), symmetric with the unhandled-top-level rule. Without the suffix the module
just runs top-to-bottom (no entry function is called). Running an explicit file (`chezzi run <file>`)
is always top-level-only.

## Imports & module resolution

Grammar — dot paths, `import..from`, alias at both levels, **no** `from..import`:

```chezzi
import std.io                       # whole module → io.read()
import std.io as fs                 # module alias → fs.read()
import read, write from std.io      # named (no braces — indentation lang)
import read as r from std.io        # named + alias
```

Resolution — **optional root marker**, kills Python's run-relative footgun:

1. Pick the run's **origin**. For `chezzi run <file>` it's that file. For a bare `chezzi run` (no file
   → the manifest entrypoint) it's the *cwd* — the directory you launched from.
2. Walk *up* from the origin for the **nearest** `chezzi.toml`. Found → that dir is root. **Not found
   → the script's own dir is root** (`run <file>`) / an error (bare `run` needs a manifest).
3. `std.*` is reserved → resolves to the **stdlib**, whose source is read from `$CHEZZI_STD` if that
   env var is set (a dev override: "use *this* tree", exclusive — a module missing from it is an
   error, never a silent fall-back), else from the **stdlib baked into the binary** (`std/*.chz` is
   `include_str!`'d at compile time, like the `docs/*.md` topics). So an installed `chezzi` is
   self-contained: it needs no source checkout, and moving or deleting the repo cannot break
   `import std.*`. Note the flip side: a **pre-built** binary plus an edited `std/*.chz` is stale
   until it is rebuilt (`cargo run`/`cargo test` rebuild automatically; otherwise use `$CHEZZI_STD`).
4. `a.b.c` → `<root>/a/b/c.chz`. **No `./` relative imports.**

Single-file scripts need zero config (Deno/Bun/Go model); `chezzi.toml` only matters once a project spans multiple files.

**Malformed graphs fail with a clean diagnostic, never a host crash.** Import **cycles** (`A↔B`,
self-import) are a clean error (Go-style), not lazy resolution. A pathological **acyclic** chain is
likewise bounded: a transitive import chain deeper than **256** modules is rejected with
`import chain too deep (exceeds 256)` attributed to the offending import, rather than recursing until
the host stack overflows. The limit is a pathological-depth backstop far above any real project
(diamond re-imports dedupe and do **not** count toward depth); tens/hundreds of modules are entirely
unaffected.

**One root governs the whole graph, and it is computed exactly once per run.** A single `chezzi` run
derives its module-graph root **once** — the nearest `chezzi.toml` walking up from the run's origin
(step 1–2) — and that same root resolves **every** import in the program (`a.b.c` → `<root>/a/b/c.chz`,
step 4), no matter which module the `import` appears in. Crucially, the *same* root that locates the
entry file also resolves its imports; the two can never disagree (an earlier bug where a bare
`chezzi run` re-derived a *second* root from the entry file — and so could silently load the wrong
same-named module — is fixed).

The origin differs by how you launch:

- `chezzi run <file>` → origin is the file; root = nearest `chezzi.toml` **above the file**.
- bare `chezzi run` (manifest entrypoint) → origin is the **cwd**; root = nearest `chezzi.toml`
  **above the cwd** (the manifest that declares the `[project] entrypoint`). Both the entry file and
  its imports resolve against *that* manifest's dir, so a bare run is **cwd-invariant** across any
  subdirectory that shares the same nearest manifest.

**Nearest-marker wins (sub-packages).** Because the root is the *nearest* marker, a **nested**
`chezzi.toml` in a subdirectory *is* a real boundary: it becomes the root for any file **run from
beneath it** — exactly Go's `go.mod`, Cargo's `Cargo.toml`, and npm's `package.json` sub-package
semantics. So `chezzi run vendor/lib/tool.chz` treats `vendor/lib/` as its root (if it holds a
`chezzi.toml`) and its `import util` resolves to `vendor/lib/util.chz`. A bare `chezzi run` from the
*outer* project, by contrast, is one run rooted at the outer manifest: it does **not** consult a
nested marker for that run's graph (just as `cargo run` at a workspace member uses that member's
manifest, not a nested one it happens to contain). Keep this in mind when vendoring: a subtree is a
sub-package only for runs that originate inside it.

**Types are module-scoped (Python-style).** A `struct` / `enum` / `type` alias is private to its
declaring module — every top-level type is exported by default (like functions; no `pub`), and is
reachable elsewhere **only via import**, accessed by the same bound last-segment name a function uses:

```chezzi
import core.geo                      # binds `geo`
p: geo.Point = geo.Point(1, 2)       # qualified construction + annotation
c := geo.Color.Red                   # qualified enum variant
xs: List[geo.Point] = []             # qualified type inside a generic

import Point from core.geo           # named import → bare use
q := Point(3, 4)                     # bare construction
import Point as Pt from core.geo     # rename on import (user types only)
```

A bare use of a type whose module was imported whole (`import geo`) but not named-imported is a
**check-time error** (`unknown type 'Point'; import it from geo`). Two modules MAY declare the same
type name with no collision — each is reachable from its own module. This gate is only on
*naming/constructing* the type. **Reading a field or calling a method off a VALUE of that type always
works, import-free** — member resolution keys off the value's own module-scoped identity, not whether
the type name is in the current module's scope. So a named import of a factory *function* alone
(`import make from geo`, `w := make()`) still resolves `w.x` / `w.bump()` even though bare `Point`
would be an unknown type (matches the module-owned `Match`/`Response`/`ProcResult` rule below).

**Identity is separate from display.** Every user `struct` / `enum` / variant / `type` alias has ONE
canonical **identity key** — always module-qualified, `<module-key>::Name` (the module key is the
declaring module's dotted path, or the entry file's stem) — used uniformly as the runtime type tag and
as the key into every layout table (construction, field/method resolution, `match`-pattern variant
ids, `json.decode` targets, the `--parallel` wire/snapshot format; checker, compiler, and both engines
derive it identically). Because the key is unique by construction there is **no** collision special-
case: two modules' `Point` are simply `a::Point` and `b::Point`. The **display name** is the bare
`Name`, carried separately on the type's def: all user-facing output — print/`str`, error messages,
`json.decode` errors, `repr` — renders the bare name, so output is byte-identical regardless of module
and two colliding `Point`s **both** print `Point(...)` (Python-like; the module is never shown in
normal output). JSON *encode* likewise emits the bare field/type naming (no `module::` leaks into the
wire). Reserved/native types (`Result`/`Option`/`Some`/`Ok`/…, `Iterator`, the std library type
surface on `import std.*`, and the FFI width names like `int32`) are **not** module-keyed — they keep
their bare name globally. An imported `type` alias is **transparent**: its body is resolved in the
*defining* module's scope, so a cross-module `import Len from sizes` where `type Len = int32` carries
its FFI-width license.

`chezzi init [dir]` scaffolds a new project (`chezzi.toml` + `src/main.chz` + an example `*_test.chz`).
The generated `chezzi.toml` is **both a root marker and a parsed manifest**: the resolver checks for
its *presence* to fix the root, and the toolchain parses its `[project]` keys. `name`/`version` are
metadata; **`entrypoint`** (a dotted module path + optional `:function`, scaffolded active as
`"src.main:main"`) is what a bare `chezzi run` executes — the `:main` suffix calls `main` so the
scaffolded `src/main.chz` needs no trailing call. The parser is a tiny fixed-schema reader
(`[section]` headers, `key = "value"`
string pairs, `#` comments); unknown keys/sections are ignored, and an empty `chezzi.toml` is a valid
root marker (all fields default to unset, so `entrypoint` is required only for the no-file `chezzi run`).

## Standard library

- **Builtins (no import):** `print`, `range`, casts (`int()`/`str()`/`float()`),
  `ord`/`chr`, `Set()`/`Set(list)`, `panic(msg)` (raise a recoverable fault), core-type methods
  (`s.upper()`, `s.chars()`, `xs.push()`, `m.get()`, `set.add()`). The **universe builtins**
  `ord`/`chr`/`panic` (first-class `native fn`) and `int`/`float`/`str`/`bytes`/`bytearray`
  (non-first-class `native ctor`) now declare their **signatures in Chezzi**, in the always-linked
  **`std/prelude.chz`** (see §"`native fn`/`native ctor`" in `syntax.md`) — the engine binds their bodies
  by name. `print` (variadic) and `range` + the `List`/`Map`/`Set` container ctors stay engine-synthetic.
  A `native fn`/`native ctor` decl is **prelude/std-only** (rejected in user code): an internal analog of
  `extern`, with the same-scoped relaxation that an unannotated param / missing `-> ret` means the
  dynamic/native type — Chezzi user code stays statically typed (no user-facing `any`/`never`).
- **Std modules v1 (shipped, M6c):** `std.math`/`std.io`/`std.os` (native-Rust via the FFI seam),
  `std.string` + `std.cmp` (written in Chezzi; `std.cmp` adds M7-G3). Imported with
  `import std.math` / `import f from std.io`. `std.cmp` holds generic `min`/`max`/`clamp`
  (`[T: Comparable]`); `list.sort()` is likewise Comparable. (`std.math.min`/`max` were retired into
  `std.cmp`; `abs` stays native.) **Integer overflow policy:** the one integer type is `i64`; every
  overflow — arithmetic (`+ - * / %`), left shift (`<<`, when a significant bit is shifted out),
  negation, `MIN / -1`, `MIN % -1` (both trip the same `i64` checked-op overflow — so `MIN % -1`
  faults rather than yielding Python's `0`), `math.abs(MIN)`, and integer `List.sum()` (checked add) —
  is a *recoverable
  panic* (`"integer overflow in <op>"`, catchable by `recover:`), never a silent wrap and never a host
  crash. **Float arithmetic is total IEEE-754** (the policy diverges by type): a `float` op *never*
  faults — `1.0/0.0` is `inf`, `-1.0/0.0` is `-inf`, `0.0/0.0` and `5.0%0.0` are `NaN`, and
  `math.sqrt(-1.0)` is `NaN`. `inf`/`NaN` are ordinary values; inspect them with `math.is_nan` /
  `math.is_inf` / `math.is_finite` (`import std.math`). Only **integer** arithmetic faults (overflow,
  `/0`, `%0`). (Casting a non-finite float back to `int` — `int(1.0/0.0)` — still faults: `inf`/`NaN`
  have no integer value.) **Ordered comparisons involving `NaN` are total too:** `< <= > >=` against
  a `NaN` always evaluate to `false` (never a fault), matching IEEE-754 / Python / Rust; equality is
  unchanged (`nan == nan` is `false`, `nan != nan` is `true`). Sorting is deterministic with `NaN`:
  `sort()` and `sort_by_key` use a total order (`f64::total_cmp`, `NaN` sorts to one end) instead of
  faulting. **One-way `int`→`float` widening — UNTYPED CONSTANTS only (Go's rule):** an untyped int
  *constant* expression adapts to a `float` context and is converted to a real `f64`; a **typed** `int`
  *value* never implicitly converts (write `float(x)`), and the reverse is always a lossy type error. An
  untyped int constant is an int literal, unary `-`, and `+ - * / %` composed over those — anything with
  a declared type (a name, a CALL RESULT, a field, an index) is typed and is rejected at a `float` sink
  with a diagnostic naming the fix. It fires at every value-definition boundary: a typed binding
  (`x: float = 1 + 2` so `x / 2 == 1.5`, real float division), a `float` function/method parameter
  (coerced at the callee prologue, from the DECLARED param type — so a call through a function VALUE
  never widens: `f := id[float]`; `f(1)` is an error, write `f(1.0)`), a `float` parameter DEFAULT
  value (`fn g(a: float = 3)`), a `-> float` return, a `float` struct field, native/`extern` `double`
  params, a **mixed-numeric-constant** collection (a list/map literal with ≥1 untyped float constant
  infers `List[float]`/`Map[_, float]` — `[1, 2.3]`, `[1, -2.5]`, `[1 + 1, 2.5]`), a **mixed-numeric-constant
  if/match EXPRESSION** (an untyped int-constant tail branch beside a float-constant sibling branch widens
  to `float` — `x := if c: 1 else: 2.5`, `match n: 0: 1; _: 2.5` — the same peephole, consistent with the
  `[1, 2.5]` literal; a TYPED int branch does NOT adapt, and this is a property of the EXPRESSION, distinct
  from un-annotated multi-`return` merge below which still conflicts), or an annotated
  `xs: List[float] = [1, f]` / `[1, 2]` (the annotation is the type context — spelled as a `List[…]`/
  `Map[…]`; a whole-collection alias `type LF = List[float]` is not a type context, an aliased ELEMENT
  `List[F]` is). A scalar `float` sink spelled through a type ALIAS (`type F = float`) is a float sink
  like any other (the backend resolves the alias, and a generic type param of the same name shadows it).
  The sink must be DECLARED `float`: a generic-erased slot (a method param declared `T` on a `Box[float]`)
  and a variadic `float` param's all-int-constant pack (`fn f(...zs: float)`; `f(1, 2)`) do NOT adapt —
  the backend has no declared `float` to coerce from.
  The element widening belongs to the LITERAL, so it also fires where the element type is not `float`
  (`xs: List[Any] = [1, -2.5]` stores `1.0`) — checker and backend agree there too. The compiler emits a real conversion
  (`Op::CoerceFloat`) so the checked path and the parity harness are byte-identical across both engines.
  The checker's accepted set is a strict SUBSET of what the type-blind compiler can coerce (one shared
  predicate, `ast::const_num`), so no sink can hold a runtime `Int` under a static `float`. Lossy
  conversions stay type errors (`y: int = 2.3`, `-> int: return 2.3`, `float` into `List[int]`,
  `int`→`float` across a **newtype** boundary). Widening is **scalar-at-the-sink**: a compound/nested
  float annotation is NOT widened — `List[List[float]] = [[1]]`, `float? = Some(3)`, `float! = Ok(3)`, and
  a non-literal RHS (`List[float] = f()`) all stay type errors (use explicit floats or a literal). An un-annotated mixed collection with a TYPED int element
  (`a := 1; xs := [a, 2.5]`) is an error — no type context, no adaptation; annotate AND write
  `float(a)`. One further restriction: a plain reassignment `x = 3` to a `float` local is rejected
  (type-blind target). The same scalar-only rule governs
  **un-annotated multi-branch return inference**: sibling `return` branches merge with a join. It does
  **not** widen `int`→`float` across branches — an inferred return is not a widening *sink* (widening
  emits `Op::CoerceFloat` only at an explicit sink), so mixed `if c: return 1 else: return 2.0`
  **conflicts**; annotate `-> float` to opt in. `return Ok(1)` / `return Ok(2.0)` likewise conflict (no
  widening inside a merged type-arg slot — the `float! = Ok(3)` error above). The `Result` **error
  slot** defaults to the built-in `Error` protocol when it is un-pinned or its payload **satisfies
  `Error`** (`return Err("a")` + `return Ok("h")` infers `Result[str, Error]`, not `Result[str, str]`,
  because `str` satisfies `Error`; two distinct **sendable** `Error`-satisfying payloads across branches
  unify to `Error` rather than conflicting). A concrete payload that does **not** satisfy `Error` — **or
  satisfies it but is not sendable** — is preserved (not laundered into the `Error` existential); a
  deliberate concrete error type is spelled explicitly (`-> Result[str, str]` / `-> int!DbErr`). The
  **every** protocol existential is **sendable** (Go `chan interface` parity, Task 2): `Channel[Error]`,
  `Channel[int!]`, and `Channel[Drawable]` over any user protocol all type-check — the erased witness
  crosses the airlock by deep value copy, and the concrete witness's own sendability is checked at each
  widening site (a non-sendable witness is rejected there, not laundered). A witness that genuinely
  can't serialize (one carrying a `Module` handle — native/FFI *fn values* now cross by value) is
  rejected at the **runtime airlock**, not at construction. See [`docs/syntax.md`](syntax.md) "Return type inference".
  No `byte`/`u8` scalar (Python model — binary data is the immutable `bytes` *sequence* type, **shipped**, not a
  scalar) and no bignum (a non-goal). **`bytes`** is a heap byte sequence (`b"..."` literal with
  `\xHH` escapes): `b[i]` -> `int` 0-255 (Index protocol), `b[a:b:c]` -> `bytes` (Slice protocol, byte
  offsets), `for x in b` yields `int`, `b.len()` is the byte count, `==`/`!=` are structural, and
  `bytes` is `Hashable` (valid map/set key). `str(b)` / `print(b)` / interpolation use the Python
  `b'...'` repr. Immutable (no `b[i] = x`). **`bytearray`** is the **mutable sibling** (Python
  `bytearray` model), **shipped**: constructor-only (`bytearray()` empty, `bytearray(N)` N zero bytes,
  `bytearray(b)`/`bytearray([ints])` from a bytes/List[int]) — no `ba"..."` literal. `ba[i]` -> `int`,
  `ba[i] = x` mutates in place (`IndexSet`; value 0–255), `ba[a:b:c]` -> a new `bytearray`,
  `for x in ba` yields `int`, `len`, `.push(int)` / `.pop() -> Option[int]` / `.extend(bytes|bytearray|
  List[int])`, `==` structural (incl. cross-type `bytes == bytearray` content-equal, Python parity).
  `bytearray` is **NOT** `Hashable` (mutable ⇒ not a map/set key, like `list`); its repr is
  `bytearray(b'...')`. The conversion bridge moves between the forms: `bytes(ba)` snapshots,
  `bytearray(b)` copies. Crosses the `--parallel` airlock by value (deep copy, like `list`).
  **str ↔ bytes (UTF-8), shipped:** `str.encode() -> bytes` UTF-8-encodes (always succeeds — `str` is
  UTF-8 internally); `bytes.decode() -> str` / `bytearray.decode() -> str` UTF-8-decode, faulting
  **recoverably** (catchable by `recover:`) on invalid UTF-8 (never a panic). UTF-8 **only** — no
  encoding-name argument. Remaining non-goals: a `byte`/`u8` scalar, non-UTF-8 codecs (latin1/utf16)
  and base64/hex/sha (a separate `std.*` gap), and byte-sequence methods beyond the tables + Display.
- **Std modules — M8 (shipped):** `std.json` (pure-Chezzi `Json` enum + `parse`/`stringify`/
  accessors **and** type-directed `json.decode[T](s)` into a struct/map/list/scalar);
  `std.process` (`cmd(s) -> Result[str]`); `std.fs` (`list_dir`/`exists`/`is_file`/`is_dir`/
  `size`/`glob`); `std.time` (`now`/`monotonic`/`sleep_ms`/`format`). Plus the **`set`** type
  (`{a, b, c}`), **`s.chars()`** + iterable strings (Python-style; no `char` type).
- **Std modules — M9 (shipped):** `std.regex` (the `regex` crate; stateless `is_match`/`find`/
  `find_all`/`replace_all`/`split`, returning a `Match` struct `{text, start, end, groups}` — spans
  are codepoint offsets, so `subject[m.start:m.end] == m.text`); `std.request` (blocking HTTP/HTTPS via `ureq`+rustls; `get(url)` /
  `post(url, body)` returning a `Response` struct `{status, body, headers: Map[str,str]}`, where a
  ≥400 status is a normal `Response`, not an `Err`). These are Chezzi's **first runtime
  dependencies**. Both are **synchronous/blocking** (the language is single-threaded — see below).
  `Match`/`Response` (and `ProcResult` from `std.process`) are **module-owned** struct types, not
  program-global reserved names: reading their fields off a returned value works import-free, but
  naming/constructing the type requires importing the owning module (a user struct of the same name,
  without that import, is the user's own type). The native seam grew `NativeRet::Struct`/`Map` so a
  native fn can return a structured value.
- **Shipped since (M10):** generic enums; the `Stringable` protocol (custom `str(x)`); the `Hashable`
  protocol — any `Hashable` type is now a valid map/set key. A key holding a genuine **reference
  cycle** faults *recoverably* on membership/key-equality (`Set.has`/`add`, `Map` get/insert/remove,
  `in`, set algebra, `List.contains`/`index_of`/`unique`/`dedup`) with `"maximum structural depth
  (10000) exceeded"` — the SAME fault `==` raises (container key-equality is defined by `==`), matching
  Python's `RecursionError`; catch it with `recover:`.
- **Shipped:** the scalar types `int`/`float`/`bool`/`str` now **intrinsically satisfy `Stringable`**
  (a `[T: Stringable]` generic accepts them, and the erased body's `v.str()` dispatches to the scalar
  render), closing the last inconsistency where every other scalar-friendly builtin protocol
  (`Comparable`/`Hashable`/`Add`/…) already had an intrinsic scalar arm but `Stringable` did not.
- **Shipped (bug-hunt wave-6 W6-3):** *every* intrinsically-granted protocol method is now **callable**
  from an erased generic body, not just `compare`/`str` — `add`/`sub`/`mul`/`div`/`mod`/`neg` on
  `int`/`float`/a numeric `newtype`, `hash` on `int`/`str`/`bytes`/`bool`/a zero-field struct, and
  `index`/`set_index`/`slice` on `list`/`map`/`str`/`bytes`/`bytearray`. Each dispatches to the **same
  primitive its operator form uses**, so `a.add(b)` ≡ `a + b`, `c.index(k)` ≡ `c[k]`, `c.slice(…)` ≡
  `c[a:b:c]` and `x.hash()` is exactly the hash `x` gets as a map/set key — same values, same faults.
  The checker↔runtime pairing is machine-checked per **(protocol × receiver type)**
  (`checker::proto::INTRINSIC_PROTO_METHODS` + `vm::tests::intrinsic_grants_all_have_vm_arms`, which
  sweeps the whole cross product), and a bare `return Ok(())` grant no longer compiles — so neither a
  new grant nor a WIDENED one can ship without its arm. Three documented exceptions: `Iterator`'s
  stateful `next` on a *raw* collection (no cursor position, W6-3b), `compare` on a NaN operand (no
  "unordered" int, W6-3c), and a numeric `newtype` that defines its own operator-named method (the
  method form gets the user method, the operator form the underlying's native op, W6-3d).
- **Shipped since (post-M18 stdlib batch):** `std.request` custom headers + non-GET/POST verbs
  (`put`/`patch`/`delete`/`head` + a general `request(method, url, body, headers)`), carried off-heap
  via a new `NativeArg::Map` so the headers form stays blocking-pool-offloadable under `--parallel`;
  `std.math` trig/exp/log intrinsics (`sin cos tan asin acos atan atan2 exp ln log2 log10 log`);
  pure-Chezzi `std.string` (`ends_with index_of count replace strip_prefix strip_suffix`) and `std.iter`
  (`take drop any all find flatten`) helpers.
- **Shipped:** whole-string `std.regex` (`is_match`, `find`, `find_all`, `replace_all`, `split` —
  each takes the pattern as a string, compiled behind an internal cache). **Later:** a first-class
  *compiled* `Regex` handle value (compile once, reuse) — still blocked on Level-3 **Userdata** below.
- **Shipped:** **enum methods** — `fn name(self, …)` blocks after an enum's variants, mirroring struct
  methods end-to-end (name-resolved dispatch, generic-enum type params in scope, structural-protocol
  satisfaction so an enum can define `str`/`hash`/`add`/`compare` for `Stringable`/`Hashable`/`Add`/
  `Comparable` and pass into protocol-bound generics, and `+`/`-`/`*`/`<` operator overloading).
- **Shipped (M21):** nominal **`newtype`** — `newtype Name = <type>` (optionally with a method block)
  is a DISTINCT type wrapping the underlying (Go defined-type model), not a transparent alias: only an
  explicit construct (`Name(x)`) or cast-unwrap (`int(n)`/`float(n)`/`str(n)`) crosses the boundary,
  so accidental mixing with the raw underlying (or a different newtype) is a compile error. Numeric
  (`int`/`float`) underlyings auto-flow same-type operators (the underlying's *native* op,
  unwrap→op→rewrap); a `str`/`bool` newtype does **not** auto-inherit `+`/`<` in v1 (define a method);
  equality (`==`/`!=`) works for any underlying. A **numeric** (`int`/`float`) newtype additionally
  satisfies `Comparable` by its underlying's *native* order (not a user `compare` method — the same-type
  `<`/`>` arm auto-flows to the underlying), so `<`/`>` AND `List[newtype].sort()`/`.min()`/`.max()`
  order by the wrapped scalar; a `str`/`bool` newtype is not `Comparable` in v1. Methods
  + `Stringable`/`Hashable`/`Comparable` work via the newtype's own methods (`str`/`hash`/`compare`
  dispatched at runtime in both engines); the **operator** protocols (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`)
  are NOT satisfiable by a newtype method (a newtype's own `add`/`div`/… is never dispatched as an
  operator — the same-type arm auto-flows to the underlying's native op), so they come only from a
  numeric underlying's intrinsic auto-flow. **Generic newtypes** (`newtype Stack[T] = List[T]`) are methods-only
  (no native operator auto-flow even for `Box[T] = T`): ctor infers type args (from the binding/
  return/parameter **annotation** — `e: Stack[str] = Stack([])` — or a turbofish
  `Stack[int]([])` when an empty literal can't bind `T`), cast-unwrap propagates the instantiation
  (`List(s)` for `s: Stack[int]` ⇒ `List[int]`). v1 limits: aggregate underlyings get
  identity+construct+unwrap+own-methods only (no `.push`/index/iterate forwarding); no `derive`;
  static / associated methods on a **newtype** are a follow-up (they **have** landed for struct +
  enum — see the "Static methods" milestone note below). Declaring one (`fn zero()` — no `self`)
  is now **rejected with a clear "not supported yet" error** at the decl site (and at any
  `Newtype.method()` call site), not the old cryptic "unknown name".

> **Static (associated) methods — the "no self ⇒ static" rule (landed; struct + enum).** A
> struct/enum method whose first parameter is **not** `self` (or which has no parameters) is a
> **static** method, called `Type.method(args)` instead of `value.method(args)` (the Rust `fn new`
> ergonomic). Additive — the positional `Name(...)` ctor is unchanged; static methods enable named /
> alternative ctors (`Rect.square(5)`) and validating ctors returning `Result` / `Option`
> (`Email.parse(s) -> Result[Email, str]`, `Color.from_str(s) -> Option[Color]`). An instance method
> and a static method are different call shapes — neither is invocable as the other. For enums a
> **variant** wins over a static-method name on `Enum.x` (variant/static names must be disjoint —
> enforced at declaration time). Generic statics use the **type-level** turbofish `Box[int].empty()`
> (the type arg sits on the type); a static method may **also** declare its own `[U]` (PART 2), pinned
> on the member or inferred (`Box[int].make[str](x)` / `Box.make(5)`). The enclosing type param must be
> **pinned at the construction site** — by a type-level turbofish (`Box[int].empty()`), an argument
> (`Box.of(5)` ⇒ `T=int`), or a binding/return annotation (`b: Box[int] = Box.empty()`). An
> un-turbofished, un-annotated factory whose return leaves `T` free (`b := Box.empty()`) is **rejected**
> at the first mismatching use with the same "un-inferred type parameter … bind it at the construction
> site" guidance as a bare container ctor (`[]`) or a generic free-function return — it is **not**
> silently degraded to `Unknown` (which used to swallow any later argument and defeat homogeneity). A
> method-**own** `[U]` with nothing to bind it stays refinably `Unknown` (genuinely unconstrained). v1 limits: static methods do
> **not** participate in protocol conformance (protocols stay instance-only); static methods on
> `newtype` and **associated protocol requirements** (`T.zero()`) remain follow-ups — the latter
> **attempted twice and SHELVED** (see `docs/future.md` §3.13; factory-closure is the working alternative).

> **Turbofish at the declaration site — type-side (PART 1, landed).** Explicit type args for a generic
> are pinned **at the site the generic is DECLARED**: declared on the type (`enum/struct/newtype [T]`)
> → pinned on the type (`Box[int]`); declared on a member (`fn m[U]`) → pinned on the member. For a
> generic TYPE the args go on the TYPE, uniformly for enum **variant constructors** and **static
> methods**: `Box[int].Has(5)`, `Result[int, str].Ok(5)`, nullary `Box[int].Empty`, generic static
> `Box[int].empty()`. Multi-param types use the comma form (`Result[int, str].Ok`). The old **gliding**
> form `Enum.Variant[T](args)` (type args on the variant) is **removed** — the checker redirects to the
> type-side form. Inference is unchanged: `Box.Has(5)` (no turbofish) still infers `Box[int]`; the
> turbofish is needed only when args can't bind the params (`Box[int].Empty`, multi-param enums). The
> change is in the checker's resolution + the value's inferred type args only — runtime is type-erased,
> so both engines stay byte-identical.

> **Turbofish at the declaration site — member-side (PART 2, landed).** Completes the rule: a **member**
> declares its OWN type args (`fn make[U]`, `fn first[A, B](self, …)`), pinned on the member and
> composing with the type-side args from PART 1. `Box[int].make[str](x)` supplies the enclosing `T`
> *and* the method `U`; `Box.make[str]("hi")` / `s.first[int, str](1, "x")` are the bare carriers.
> Inference is the default — the turbofish is needed only when a param can't be bound by an arg
> (`Box[int].make(5)` infers `U = int`); an un-inferred member/enclosing param degrades to `Unknown`,
> never a leaked `Ty::Param`. A method param may not shadow an enclosing type param; a member-level
> turbofish on a non-generic member or a builtin (`xs.iter[int]()`, `xs.len[int]()`) is an arity error.
> **UNIFORM PARSE RULE:** `recv.name[X](args)` parses as a method turbofish on **ANY** receiver — a
> bare ident, a call result (`W(1).cast[str]("a")`), a field (`h.w.cast[str](x)`), or an index
> (`xs[0].cast[U](x)`). The parser steal is **widened to any member-access receiver**; the combined
> form `Box[int].make[str](x)` rides the same path (the receiver `Box[int]` is itself a postfix) and
> the checker threads both the enclosing type args and the method args. The combined form supports a
> multi-arg method turbofish (`mk().pair[int, str](..)`) and nested-generic type args
> (`W(1).cast[Map[str, int]](m)`). **Authorized trade-off:** index-then-call of a fn-**valued** field
> now needs parens on any receiver — `(recv.name[k])(args)` (the bare-ident receiver already required
> this); the numeric form `arr[0].handlers[0](20)` still parses as index-then-call (an int is not a
> type). A method turbofish on a generic **variant** ctor (`Box[int].Has[str](5)`) is an error.
> Runtime is type-erased (dispatch to the existing `CallStatic` / method paths), so both engines
> (serial `--serial` VM and the default M:N VM) are byte-identical (`examples/turbofish_member_args.chz`). Still out of scope: static
> methods on `newtype` and associated protocol requirements (`T.zero()`) — the latter **SHELVED**
> after two rejected attempts (see `docs/future.md` §3.13).

> **Expected-type inference — an annotation pins a generic ctor / generic fn-call (landed).** Beyond
> the turbofish above, a type **annotation** that surrounds a generic constructor or generic function
> call now flows INTO its type-parameter inference: a `let`-binding's declared type, a function's
> declared **return** type, and a call **argument**'s declared parameter type each pre-seed the
> generic's params, which in turn pin any closure params that depend on them. So
> `h: Heap[int] = Heap([], fn(x, y): x < y)`, `fn mk() -> Heap[int]: return Heap([], …)`, and
> `take(Heap([], …))` (with `take(h: Heap[int])`) all type-check — previously each needed an explicit
> turbofish or annotated comparator params. The annotation fills **only** the params the arguments
> left free (precedence: **turbofish > arguments > annotation**), so a concrete argument still wins and
> a conflicting annotation is the usual assignability error. It also reaches generic **newtype** ctors
> (`e: Stack[str] = Stack([])`) and a return-only param of a generic fn (`xs: List[int] = empty()` for
> `fn empty[T]() -> List[T]`). Checker-only (a new expected-type hint threaded into the ctor/call
> inference, consumed by `unify` before the un-inferable-closure-param probe); runtime is type-erased,
> so both engines stay byte-identical. **Remaining gap:** the hint does not yet reach a
> generic ctor nested inside a **container literal** (`a: List[Heap[int]] = [Heap([], …)]`) — that
> outer expression is a list literal, not a call, so annotate the closure params or turbofish there.
> The same expected-type / turbofish machinery now also pins a generic fn used as a **VALUE** (not
> called, or called indirectly): `g := ident[int]` (turbofish) and `h: fn(int) -> int = ident` /
> HOF-param / return-position (against a concrete `fn(...) -> ...`) yield the substituted concrete fn
> value; a bare un-pinned generic fn value stays an error (see `docs/syntax.md`, "A GENERIC fn as a value").

> **Native FFI — Level-2 SHIPPED in M6c; Level-3 dynamic C-ABI v1 SHIPPED.** Because Chezzi is
> written in Rust, the native-stdlib mechanism doubles as a foreign-function interface: bind a Rust fn
> and expose it as a module member, instead of reimplementing everything in Chezzi.
> - ✅ **`NativeFn`** — a Rust fn registered as a callable Chezzi value (member of a native module),
>   carried by `vm::Obj` (`Native`); parity-tested.
> - ✅ **`Host` trait** (`src/native/mod.rs`) — the engine-agnostic context a native fn uses
>   (`arg_int`/`arg_float`/`arg_str`, stdout/stderr/stdin, args/env/cwd) so a binding is written
>   once and runs on the VM (heap handles). Returns flow back as an
>   engine-neutral `NativeRet`, lowered to each engine's value *after* the call (GC-safe).
> - **Dependency policy:** the **core** (lexer/parser/checker/compiler/VM/GC) is Rust `std` only.
>   The **runtime** links a small fixed set of crates *unconditionally* (no Cargo features): `regex`
>   (`std.regex`), `ureq`+TLS (`std.request`), `libc`/`polling`/`socket2` (the `--parallel` netpoller
>   + `std.net`), and `libffi`/`libloading` (the Level-3 C-ABI FFI). See `Cargo.toml`.
> - ✅ **Level-3 dynamic C-ABI (v1)** — an `extern "lib":` indentation block of statically-typed C
>   signatures, bound at module init by `dlopen`+`dlsym` and called at runtime via `libffi`, reusing
>   the SAME `Host`/`NativeRet` seam (so both VM engines produce identical output).
>   `extern` fns become ordinary module globals (`vm::Obj::Cffi(Arc<Cffi>)`),
>   so the normal call-dispatch and `infer_named_call` type-check paths work with zero special-casing.
>   The checker enforces **C-marshallability** (int/float/bool/str/ptr params + returns, void return,
>   the fixed-width ints, and a flat-scalar struct by value) on the resolved type, so a non-marshallable
>   param is a compile error. The `Cffi` keeps its `Library`
>   alive + stores the resolved symbol as a `usize` (libloading `Symbol` is `!Send`) and rebuilds the
>   `Cif` per call (libffi `Cif` is `!Send`), so it is `Send + Sync` for `--parallel`; the M:N
>   snapshot path shares the `Arc<Cffi>` (same address space — no re-`dlopen`). See `src/native/cffi.rs`
>   + `examples/ffi.chz`. **Structs by value shipped (flat scalar fields):** name a Chezzi `struct` of
>   scalar fields (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`) as an extern param and/or return type to
>   pass/return a C struct **by value** — `CType::Struct{name, field_names, fields}` carries only owned
>   data (no libffi `Type`, which is `!Send`/`!Sync`/`!Clone`), and the libffi structure type + per-field
>   offsets are rebuilt per call via `ffi_get_struct_offsets` (so the platform ABI — small-struct-in-
>   registers vs by-hidden-pointer — is libffi's, never hand-rolled), keeping `Cffi` `Send + Sync`. A
>   struct return uses the raw `ffi_call` with an own rvalue buffer sized `max(struct_size,
>   sizeof(ffi_arg))` (the register-width floor the narrow-int-return fix established), reading each field
>   at its libffi offset into a `NativeRet::Struct` both engines already lower. See `examples/ffi_struct.chz`.
>   **v1 limits:** nested structs, `str`/`owned_str` struct fields, and generic structs are rejected (a
>   struct with a non-scalar field errors naming the struct + field); **sync scalar callbacks shipped**
>   (a `fn(scalars) -> scalar` extern param → a libffi closure trampoline C calls back synchronously,
>   scalars only, fault caught + re-raised; **pointer-deref builtins now shipped** — see below —
>   stored/cross-thread callbacks deferred), varargs, the rich Rust `Box<dyn Any>` userdata handle, and a custom user-named
>   deallocator are deferred. **Fixed-width integers shipped:** beyond bare `int` (↔ C `long`), the marshalling type
>   names `int8`/`int16`/`int32`/`int64`/`uint8`/`uint16`/`uint32`/`uint64` bind C `int32_t`/`uint32_t`/…
>   (bidirectional, truncate-on-param / sign-or-zero-extend-on-return; `examples/ffi_int.chz`). They are
>   **imported per-name from `std.ffi`** (Chezzi's first type imports), not global builtins.
>   **`char*` ownership + nullable returns shipped:** a plain `str` return is borrowed (copied,
>   never freed); declare it **`owned_str`** (a return-only marshalling type) to copy **and** free a
>   `malloc`'d buffer with libc `free` (no leak), or **`str?`** (`Option[str]`) to make a `NULL` return
>   `None` instead of a fault (`owned_str?` composes both). See `examples/ffi_str.chz`. **Opaque `void*`
>   handles shipped:** declare `ptr` (an opaque type imported from `std.ffi`, ↔ C `void*`) to hold a C handle
>   (`FILE*`/`sqlite3*`/…) across calls — `Obj::Ptr(usize)` / `Value::Ptr(usize)`, a GC leaf, sendable
>   by value (`WireValue::Ptr`), value-compared by address, `<ptr null>`/`<ptr>` stringify (never the
>   raw address — non-deterministic), never auto-freed (manual destroy). The `ptr` type AND the value
>   vocab (`null()`/`is_null`) all live in `std.ffi` — using `ptr` (including in an `extern` block)
>   requires `import std.ffi` (or `import ptr from std.ffi`), exactly like the fixed-width integer
>   types; see `examples/ffi_ptr.chz`. **The memory behind a `ptr` is now
>   readable/writable** via `std.ffi` `load_*`/`store_*` (every C scalar width + `load_str`, each with
>   an `_at(p, off)` byte-offset form) — so struct fields, return buffers, and C output-params a library
>   hands you can be read/written. **You can also make your OWN C-laid-out buffer** via `std.ffi`
>   `alloc(nbytes)` / `alloc_zeroed(nbytes)` (libc `malloc`/`calloc`, returning a raw `ptr`) and release
>   it with `free(p)` — **manually freed** (`defer ffi.free(p)`; never auto-freed; `free(null())` is a
>   no-op; negative-size + OOM are recoverable errors). Combined with the deref builtins + a callback
>   comparator, `qsort`/`bsearch` of a Chezzi list now fully works (see `examples/ffi_qsort.chz`).
>   **Unsafe, like ctypes:** a bad pointer segfaults; double-free / use-after-free / out-of-bounds
>   store_/load_ are UB (no bounds/lifetime tracking); only the NULL base
>   pointer is guarded (recoverable error, no fault) — a `ptr` cannot be forged from an int (provenance
>   is C-sourced). See `stdlib.md §std.ffi`. A slow C call runs inline (extern
>   names are NOT in `is_blocking`, so it pins its worker under `--parallel`).
> - **FFI v1 limits (known + by design):**
>   - **Integer width (FFI-2 — RESOLVED, opt-in):** bare Chezzi `int` (i64) still marshals as C
>     **`long`** (64-bit on every supported **LP64** unix target — unchanged for back-compat; the prior
>     limit was *"scalars only: int ↔ long, no fixed-width int type"*). To bind a C function taking or
>     returning a fixed-width integer (`int32_t`, `uint32_t`, …), declare the parameter/return with one
>     of the **fixed-width marshalling type names** — `int8`, `int16`, `int32`, `int64`, `uint8`,
>     `uint16`, `uint32`, `uint64`. Like `ptr` (and unlike `owned_str`, which is neither global nor
>     importable — it is licensed **only inside an `extern` signature**; a bare non-extern annotation is
>     rejected *'owned_str' is a return-only extern marshalling type and cannot be used as a general type
>     annotation*), these are
>     **not global**: each is a **type imported per-name from `std.ffi`** — Chezzi's first type imports — with the same
>     `import int32, uint32 from std.ffi` form as the `null`/`is_null` value members (`std.ffi` exports
>     both callable members and these eight TYPE names; the declaring list is `native::ffi::TYPE_NAMES`,
>     no grammar change). A module that names a width type without importing it gets *unknown type
>     'int32' (import it from std.ffi …)*; a bogus name (`import int99 from std.ffi`) errors like any bad
>     import. The import is **per-module**: a struct's int32 field resolved in module A is usable from
>     module B with no import in B, but a bare `int32` written in B's own source needs B's own import. To
>     the program each is a plain `int` (`Ty::Int`); the
>     width/signedness is a **runtime-only** marshalling distinction the backends recover via `ctype_of`
>     (the platform-exact libffi `sint8`/`uint8`/…/`sint64`/`uint64` types). Unlike `owned_str`
>     (return-only), these are **bidirectional** — valid as both param and return. **Boundary semantics
>     (C-cast, no overflow trap):** a **param truncates** the Chezzi i64 to the C width (wrapping —
>     `255` → `int8` is `-1`, `300` → `int8` is `44`); a **return sign-extends** (signed) or
>     **zero-extends** (unsigned) the C value back to i64 (`int32` `-1` → `-1`; `uint32` `0xFFFFFFFF` →
>     `4294967295`). `uint64` above `i64::MAX` is not representable in Chezzi's i64 `int` and wraps
>     negative (a documented v1 limit; the other seven widths fit i64 losslessly). **Aliases:** a
>     `type Len = int32` used in an `extern` sig behaves identically to bare `int32` (`ctype_of`
>     resolves the alias one hop to the width) — but the alias resolves only if its target `int32` is
>     imported in the same module that declares the alias. **No C-spelling aliases** (`c_int`/`c_short`/…) yet —
>     their width is platform-dependent (LP64 vs LLP64); deferred to a future task. See
>     `src/native/cffi.rs` (`CType::Int8`..`CType::UInt64`) + `examples/ffi_int.chz`. **Non-unix is
>     unsupported:** on an LLP64 target (Windows x64) C `long` is 32-bit and would truncate bare `int`,
>     so the checker **rejects `extern` on non-unix targets** and the `cffi` module is
>     `#[cfg(unix)]`-gated.
>   - **`char*` ownership (FFI-3 — RESOLVED, opt-in):** a plain `str`-typed return is **borrowed** —
>     copied into a Chezzi string and **never `free`d**, so a freshly `malloc`'d `char*` leaks (use this
>     for static/interned returns). To take ownership, declare the return **`owned_str`** (a return-only
>     marshalling type name, sibling of `ptr` — the program still sees a plain `str`): Chezzi copies the
>     buffer **then frees it** with libc `free`, resolved once via `dlsym("free")` on the loaded library
>     at `Cffi::new`. **Limits:** only libc `free` is supported (a custom user-named deallocator is
>     deferred); if `free` can't be resolved the return degrades to the old leak rather than aborting;
>     and `owned_str` is a **user assertion** that the buffer is genuinely `malloc`'d — declaring a
>     static/string-literal return `owned_str` corrupts the heap (same C-trust-boundary stance as a
>     non-NUL-terminated over-read). `owned_str` is **return-only** (rejected as a parameter) and is
>     legal **only inside an `extern` signature** — a bare non-extern annotation (`fn f(x: owned_str)`)
>     is rejected at the checker rather than silently collapsing to `str`.
>     See `src/native/cffi.rs` (`CType::OwnedStr`) + `examples/ffi_str.chz`.
>   - **Nullable `str?` returns (RESOLVED, opt-in):** a plain `str` return that comes back `NULL` is a
>     recoverable **fault** (it would break the static non-null `str` guarantee). To opt into a legitimate
>     `NULL` (e.g. `getenv` of an unset var), declare the return **`str?`** (`Option[str]`): `NULL` →
>     `None`, non-null → `Some(str)`. Composes with ownership: `owned_str?` is nullable **and** freed.
>     `str?` is **return-only** (a `str?` parameter is *not C-marshallable*). See `CType::OptStr`.
>   - **No `--parallel` serialization / non-reentrant C (FFI-7):** `extern` calls are **NOT** serialized
>     under `--parallel` — two OS-thread workers can be inside C code at the same time. Calling a
>     **non-reentrant** C function (e.g. `strtok`, `gmtime`/`localtime`, `setlocale`, anything using
>     `errno` carelessly or static internal buffers) concurrently **races at the C level** (Chezzi
>     cannot guard state it does not own). Use only thread-safe/reentrant C entry points under
>     `--parallel`, or confine such calls to the sequential engines.
>   - **Untyped + un-freed handles (FFI-`ptr`):** a `ptr` is **one opaque type** for every C handle —
>     Chezzi never distinguishes a `FILE*` from a `sqlite3*` (ctypes-level; passing the wrong handle is
>     C-level UB, the author's cross-boundary assertion) — and is **never auto-freed**: the program
>     calls the library's own destroy (`fclose(f)`); forgetting **leaks** (same stance as a borrowed
>     `str` return). NULL is allowed (a `ptr` return of NULL is `<ptr null>`, not a fault — unlike a
>     non-nullable `str`/`owned_str` return; use `str?` for a nullable `char*`). The `ptr` is opaque
>     *as a value* (no `FILE*` vs `sqlite3*` distinction, cannot be forged from an int), but its memory
>     is **no longer opaque**: `std.ffi` `load_*`/`store_*` read/write the bytes behind it (unsafe — a
>     bad pointer segfaults; only NULL is guarded). See `stdlib.md §std.ffi`.
>   - **Flat-scalar structs only (FFI-`struct`):** a struct passed/returned by value must have **only
>     scalar fields** (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`) in v1. A **nested struct** field or a
>     **`str`/`owned_str`** field is rejected at the checker with an error naming the struct + offending
>     field; a **generic struct** (`Pair[int]`) has no fixed C layout and is rejected. A transparent
>     `type P = Point` alias to a flat struct is accepted identically to the bare struct. The struct's
>     field order + types define the C layout (libffi computes size/alignment/offsets); valid as both a
>     param and a return. The struct may be declared **before or after** the `extern` block (forward
>     reference is fine). **`bool` field:** marshals as a C `_Bool` (1 byte) — it matches a C struct
>     field declared `_Bool`/`char`, **not** a 4-byte `int`; for an *int*-width boolean field declared
>     `int` in C use `int8`/`uint8`/`int32` and test `!= 0`. (Field types are part of the C-layout
>     contract the binding author must match, like `int32` vs `int64`.) See `CType::Struct` +
>     `examples/ffi_struct.chz`.
> - **Still deferred (Level-3):** the rich **Rust `Box<dyn Any>` userdata handle** (for compiled-in
>   Rust libraries like Burn — distinct from the C `void*` `ptr` above, which shipped), a **custom
>   user-named deallocator** (only libc `free` backs `owned_str`), and the deferred FFI features above
>   (nested structs-by-value /
>   `str` struct fields / **the rest of callbacks #4** — stored/cross-thread callbacks; **sync scalar
>   callbacks, pointer-deref `load_*`/`store_*` builtins, AND the C-buffer alloc layer
>   (`ffi.alloc`/`alloc_zeroed`/`free`) have shipped** (so `qsort`/`bsearch` of a Chezzi list works; a
>   GC-tracked auto-freed owned-buffer type + bulk-copy helpers + `realloc` remain deferred) /
>   **varargs #5**, with design
>   notes + the callback feasibility ladder + workaround in `docs/ffi-and-packaging.md §1b`). See
>   `docs/ffi-and-packaging.md`. (`std.os.exit` with a real exit-code channel through the run drivers
>   has since **shipped** — see `examples/exit.chz`.)

## Type conversions & casting

Chezzi has **no `as` cast operator**, no `Into`/`TryFrom`, and no general-purpose value-level
conversion protocol (yet — see `docs/future.md`); a **bound-only** `Convert[S]` conversion protocol
exists as a generic bound (below). Conversion is done by a small fixed set of explicit builtins plus one implicit
numeric widening. The design is deliberately minimal: prefer an explicit constructor call over silent
coercion, and keep newtypes nominally distinct so a conversion is always visible in the source.

**Scalar conversion constructors** (global builtins — see `docs/stdlib.md §1`):

| Form | From → To | Failure |
|------|-----------|---------|
| `int(x)` | `int`/`float`/`bool`/`str` → `int` | truncates a float; **faults** (recoverable) on a non-finite float or an unparseable string |
| `float(x)` | `float`/`int`/`bool`/`str` → `float` | **faults** on an unparseable string |
| `bool(x)` | `int`/`float`/`bool`/`str` → `bool` | never fails — a truthiness cast (int `0`/float `0.0`/`-0.0`/empty `str` → `false`; `NaN` → `true`; non-empty `str` is `true`, **not** a parse) |
| `str(x)` | **anything** → `str` | never fails — the `Stringable` display cast (`print`/interpolation use the same path) |
| `ord(s)` / `chr(n)` | `str` ↔ codepoint `int` | narrow, single-purpose |

**Safe (non-faulting) string parse** — return `Option` instead of faulting: `s.to_int() -> int?`,
`s.to_float() -> float?` (`None` on bad input). Their error-message-carrying siblings return a
`Result` instead: `s.parse_int() -> Result[int, str]`, `s.parse_float() -> Result[float, str]`
(`Ok(n)` or `Err(msg)` with a human-readable parse-error message). Use these over `int()`/`float()`
when the input is untrusted.

**Implicit coercion — one-way `int` → `float` widening of an UNTYPED CONSTANT only** (Go's rule). An
untyped int *constant* expression (literal / unary `-` / `+ - * / %` over those) adapts to a `float`
slot and is converted to a real `f64` at every value-definition boundary (typed binding, `float`
param/default, `-> float` return, `float` struct field, mixed-numeric-constant collection). A **typed**
`int` value never implicitly converts — write `float(x)`, and a call through a function VALUE never
widens at all. It is **scalar-or-element-at-the-sink**: never propagated into a nested/type-argument slot
(`List[List[float]] = [[1]]`, `float? = Some(3)` stay errors), and the reverse (`float` → `int`) is always
a lossy type error. Emitted as `Op::CoerceFloat` so both engines are byte-identical. (Full rules in the
numeric-arithmetic section above.)

**Newtype boundary** (`newtype Name = <T>`) — nominally distinct, so crossing is always explicit:
wrap with `Name(x)`, unwrap with the matching scalar/aggregate cast (`int(n)`, `list(s)`, …; the
underlying must match exactly). A numeric-scalar newtype auto-flows the underlying's native operators;
everything else is methods-only. See the M21 note above.

**Stringable** — every scalar (`int`/`float`/`bool`/`str`) intrinsically satisfies the `Stringable`
protocol, so `[T: Stringable]` generics accept them (an erased `v.str()` body dispatches to the same
native `stringify` that `str(x)` uses). Structs/enums/newtypes opt in with their own `str(self) -> str`.

**Intrinsic conformance implies a callable method.** Wherever a built-in satisfies a protocol
*intrinsically* (no user method), the protocol's method is callable on it inside an erased generic body
or through a protocol-typed value — and it is defined as **exactly** the operator/primitive form:
`a.add(b)` ≡ `a + b` (same overflow / divide-by-zero faults, same int↔float coercion), `a.neg()` ≡ `-a`,
`a.compare(b)` is what `<` orders by, `c.index(k)` ≡ `c[k]`, `c.set_index(k, v)` ≡ `c[k] = v` (returns
`nil`), `c.slice(s, e, st)` ≡ `c[s:e:st]` (its three components are `int?`, i.e. `Option[int]`), and
`x.hash()` is exactly the hash `x` gets as a map/set key. `hash()`'s numeric value itself is
**unspecified** (a build-dependent 64-bit hash, possibly negative) — only its consistency is
guaranteed: equal values hash equally, and it agrees with container membership.

The intrinsic grants and their methods are: `Comparable`→`compare`, `Stringable`→`str`,
`Hashable`→`hash`, `Error`→`message`, `Iterable`→`iter`, `Index`→`index`, `IndexSet`→`index`+`set_index`,
`Slice`→`slice`, `Add`/`Sub`/`Mul`/`Div`/`Mod`→`add`/`sub`/`mul`/`div`/`mod`, `Neg`→`neg`. A type that
DEFINES the method always gets its own (intrinsic dispatch is a resolution fallback, never a shadow).

**Three documented exceptions to the equivalence** (each with a `docs/gaps.md` entry):

- `Iterator`'s `next` on a *raw* collection is granted but faults — a raw collection holds no cursor
  position (W6-3b); iterate it with `for`, or call `.iter()` for a real cursor.
- `a.compare(b)` with a **NaN** operand raises `cannot compare NaN (compare has no unordered result)`.
  `<`/`<=`/`>`/`>=` are total on floats (every NaN comparison is `false`), but `compare(self, other) ->
  int` has no encoding for "unordered", so the method faults rather than answer wrong — a recoverable
  value-domain fault like `division by zero`, same position Rust takes (`f64` is `PartialOrd`, not
  `Ord`). Order NaN-bearing data with the operators, or filter NaN first (W6-3c).
- A numeric `newtype` that **defines** `add`/`sub`/`mul`/`div`/`mod`/`compare` diverges: the method form
  dispatches ITS method (never shadowed — that rule wins) while `+`/`<` still auto-flow to the
  underlying's native op (see the newtype note in `docs/syntax.md`). Don't write both spellings over
  such a type (W6-3d).

**What does NOT exist (current boundaries):**

- No `as` operator (`as` in the grammar is only import aliasing).
- No `Into`/`TryFrom`, and no value-level conversion protocol — a `Convert[S]` **bound** exists (see
  below), but there is not yet an ergonomic value-position conversion mechanism (`T.convert` through a
  bound is a separate pending slice).
- `cast[T](val: Any) -> Option[T]` (a checked downcast off the `Any` top type) is **deferred** — it
  needs runtime type tags, since generics are erased (`docs/future.md`).

**`Convert[S]` — bound-only conversion protocol (partial).** A structural, target-keyed conversion
protocol `Convert[S]` exists as a reserved builtin. A type **witnesses** `Convert[S]` by declaring a
**static** method `fn convert(x: S) -> Self` (associated / no `self` receiver) — witnessed structurally
like `Comparable`/`Add`, but `is_static`-aware (an instance `convert(self, …)` does NOT witness it). It
is usable **only as a generic bound** `[T: Convert[S]]`; because a static ctor cannot be invoked on a
value, `Convert[S]` is **rejected as a value-annotation type** (param/field/return/binding, including
nested `List[Convert[int]]`/`Option[…]`/tuple, a same- or cross-module type alias, and a protocol that
*embeds* a static-ctor protocol) — bound-only by the same static-slot rule that applies to any
static-ctor protocol. NOT available: calling `T.convert(x)` **through** the bound — generics are erased
and a generic body is checked once with `T` abstract, so there is no concrete type to construct at
runtime (this affects *every* generic static call, e.g. `T.empty()`, not just `convert`). `T.<static>()`
on a type parameter is a clear error ("cannot call a static method through the generic type parameter
'T' … call the concrete type's static method directly or pass a `fn(...) -> T`"), and generic
construction over the bound is **deferred** pending witness-passing (`docs/future.md §15`). Use direct
`Type.convert(x)` (which needs no protocol) or a passed converter function. The cheap scalar fills
(`bool(x)` truthiness cast + `Result`-returning `parse_int`/`parse_float`) have **landed**. Recorded in
`docs/future.md §3`.

## Architecture — pipeline

```
source.chz
  → Lexer        (indent-aware: INDENT/DEDENT tokens)
  → Parser       (Pratt expr parsing + recursive-descent stmts) → AST
  → Desugar      (AST → AST lowering: pipe, optional chaining/`??`, comprehensions, defaults)
  → Checker      (local inference; explicit fn sigs; machine-readable errors) → typed AST
  → Bytecode compiler → Stack VM (+ mark-sweep GC)
```

(Historically a Phase-1 tree-walk interpreter ran first as the reference semantics; it has since been
removed — the bytecode VM is the sole engine.)

Each component is an isolated, separately-testable module. Golden tests assert the two VM engines
(the serial `--serial` VM and the default M:N VM) produce identical output.

### Repo layout

```
src/
  lexer/        # chars → tokens, indent stack
  parser/       # tokens → AST (Pratt)
  ast/          # node definitions
  desugar/      # AST → AST lowering (pipe, ?., ??, comprehensions, defaults)
  checker/      # type inference + checking
  compiler/     # AST → bytecode
  vm/           # stack machine (sole engine; serial + M:N schedulers)
  gc/           # mark-sweep
  runtime/      # builtins + native std modules
  resolver/     # module path resolution
  test_runner   # `chezzi test` — discovers + runs `test fn`s in `*_test.chz`
  main.rs       # `chezzi init/run/test/check/tokens/ast/docs`
std/            # std modules written in Chezzi
examples/*.chz  # golden-test corpus + LLM eval material
tests/          # Rust unit + golden tests
```

## Roadmap

| # | Deliverable | Runnable proof |
|---|-------------|----------------|
| ✅ **M1** | Indent-aware lexer | `chezzi tokens foo.chz` prints token stream incl. INDENT/DEDENT |
| ✅ **M2** | Parser → AST + pretty-printer | `chezzi ast foo.chz` round-trips source |
| ✅ **M3** | Tree-walk interpreter | Working language: arithmetic, fns, if/for/while, structs, enums, match, interpolation, Result+`?` run single-file |
| ✅ **M4** | Type checker (local inference) | Type errors caught pre-run with clear messages; `--errors=json` mode |
| ✅ **M4.5** | Modules / imports + resolver | Multi-file program runs; `chezzi.toml` root detection works |
| ✅ **M5** | Bytecode compiler + stack VM + mark-sweep GC | Runs on VM (default); ~4–6.5× over the tree-walker; golden + parity tests match |
| ✅ **M6** | Stdlib fill-out + pipe `\|>` operator + core-type methods | **Done**: str/list methods + pipe chains, plus M6c — the Level-2 native FFI seam (`NativeFn`+`Host`) shipping `std.math`/`io`/`os` (native) and `std.string` (Chezzi), running identically on both engines |
| ✅ **M7** | User-defined generics + structural protocols | Generic fns/structs, `Comparable` bound, `std.cmp` (`min`/`max`/`clamp`); golden tests on both engines |
| ✅ **M8** | Tier-1 stdlib | `std.json` (+ type-directed `decode[T]`), `std.process`, `std.fs`, `std.time`; the `set` type, iterable strings (`s.chars()`) |
| ✅ **M9** | Tier-2 stdlib | `std.regex` + `std.request` (first runtime crate deps; blocking); `Match`/`Response` structs |
| ✅ **M10** | Type-system depth | `Stringable`/`Hashable` + operator protocols (`Add`/`Sub`/`Mul`), generic enums, type aliases, multi-bound generics, any-`Hashable` map/set keys |
| ✅ **M11** | Panic recovery + Go-style errors | Phase A ✅ `Result[T, E]` + `Error` protocol; Phase B ✅ `recover:` boundary with try-block semantics. Both engines parity-tested |
| ✅ **M12** | Tier-3 ergonomics (part) | **Iterator protocol** (user structs with `next(self) -> Option[T]` iterable in `for`, lazy); **match guards** (`pattern if cond:`) + int **range patterns** (`1..10:`). Both engines parity-tested |
| ✅ **M13** | `Iterator[T]` protocol | The language's first **parameterized** protocol bound: `[S: Iterator[T], T]` accepts any iterable (built-ins intrinsically, structs via `next`) and recovers element type `T`. Lazy adapters (Take/Mapped) were the original answer to `yield` (then a non-goal; `yield`/generators have since shipped VM-only — see above). Checker/parser/grammar only; both engines parity-tested |
| ✅ **M14** | Generics depth | **Method-level type params** (a method's own `[U]`, inferred at call) + **user-defined parameterized protocols** (`protocol Container[T]`, structural conformance with concrete-arg bounds `[X: Container[int]]`, and first-class **value/annotation types** `c: Container[int]` — statically witnessed at every store/pass boundary, runtime-erased, strictly invariant `Container[int]` ≠ `Container[str]` ≠ bare `Container`, method-return element recovery) — the special-cased `Iterator[T]` generalized. Checker/parser/grammar only; both engines parity-tested |
| ✅ **M15** | Slicing + indexing protocols | Python-style `xs[a:b:c]` / `s[0:2]` / `xs[::-1]` (open bounds, step, reverse, bounds-clamped) + negative indexing `xs[-1]` (plain index faults out of range, slice bounds clamp — Python's asymmetry); the `..` operator stays the for-loop/match range (and a slice receiver — a range is **not a value** in any other position, and the checker now enforces that; use `range(a, b)` to materialize a `List[int]`). Prebuilt **`Index[K, V]` + `IndexSet[K, V]` + `Slice[R]`** structural protocols — built-in `list`/`map`/`str` conform intrinsically, user structs via `index`/`set_index`/`slice(self, start: int?=None, end: int?=None, step: int?=None)`, so `custom[k]`/`custom[k]=v`/`custom[a:b:c]` work and a generic can be bounded by `Index[int, V]`. Both engines parity-tested |
| ✅ **M16–M18** | Concurrency + `defer` | `spawn` / `parallel:` nursery, `Channel`/`Shared`/`Executor`, real OS-thread M:N engine (`--parallel`) with work-stealing + reduction-counting preemption + netpoller + `std.net`; `defer` (call + block forms). Design in [`docs/concurrency.md`](concurrency.md), phases in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md) |
| 🟦 **M19** | Perf track (in progress) | Landed: peephole + const-fold, superinstructions, global-slotting, struct-field inline cache, FxHash, `ConstStr` interning, call-loop flatten, small-string optimization. Behavior-preserving + two-engine parity on every change. Backlog ranked in [`docs/future.md §4`](future.md); measured deltas in [`docs/benchmarks.md`](benchmarks.md) |
| ✅ **M20** | In-language tests | `assert <cond>[, "<msg>"]` (both-engine statement primitive, faults with its source line), the `test fn` marker (free tests + struct **suites** with `before_all`/`after_all`/`before_each`/`after_each` hooks + a shared typed fixture), and `chezzi test [path]` — a Rust-side VM-only runner over `*_test.chz` files (`PASS/FAIL name (file:line) msg`, non-zero exit on failure). Surface in [`docs/syntax.md §9c`](syntax.md) |
| ✅ **M21** | Nominal `newtype` | `newtype Name = <type>` — a DISTINCT type wrapping the underlying (Go defined-type model), not a transparent alias: construct (`Name(x)`) / cast-unwrap (`int(n)`) cross the boundary; accidental mixing with the raw underlying or a different newtype is a compile error. Numeric (`int`/`float`) same-type operators auto-flow (native op, unwrap→op→rewrap); a `str`/`bool` newtype does not auto-inherit `+`/`<` (define a method); methods + `Stringable`/`Hashable`/`Add`/`Comparable` via the newtype's own methods (runtime hash/str dispatch, both engines). **Generic newtypes** (`newtype Stack[T] = List[T]`, Go defined-type model + generics): methods-only (no native operator auto-flow even for `Box[T] = T`); ctor infers type args (from the binding/return/parameter annotation — `e: Stack[str] = Stack([])` — or a turbofish `Stack[int]([])` when an empty literal can't bind `T`); cast-unwrap propagates the instantiation (`List(s)` for `s: Stack[int]` ⇒ `List[int]`). v1 limits: aggregate underlyings get identity+construct+unwrap+own-methods only; no `derive`; no static / associated methods **on a newtype** (`Type.method()`) yet — a follow-up (static methods HAVE landed for struct + enum; see the "Static methods" note); declaring one is **rejected with a clear "not supported yet" error** (decl site + call site), not a cryptic "unknown name". Surface in [`docs/syntax.md §7b`](syntax.md) |
| ✅ **M22** | Operator protocols + protocol embedding | New per-operator protocols **`Div`/`Mod`/`Neg`** (methods `div`/`mod`/`neg`, powering `/`/`%`/unary `-`; `int`/`float` intrinsic, structs/enums via the method, numeric scalar newtypes via the underlying's native auto-flow (`Div`/`Mod` only — `Neg` is out of scope for newtypes, and a newtype operator *method* is never dispatched) wired exactly like `Add`/`Sub`/`Mul`. **Protocol embedding** — a protocol body may list embed lines (`Add + Sub`, order-free, interleaved with `fn` sigs); a type satisfies it iff it satisfies every embed (transitively) AND every own method, flattened at bound sites. Collision rules: own-fn-vs-embed = error, same-sig embed diamond dedups, differing-sig embed = error, cyclic embed = error. Builtin **`Arithmetic`** bundle = `Add + Sub + Mul + Div`. Checker/parser/grammar + both-engine operator dispatch; parity-tested. Surface in [`docs/syntax.md`](syntax.md) |
| **Stretch** | Cranelift AOT/JIT backend | Near-Go native speed (optional; a late-stage endeavor once the language has matured) |

> Native FFI (Level-2 compiled-in bindings) **shipped in M6c**; **Level-3 dynamic C-ABI FFI v1
> shipped** (`extern "lib":` scalar calls via dlopen+libffi, **plus opaque `void*` handles** via the
> `ptr` type + `std.ffi`, the return-only `str` opt-ins **`owned_str`** (copy + libc `free`) and
> **`str?`** (`NULL` → `None`), **plus flat-scalar structs by value**, **plus sync scalar callbacks**
> — a `fn(scalars) -> scalar` extern param C calls back synchronously, same-thread, scalars only,
> **plus pointer-deref `load_*`/`store_*` builtins** reading/writing the memory behind a `ptr`,
> **plus the C-buffer alloc layer** `ffi.alloc`/`alloc_zeroed`/`free` — libc-backed, manually-freed raw
> buffers, so `qsort`/`bsearch` of a Chezzi list now fully works) — see
> the *Standard library* note above. The remaining Level-3 surface (nested structs-by-value, `str`
> struct fields, stored/cross-thread callbacks, a GC-tracked auto-freed owned-buffer type + bulk-copy
> helpers + `realloc`, varargs,
> a custom user-named deallocator, and the rich Rust `Box<dyn Any>` userdata handle) is still a future idea.

## Verification

- **Per-phase Rust unit tests** — lexer token streams, parser AST shapes, checker accept/reject cases.
- **Golden tests** — `examples/*.chz` + `*.expected`; harness runs each through both VM engines (serial `--serial` and default M:N), asserts identical stdout.
- **Manual end-to-end** via the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval** — feed the grammar cheatsheet + `--errors=json` to a model, measure first-try compile rate; failures feed grammar/error-message work.
- **Perf check** — tracked against CPython via the `benches/` harness (`benches/run.chz`, hyperfine);
  baseline + per-bench bottleneck analysis in [`docs/benchmarks.md`](benchmarks.md). After the M19
  phases: ~1.3×–3.9× slower than CPython (worst on call/alloc-bound benches), startup ~11× faster.
