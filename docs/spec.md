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
2. **Usable tool** — bytecode VM for ~10x over a tree-walker; real modules so programs split into files.
3. **LLM-friendly** — static types as guardrails, explicit signatures, machine-readable compiler errors, small orthogonal grammar.

Closest existing cousins (read, don't copy): **Crystal**, **Nim**.

## Locked decisions

| Decision | Choice |
|----------|--------|
| Implementation host | **Rust** |
| Execution model | **Tree-walk first → bytecode stack VM** |
| Type system | **Static, local inference** (explicit param types; inferred locals *and* fn return types) |
| Surface syntax | **Indentation blocks** (Python-feel; lexer emits INDENT/DEDENT) |
| Errors | **Result/Option + `?`** (errors as values, no hidden control flow) |
| Code organization | **Composition, not inheritance** — structs + methods + interfaces (structural `protocol`s), like Rust/Go. No classes, no inheritance. |
| Memory | **Mark-sweep GC** (hand-built; primitives unboxed) |
| Name / ext / binary | **Chezzi** / `.chz` / `chezzi run foo.chz` |

## Language v1 — feature set

**Core:** `int float bool str`, `list[T]`, `map[K,V]`, `set[T]`, `tuple`, `fn`, `struct`, `enum`,
`if/else`, `for/while`, `Result[T, E]` & `Option[T]` + `?`, closures (`fn(x): x*2`), built-in generics
(`list`/`map`/`set`/`Result`). `Result[T, E]` is two-param: `T!` = `Result[T, Error]`, `T!E` =
`Result[T, E]`, `T?` = `Option[T]` (E defaults to the built-in `Error` protocol).

**Included:**
- **Pattern matching** — `match` on enums (also int/str/bool + tuple scrutinees), exhaustiveness-checked.
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
  (percent), `{255:x}` (hex). Type chars: `d f x X b o e %`. **Width and precision are capped at 4096**
  (a larger spec is a parse error — never a giant allocation). String `.N` truncates; an unknown type
  char or a type/value mismatch is reported before any output (runtime-prefixed; not caught by
  `check`). A bare interpolated ternary works; parenthesize to give it a spec (`{(if b: 1 else: 2):>5}`).
  The spec parser+formatter is shared by both engines (`src/fmtspec.rs`) → byte-identical output for
  well-formed programs. Full grammar in [`syntax.md` §10](syntax.md).
- **Literal forms** — int (`42`, `0xFF`/`0b1010`/`0o17`, `_` separators), float (`3.14`, scientific `6.022e23`/`1e3`/`1.5e-9` — any exponent ⇒ float), str in either `"…"` or `'…'` (interchangeable: same escapes & interpolation), also **triple-quoted** `"""…"""` / `'''…'''` (same escapes/interpolation, but unescaped quotes allowed inside) with escapes `\n \t \r \\ \" \' \0` and `\u{HEX}` unicode (1-6 hex digits), and **raw** `r"…"` / `r'…'` / triple `r"""…"""` (verbatim `str` — NO escapes, NO interpolation, braces literal; the escape hatch for the always-on `{…}`). See `docs/syntax.md §2/§10`.
- **Membership & assignment ops** — `x in xs` membership (`bool`; list/set element, map **key**, str substring); compound assignment `+= -= *= /= %= &= |= ^= <<= >>=` (= `x = x OP v`; bitwise forms int-only); and multi-target / tuple-swap assignment `a, b = b, a` (RHS evaluated first). See `docs/syntax.md §3/§4`.
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
- **M14** — method-level type params (`fn map_to[U](self, …)`) + **user-defined parameterized protocols** (`protocol Container[T]`, concrete-arg bounds `[X: Container[int]]`) — generalizing the special-cased `Iterator[T]`.
- **M15** — slicing + indexing protocols (Python-style `xs[a:b:c]` + negative indexing; `Index`/`IndexSet`/`Slice` structural protocols, built-ins intrinsic + user structs via `index`/`set_index`/`slice`).
- **M16–M18** — **concurrency** (`spawn` / `parallel:` nursery, `Channel[T]`, `Shared[T]`, `Executor`, real OS-thread M:N engine via `--parallel`, netpoller + `std.net`) and the **`defer`** statement (call + block forms, `recover:`-integrated). See [`docs/concurrency.md`](concurrency.md).
- **M20** — **in-language test framework**: `assert <cond>[, "<msg>"]` (a both-engine statement primitive that faults with its source line), the `test fn` marker (free tests + struct **suites** with `before_all`/`after_all`/`before_each`/`after_each` lifecycle hooks + a shared typed fixture), and `chezzi test [path]` — a Rust-side, VM-only runner over `*_test.chz` files reporting `PASS/FAIL name (file:line) msg` with a non-zero exit on failure. See [`docs/syntax.md §9c`](syntax.md).

**Non-goals (by design, never):** classes & inheritance — Chezzi is composition-only with
structural `protocol`s, like Rust/Go (see *Locked decisions*). (**`yield`/generators** were once
listed here as a non-goal; they have since shipped as a complete VM-only coroutine runtime — see
below.)
**Variadics** — neither variadic arguments (`fn log(*args)`) nor variadic generics (`Foo[T...]`):
pass an explicit `list`, and generics are always fixed-arity. Default + named arguments cover the
ergonomic cases variadics usually serve. **Spread/unpack syntax** (`[*a, *b]`, `{**m}`, `f(*args)`)
is likewise dropped — list concatenation and map merge are served by plain methods/operators, not
new syntax.

**Concurrency — SHIPPED (Tiers A–D).** No longer deferred: Chezzi has a shared-nothing actor model
(`spawn` cheap tasks + a `parallel:` structured-concurrency nursery), `Channel[T]` (move-on-send,
`close`/`for v in ch`/`try_send`), `Shared[T]`, and `Executor`. `chezzi run` defaults to the real
OS-thread engine (size its worker pool with `--threads=N` / `CHEZZI_THREADS`, `0` = all cores);
`--serial` selects the cooperative engine (kept as the byte-identical parity oracle). The OS-thread
engine is a **M:N work-stealing scheduler** (reduction-counting
preemption, a dirty/blocking pool for opaque blocking natives, and an epoll/kqueue netpoller backing
non-blocking `std.net` TCP). **M-C implicit nurseries shipped** — every function body and the module
top level is an implicit nursery that joins at its `return`/end, so a bare `spawn` is legal anywhere
(an explicit `parallel:` is an inner sub-nursery for earlier joins). Full design
in [`docs/concurrency.md`](concurrency.md); phase history in
[`docs/concurrency-tier-d.md`](concurrency-tier-d.md) + [`docs/concurrency-b3.md`](concurrency-b3.md).

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
feature: a `fn` declaring `-> Iterator[T]` may `yield` values; calling it returns a
suspendable generator usable anywhere an `Iterator[T]` is (`for` loops, `Iterator[T]` bounds). It is
VM-only — the frozen tree-walk interpreter rejects `yield` (it cannot suspend a native Rust call),
so two-engine parity is **waived** for generators. The adapter-struct model remains the
parity-clean, recommended way to write lazy sequences. Live status is tracked in
[`PROGRESS.md`](../PROGRESS.md).

**`Iterable[T]` protocol + `.iter()`** (additive over the `Iterator[T]` iteration model, all three
engines parity-clean). `Iterable[T]` promises `.iter() -> Iterator[T]` (a fresh COMPOSABLE cursor);
`Iterator[T]` additionally promises `.next()`, so every `Iterator` IS `Iterable` (its `iter()` returns
self). Every built-in collection (`list`/`set`/`map`→keys/`str`→char/`bytes`/`bytearray`→int) now
exposes `.iter()`, returning a cursor — a frozen snapshot of the collection plus a read position,
typed as the existing `Iterator[T]` existential (no new value type), with `.next() -> Option[T]` (Some,
then idempotent None). This lets a plain `list` flow into the same Take/Mapped adapter pipeline as a
hand-written struct iterator (`examples/iterable.chz`). A generator, a user `next`-struct, and a struct
with only `iter(self) -> Iterator[E]` (driven by a one-time `.iter()`) all satisfy `[S: Iterable[T]]`.
The cursor is **sendable** — it crosses the `spawn`/channel airlock as a deep copy (an independent
snapshot + position on the receiver), exactly like a `list`. A frame-holding **generator** (a value
returned by calling a generator `fn`) shares the same `Iterator[T]` existential but is **not**
sendable — its parked frames reference the producing heap. The checker cannot distinguish the two
(both are `Iterator[T]`), so the runtime is the enforcement point: a generator crossing **any** task
airlock — passed/captured into a `spawn`, stored in a `Channel`/`Shared`/`RwShared`/`Atomic`, or **merely being
a module global while a nursery runs** (the M:N engine snapshots all module globals) — raises a
**graceful, catchable** runtime error (`a generator cannot be sent across tasks`) with the real
spawn/nursery-site location, **never** a panic. There is **no** compile-time
multi-pass/single-pass safety (unfixable without move/ownership): each `.iter()` is a fresh cursor, but
reusing an exhausted one yields nothing.

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

fn main():
    r := safe_div(10, 2)?              # ? propagates Err
    nums := [1, 2, 3, 4]
        |> filter(fn(x): x % 2 == 0)   # pipe (built last)
        |> map(fn(x): x * 10)
    print(nums)

main()                                 # no auto-entry — `main` is a normal fn you call yourself
```

**Multi-line literals.** Inside `[]`, `{}`, and `()` the lexer suppresses layout (newlines /
indentation), so collection literals, call arguments, and parameter lists may span lines. A single
optional trailing comma is accepted before the closer (`[1, 2,]` ≡ `[1, 2]`); a lone comma is still
an error. `(x)` is grouping; `(x,)` is a one-element tuple. (See [`syntax.md` §2](syntax.md) and the
collection/`<params>`/`<argList>` productions in [`grammar.bnf`](grammar.bnf).)

**Entry model.** Programs run top-to-bottom; there is no automatic `main`. An `Err`/`None` left
unhandled at the top level (a bare expression statement, or a top-level `?`) exits the program with
`unhandled error: …` and a non-zero code. A bare `chezzi run` (no file argument) runs the project
manifest's `[project] entrypoint` — a **dotted module path**, optionally suffixed with
**`:function`** (e.g. `"src.main:main"`). The module runs top-to-bottom like any other file; with a
`:function` suffix the entry function is then **called** (a missing/non-function name is a clear
error), so the source needs no trailing call. Without the suffix the module just runs top-to-bottom
and calls its own `main()`. Running an explicit file (`chezzi run <file>`) is always top-level-only.

## Imports & module resolution

Grammar — dot paths, `import..from`, alias at both levels, **no** `from..import`:

```chezzi
import std.io                       # whole module → io.read()
import std.io as fs                 # module alias → fs.read()
import read, write from std.io      # named (no braces — indentation lang)
import read as r from std.io        # named + alias
```

Resolution — **optional root marker**, kills Python's run-relative footgun:

1. Take the `.chz` being run.
2. Walk *up* for `chezzi.toml`. Found → that dir is root. **Not found → script's own dir is root.**
3. `std.*` is reserved → always resolves to the stdlib dir.
4. `a.b.c` → `<root>/a/b/c.chz`. **No `./` relative imports.**

Single-file scripts need zero config (Deno/Bun/Go model); `chezzi.toml` only matters once a project spans multiple files.

**Types are module-scoped (Python-style).** A `struct` / `enum` / `type` alias is private to its
declaring module — every top-level type is exported by default (like functions; no `pub`), and is
reachable elsewhere **only via import**, accessed by the same bound last-segment name a function uses:

```chezzi
import core.geo                      # binds `geo`
p: geo.Point = geo.Point(1, 2)       # qualified construction + annotation
c := geo.Color.Red                   # qualified enum variant
xs: list[geo.Point] = []             # qualified type inside a generic

import Point from core.geo           # named import → bare use
q := Point(3, 4)                     # bare construction
import Point as Pt from core.geo     # rename on import (user types only)
```

A bare use of a type whose module was imported whole (`import geo`) but not named-imported is a
**check-time error** (`unknown type 'Point'; import it from geo`). Two modules MAY declare the same
type name with no collision — each is reachable from its own module.

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
wire). Reserved/native types (`Result`/`Option`/`Some`/`Ok`/…, `Ref`, `Iterator`, the std library type
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

- **Builtins (no import):** `print`, `len`, `range`, casts (`int()`/`str()`/`float()`),
  `ord`/`chr`, `set()`/`set(list)`, `panic(msg)` (raise a recoverable fault), core-type methods
  (`s.upper()`, `s.chars()`, `xs.push()`, `m.get()`, `set.add()`).
- **Std modules v1 (shipped, M6c):** `std.math`/`std.io`/`std.os` (native-Rust via the FFI seam),
  `std.str` + `std.cmp` (written in Chezzi; `std.cmp` adds M7-G3). Imported with
  `import std.math` / `import f from std.io`. `std.cmp` holds generic `min`/`max`/`clamp`
  (`[T: Comparable]`); `list.sort()` is likewise Comparable. (`std.math.min`/`max` were retired into
  `std.cmp`; `abs` stays native.) **Integer overflow policy:** the one integer type is `i64`; every
  overflow — arithmetic (`+ - * / %`), left shift (`<<`, when a significant bit is shifted out),
  negation, `MIN / -1`, and `math.abs(MIN)` — is a *recoverable
  panic* (`"integer overflow in <op>"`, catchable by `recover:`), never a silent wrap and never a host
  crash. **One-way `int`→`float` widening (C-like):** an `int` value flows into a `float` slot and is
  converted to a real `f64` (the reverse is a lossy type error). It fires at every value-definition
  boundary: a typed `let` (`x: float = 3` so `x / 2 == 1.5`, real float division), a `float`
  function/method/closure parameter (incl. an `int` *variable*, coerced at the callee prologue), a `float`
  parameter DEFAULT value (`fn g(a: float = 3)`), a
  `-> float` return, a `float` struct field, native/`extern` `double` params, and a **mixed-numeric-literal**
  collection (a list/map literal with ≥1 float literal infers `list[float]`/`map[_, float]`). The compiler
  emits a real conversion (`Op::CoerceFloat`; the interp applies an equivalent helper) so the checked path
  and the parity harness are byte-identical across both engines. Lossy conversions stay type errors
  (`y: int = 2.3`, `-> int: return 2.3`, `float` into `list[int]`, `int`→`float` across a **newtype**
  boundary). Widening is **scalar-at-the-sink**: a compound/nested float annotation is NOT widened —
  `list[list[float]] = [[1]]`, `float? = Some(3)`, `float! = Ok(3)`, an all-int literal `list[float] =
  [1, 2]`, and a non-literal RHS (`list[float] = f()`) all stay type errors (use explicit floats or a
  mixed literal). Carve-outs: a plain reassignment `x = 3` to a `float` local is rejected (type-blind
  target), and an un-annotated non-literal mixed collection is inferred `list[float]` but its non-literal
  `int` element is not widened at runtime (annotate to convert).
  No `byte`/`u8` scalar (Python model — binary data is the immutable `bytes` *sequence* type, **shipped**, not a
  scalar) and no bignum (a non-goal). **`bytes`** is a heap byte sequence (`b"..."` literal with
  `\xHH` escapes): `b[i]` -> `int` 0-255 (Index protocol), `b[a:b:c]` -> `bytes` (Slice protocol, byte
  offsets), `for x in b` yields `int`, `len(b)` is the byte count, `==`/`!=` are structural, and
  `bytes` is `Hashable` (valid map/set key). `str(b)` / `print(b)` / interpolation use the Python
  `b'...'` repr. Immutable (no `b[i] = x`). **`bytearray`** is the **mutable sibling** (Python
  `bytearray` model), **shipped**: constructor-only (`bytearray()` empty, `bytearray(N)` N zero bytes,
  `bytearray(b)`/`bytearray([ints])` from a bytes/list[int]) — no `ba"..."` literal. `ba[i]` -> `int`,
  `ba[i] = x` mutates in place (`IndexSet`; value 0–255), `ba[a:b:c]` -> a new `bytearray`,
  `for x in ba` yields `int`, `len`, `.push(int)` / `.pop() -> Option[int]` / `.extend(bytes|bytearray|
  list[int])`, `==` structural (incl. cross-type `bytes == bytearray` content-equal, Python parity).
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
  are byte offsets); `std.request` (blocking HTTP/HTTPS via `ureq`+rustls; `get(url)` /
  `post(url, body)` returning a `Response` struct `{status, body, headers: map[str,str]}`, where a
  ≥400 status is a normal `Response`, not an `Err`). These are Chezzi's **first runtime
  dependencies**. Both are **synchronous/blocking** (the language is single-threaded — see below).
  `Match`/`Response` are program-global reserved type names (a user struct of the same name
  collides). The native seam grew `NativeRet::Struct`/`Map` so a native fn can return a structured
  value.
- **Shipped since (M10):** generic enums; the `Stringable` protocol (custom `str(x)`); the `Hashable`
  protocol — any `Hashable` type is now a valid map/set key.
- **Shipped since (post-M18 stdlib batch):** `std.request` custom headers + non-GET/POST verbs
  (`put`/`patch`/`delete`/`head` + a general `request(method, url, body, headers)`), carried off-heap
  via a new `NativeArg::Map` so the headers form stays blocking-pool-offloadable under `--parallel`;
  `std.math` trig/exp/log intrinsics (`sin cos tan asin acos atan atan2 exp ln log2 log10 log`);
  pure-Chezzi `std.str` (`ends_with index_of count replace strip_prefix strip_suffix`) and `std.iter`
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
  equality (`==`/`!=`) works for any underlying. Methods
  + `Stringable`/`Hashable`/`Add`/`Comparable` work via the newtype's own methods (hash/str dispatched
  at runtime in both engines). **Generic newtypes** (`newtype Stack[T] = list[T]`) are methods-only
  (no native operator auto-flow even for `Box[T] = T`): ctor infers type args (turbofish
  `Stack[int]([])` when an empty literal can't bind `T`), cast-unwrap propagates the instantiation
  (`list(s)` for `s: Stack[int]` ⇒ `list[int]`). v1 limits: aggregate underlyings get
  identity+construct+unwrap+own-methods only (no `.push`/index/iterate forwarding); no `derive`;
  static / associated methods on a **newtype** are a follow-up (they **have** landed for struct +
  enum — see the "Static methods" milestone note below).

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
> on the member or inferred (`Box[int].make[str](x)` / `Box.make(5)`). v1 limits: static methods do
> **not** participate in protocol conformance (protocols stay instance-only); static methods on
> `newtype` and **associated protocol requirements** (`T.zero()`) remain follow-ups.

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
> so all three engines stay byte-identical.

> **Turbofish at the declaration site — member-side (PART 2, landed).** Completes the rule: a **member**
> declares its OWN type args (`fn make[U]`, `fn first[A, B](self, …)`), pinned on the member and
> composing with the type-side args from PART 1. `Box[int].make[str](x)` supplies the enclosing `T`
> *and* the method `U`; `Box.make[str]("hi")` / `s.first[int, str](1, "x")` are the bare carriers.
> Inference is the default — the turbofish is needed only when a param can't be bound by an arg
> (`Box[int].make(5)` infers `U = int`); an un-inferred member/enclosing param degrades to `Unknown`,
> never a leaked `Ty::Param`. A method param may not shadow an enclosing type param; a member-level
> turbofish on a non-generic member or a builtin (`xs.iter[int]()`, `xs.len[int]()`) is an arity error.
> The combined form parses as an index over the member access (indistinguishable from
> `value[i].field[k](x)`) and is resolved by **checker reinterpretation** (head a known type ⇒
> member-turbofish; head a value ⇒ ordinary index-then-call) — the parser steal is **not** widened. The
> combined form carries a single method type arg; multiple method args are reachable by inference.
> Runtime is type-erased (dispatch to the existing `CallStatic` / method paths), so VM, interp, and
> `--parallel` are byte-identical (`examples/turbofish_member_args.chz`). Still out of scope: static
> methods on `newtype` and associated protocol requirements (`T.zero()`).

> **Native FFI — Level-2 SHIPPED in M6c; Level-3 dynamic C-ABI v1 SHIPPED.** Because Chezzi is
> written in Rust, the native-stdlib mechanism doubles as a foreign-function interface: bind a Rust fn
> and expose it as a module member, instead of reimplementing everything in Chezzi.
> - ✅ **`NativeFn`** — a Rust fn registered as a callable Chezzi value (member of a native module),
>   added to both `interp::Value` (`Native`) and `vm::Obj` (`Native`); parity-tested.
> - ✅ **`Host` trait** (`src/native/mod.rs`) — the engine-agnostic context a native fn uses
>   (`arg_int`/`arg_float`/`arg_str`, stdout/stderr/stdin, args/env/cwd) so a binding is written
>   once and runs on both the interp (Rc values) and the VM (heap handles). Returns flow back as an
>   engine-neutral `NativeRet`, lowered to each engine's value *after* the call (GC-safe).
> - **Dependency policy:** the **core** (lexer/parser/checker/compiler/VM/GC) is Rust `std` only.
>   The **runtime** links a small fixed set of crates *unconditionally* (no Cargo features): `regex`
>   (`std.regex`), `ureq`+TLS (`std.request`), `libc`/`polling`/`socket2` (the `--parallel` netpoller
>   + `std.net`), and `libffi`/`libloading` (the Level-3 C-ABI FFI). See `Cargo.toml`.
> - ✅ **Level-3 dynamic C-ABI (v1)** — an `extern "lib":` indentation block of statically-typed C
>   signatures, bound at module init by `dlopen`+`dlsym` and called at runtime via `libffi`, reusing
>   the SAME `Host`/`NativeRet` seam (so VM + interp + `--parallel` produce identical output).
>   `extern` fns become ordinary module globals (`vm::Obj::Cffi(Arc<Cffi>)` / `interp::Value::Cffi`),
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
>   handles shipped:** declare `ptr` (a builtin opaque type, ↔ C `void*`) to hold a C handle
>   (`FILE*`/`sqlite3*`/…) across calls — `Obj::Ptr(usize)` / `Value::Ptr(usize)`, a GC leaf, sendable
>   by value (`WireValue::Ptr`), value-compared by address, `<ptr null>`/`<ptr>` stringify (never the
>   raw address — non-deterministic), never auto-freed (manual destroy). The `std.ffi` module adds the
>   value vocab (`null()`/`is_null`); see `examples/ffi_ptr.chz`. **The memory behind a `ptr` is now
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
>     `uint16`, `uint32`, `uint64`. Unlike `ptr`/`owned_str` (bare builtins), these are **not global**:
>     each is a **type imported per-name from `std.ffi`** — Chezzi's first type imports — with the same
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
>     non-NUL-terminated over-read). `owned_str` is **return-only** (rejected as a parameter).
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

## Architecture — pipeline

```
source.chz
  → Lexer        (indent-aware: INDENT/DEDENT tokens)
  → Parser       (Pratt expr parsing + recursive-descent stmts) → AST
  → Desugar      (AST → AST lowering: pipe, optional chaining/`??`, comprehensions, defaults)
  → Checker      (local inference; explicit fn sigs; machine-readable errors) → typed AST
  → [Phase 1] Tree-walk interpreter        ← reference semantics, working lang fast
  → [Phase 2] Bytecode compiler → Stack VM (+ mark-sweep GC)
```

Each component is an isolated, separately-testable module. Golden tests assert the tree-walker
and the VM produce identical output.

### Repo layout

```
src/
  lexer/        # chars → tokens, indent stack
  parser/       # tokens → AST (Pratt)
  ast/          # node definitions
  desugar/      # AST → AST lowering (pipe, ?., ??, comprehensions, defaults)
  checker/      # type inference + checking
  interp/       # tree-walk interpreter (Phase 1)
  compiler/     # AST → bytecode (Phase 2)
  vm/           # stack machine
  gc/           # mark-sweep
  runtime/      # builtins + native std modules
  resolver/     # module path resolution
  test_runner   # `chezzi test` — discovers + runs `test fn`s in `*_test.chz`
  main.rs       # `chezzi run/test/docs/repl/tokens/ast`
std/            # std modules written in Chezzi
examples/*.chz  # golden-test corpus + LLM eval material
tests/          # Rust unit + golden tests
```

## Roadmap

| # | Deliverable | Runnable proof |
|---|-------------|----------------|
| ✅ **M1** | Indent-aware lexer + REPL that echoes tokens | `chezzi tokens foo.chz` prints token stream incl. INDENT/DEDENT |
| ✅ **M2** | Parser → AST + pretty-printer | `chezzi ast foo.chz` round-trips source |
| ✅ **M3** | Tree-walk interpreter | Working language: arithmetic, fns, if/for/while, structs, enums, match, interpolation, Result+`?` run single-file |
| ✅ **M4** | Type checker (local inference) | Type errors caught pre-run with clear messages; `--errors=json` mode |
| ✅ **M4.5** | Modules / imports + resolver | Multi-file program runs; `chezzi.toml` root detection works |
| ✅ **M5** | Bytecode compiler + stack VM + mark-sweep GC | Runs on VM (default); ~4–6.5× over the tree-walker; golden + parity tests match |
| ✅ **M6** | Stdlib fill-out + pipe `\|>` operator + core-type methods | **Done**: str/list methods + pipe chains, plus M6c — the Level-2 native FFI seam (`NativeFn`+`Host`) shipping `std.math`/`io`/`os` (native) and `std.str` (Chezzi), running identically on both engines |
| ✅ **M7** | User-defined generics + structural protocols | Generic fns/structs, `Comparable` bound, `std.cmp` (`min`/`max`/`clamp`); golden tests on both engines |
| ✅ **M8** | Tier-1 stdlib | `std.json` (+ type-directed `decode[T]`), `std.process`, `std.fs`, `std.time`; the `set` type, iterable strings (`s.chars()`) |
| ✅ **M9** | Tier-2 stdlib | `std.regex` + `std.request` (first runtime crate deps; blocking); `Match`/`Response` structs |
| ✅ **M10** | Type-system depth | `Stringable`/`Hashable` + operator protocols (`Add`/`Sub`/`Mul`), generic enums, type aliases, multi-bound generics, any-`Hashable` map/set keys |
| ✅ **M11** | Panic recovery + Go-style errors | Phase A ✅ `Result[T, E]` + `Error` protocol; Phase B ✅ `recover:` boundary with try-block semantics. Both engines parity-tested |
| ✅ **M12** | Tier-3 ergonomics (part) | **Iterator protocol** (user structs with `next(self) -> Option[T]` iterable in `for`, lazy); **match guards** (`pattern if cond:`) + int **range patterns** (`1..10:`). Both engines parity-tested |
| ✅ **M13** | `Iterator[T]` protocol | The language's first **parameterized** protocol bound: `[S: Iterator[T], T]` accepts any iterable (built-ins intrinsically, structs via `next`) and recovers element type `T`. Lazy adapters (Take/Mapped) were the original answer to `yield` (then a non-goal; `yield`/generators have since shipped VM-only — see above). Checker/parser/grammar only; both engines parity-tested |
| ✅ **M14** | Generics depth | **Method-level type params** (a method's own `[U]`, inferred at call) + **user-defined parameterized protocols** (`protocol Container[T]`, structural conformance with concrete-arg bounds `[X: Container[int]]`) — the special-cased `Iterator[T]` generalized. Checker/parser/grammar only; both engines parity-tested |
| ✅ **M15** | Slicing + indexing protocols | Python-style `xs[a:b:c]` / `s[0:2]` / `xs[::-1]` (open bounds, step, reverse, bounds-clamped) + negative indexing `xs[-1]` (plain index faults out of range, slice bounds clamp — Python's asymmetry); the `..` operator stays the for-loop/match range. Prebuilt **`Index[K, V]` + `IndexSet[K, V]` + `Slice[R]`** structural protocols — built-in `list`/`map`/`str` conform intrinsically, user structs via `index`/`set_index`/`slice(self, start: int?=None, end: int?=None, step: int?=None)`, so `custom[k]`/`custom[k]=v`/`custom[a:b:c]` work and a generic can be bounded by `Index[int, V]`. Both engines parity-tested |
| ✅ **M16–M18** | Concurrency + `defer` | `spawn` / `parallel:` nursery, `Channel`/`Shared`/`Executor`, real OS-thread M:N engine (`--parallel`) with work-stealing + reduction-counting preemption + netpoller + `std.net`; `defer` (call + block forms). Design in [`docs/concurrency.md`](concurrency.md), phases in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md) |
| 🟦 **M19** | Perf track (in progress) | Landed: peephole + const-fold, superinstructions, global-slotting, struct-field inline cache, FxHash, `ConstStr` interning, call-loop flatten, small-string optimization. Behavior-preserving + two-engine parity on every change. Backlog ranked in [`docs/future.md §4`](future.md); measured deltas in [`docs/benchmarks.md`](benchmarks.md) |
| ✅ **M20** | In-language tests | `assert <cond>[, "<msg>"]` (both-engine statement primitive, faults with its source line), the `test fn` marker (free tests + struct **suites** with `before_all`/`after_all`/`before_each`/`after_each` hooks + a shared typed fixture), and `chezzi test [path]` — a Rust-side VM-only runner over `*_test.chz` files (`PASS/FAIL name (file:line) msg`, non-zero exit on failure). Surface in [`docs/syntax.md §9c`](syntax.md) |
| ✅ **M21** | Nominal `newtype` | `newtype Name = <type>` — a DISTINCT type wrapping the underlying (Go defined-type model), not a transparent alias: construct (`Name(x)`) / cast-unwrap (`int(n)`) cross the boundary; accidental mixing with the raw underlying or a different newtype is a compile error. Numeric (`int`/`float`) same-type operators auto-flow (native op, unwrap→op→rewrap); a `str`/`bool` newtype does not auto-inherit `+`/`<` (define a method); methods + `Stringable`/`Hashable`/`Add`/`Comparable` via the newtype's own methods (runtime hash/str dispatch, both engines). **Generic newtypes** (`newtype Stack[T] = list[T]`, Go defined-type model + generics): methods-only (no native operator auto-flow even for `Box[T] = T`); ctor infers type args (turbofish `Stack[int]([])` when an empty literal can't bind `T`); cast-unwrap propagates the instantiation (`list(s)` for `s: Stack[int]` ⇒ `list[int]`). v1 limits: aggregate underlyings get identity+construct+unwrap+own-methods only; no `derive`; no static / associated methods **on a newtype** (`Type.method()`) yet — a follow-up (static methods HAVE landed for struct + enum; see the "Static methods" note). Surface in [`docs/syntax.md §7b`](syntax.md) |
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
- **Golden tests** — `examples/*.chz` + `*.expected`; harness runs each through both tree-walker and VM, asserts identical stdout.
- **Manual end-to-end** via the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval** — feed the grammar cheatsheet + `--errors=json` to a model, measure first-try compile rate; failures feed grammar/error-message work.
- **Perf check** — tracked against CPython via the `benches/` harness (`benches/run.chz`, hyperfine);
  baseline + per-bench bottleneck analysis in [`docs/benchmarks.md`](benchmarks.md). After the M19
  phases: ~1.3×–3.9× slower than CPython (worst on call/alloc-bound benches), startup ~11× faster.
