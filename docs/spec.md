# Chezzi — Language Design Spec

A fast, statically-typed, Python-feel scripting language. Hand-built in Rust.

> **Status:** M1–M11 shipped (through panic recovery + Go-style `Result[T, E]`); M12 added the iterator protocol + match guards/range patterns; M13 added the parameterized `Iterator[T]` bound (`yield` dropped as a non-goal) — 951 tests passing. This doc is the source of truth for the *language design*; live build status lives in `PROGRESS.md` and the roadmap at the bottom.

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
- **String interpolation** — `"hi {name}, sum {a+b}"`. First-class; string ops are a UX priority.
- **Struct methods** — `fn dist(self)` on structs. Composition + structural `protocol`s, no classes or inheritance (Rust/Go style).
- **Pipe `|>`** — functional chaining. Implemented in M6 (parse-time desugar to a call).
- **Tuples** — `(1, "a")`, fixed-arity, immutable; nestable in patterns.
- **Transparent type aliases** — `type UserId = int` (M10).
- **Bitwise ops** — `& | ^ << >>` (int-only, M8/M11).
- **`recover:` block** — panic-recovery boundary → `Result[T, Error]` catching any runtime fault beneath it (M11).

**Shipped post-v1 (M7–M13):**
- **M7** — user-defined generics + structural protocols (generic fns/structs, `Comparable`; `std.cmp`).
- **M8** — tier-1 stdlib (`std.json`/`process`/`fs`/`time`), the `set` type, iterable strings (`s.chars()`).
- **M9** — tier-2 stdlib (`std.regex`, `std.request`) — first runtime crate deps.
- **M10** — type-system depth: `Stringable`/`Hashable` + operator protocols (`Add`/`Sub`/`Mul`), generic enums, type aliases, multi-bound generics (`T: Add + Mul`), any-`Hashable` map/set keys.
- **M11** — panic recovery (`recover:`) + Go-style `Result[T, E]` with the built-in `Error` protocol (`message(self) -> str`).
- **M12** — iterator protocol (structs with `next(self) -> Option[T]` iterable in `for`), match guards + range patterns.
- **M13** — `Iterator[T]`: the first **parameterized** protocol bound (`[S: Iterator[T], T]`) — any iterable, element type recovered; lazy adapter structs replace `yield`.

**Non-goals (by design, never):** classes & inheritance — Chezzi is composition-only with
structural `protocol`s, like Rust/Go (see *Locked decisions*). **`yield`/generators** — lazy sequences
are adapter structs over `Iterator[T]` (Rust model), so no coroutine runtime is ever needed.

**Still deferred (YAGNI v1):** concurrency, macros, package registry, native backend. Chezzi is
**single-threaded and synchronous** — both engines run one sequential loop, there is no async/await
and no scheduler, so all stdlib I/O (`std.request`, `std.fs`, …) blocks. A Go-style model (`go`
keyword + `chan` queue) is a possible future milestone but is large (scheduler, `Rc`→`Arc` value
sharing, a channel type across grammar/checker/both engines) and not part of v1. Most of the former
"what's missing" list has since shipped (M8–M11: `std.json`, generic enums, `Stringable`/`Hashable`/
numeric protocols, panic recovery, the **`Iterator[T]` protocol** (a parameterized bound:
`[S: Iterator[T], T]` accepts any iterable — built-in `list`/`set`/`str`/`map` intrinsically, or a
user struct with `next(self) -> Option[T]` — and recovers its element type `T`), and **match guards +
range patterns** — see *Shipped post-v1* above, plus **default + named arguments** for functions and
struct constructors); remaining gaps are mostly ergonomics (variadic args, comprehensions, slicing).
**`yield`/generators are a deliberate non-goal** — lazy sequences are written as adapter structs over
`Iterator[T]` (Rust's `Map`/`Take` model), so no coroutine runtime is needed. Open items stay tracked
in `gaps.md` → *Roadmap to a complete v1*.

### Syntax sketch

```chezzi
fn add(a: int, b: int) -> int:        # explicit params; '-> T' optional (inferred from body)
    return a + b

name := "thuan"
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

**Entry model.** Programs run top-to-bottom; there is no automatic `main`. An `Err`/`None` left
unhandled at the top level (a bare expression statement, or a top-level `?`) exits the program with
`unhandled error: …` and a non-zero code. A future `chezzi.toml` `entrypoint` (tooling-only) may
declare which function a project build runs.

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

## Standard library

- **Builtins (no import):** `print`, `len`, `range`, casts (`int()`/`str()`/`float()`),
  `ord`/`chr`, `set()`/`set(list)`, core-type methods (`s.upper()`, `s.chars()`, `xs.push()`,
  `m.get()`, `set.add()`).
- **Std modules v1 (shipped, M6c):** `std.math`/`std.io`/`std.os` (native-Rust via the FFI seam),
  `std.str` + `std.cmp` (written in Chezzi; `std.cmp` adds M7-G3). Imported with
  `import std.math` / `import f from std.io`. `std.cmp` holds generic `min`/`max`/`clamp`
  (`[T: Comparable]`); `list.sort()` is likewise Comparable. (`std.math.min`/`max` were retired into
  `std.cmp`; `abs` stays native.)
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
- **Later:** custom request headers / non-GET-POST verbs / a first-class compiled `Regex`.

> **Native FFI — Level-2 SHIPPED in M6c; Level-3 deferred.** Because Chezzi is written in Rust, the
> native-stdlib mechanism doubles as a foreign-function interface: bind a Rust fn and expose it as a
> module member, instead of reimplementing everything in Chezzi.
> - ✅ **`NativeFn`** — a Rust fn registered as a callable Chezzi value (member of a native module),
>   added to both `interp::Value` (`Native`) and `vm::Obj` (`Native`); parity-tested.
> - ✅ **`Host` trait** (`src/native/mod.rs`) — the engine-agnostic context a native fn uses
>   (`arg_int`/`arg_float`/`arg_str`, stdout/stderr/stdin, args/env/cwd) so a binding is written
>   once and runs on both the interp (Rc values) and the VM (heap handles). Returns flow back as an
>   engine-neutral `NativeRet`, lowered to each engine's value *after* the call (GC-safe).
> - **Dependency policy:** default build stays **zero third-party crates** (Rust `std` only);
>   crate-backed bindings would ride behind **Cargo features** (`--features regex`).
> - **Still deferred (Level-3):** **Userdata** (`Box<dyn Any>` for opaque `File`/`Regex` handles —
>   io is whole-string for now), and *dynamic* `cdylib` plugins over a stable C ABI. `std.os.exit`
>   awaits an exit-code channel through the run drivers.

## Architecture — pipeline

```
source.chz
  → Lexer        (indent-aware: INDENT/DEDENT tokens)
  → Parser       (Pratt expr parsing + recursive-descent stmts) → AST
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
  checker/      # type inference + checking
  interp/       # tree-walk interpreter (Phase 1)
  compiler/     # AST → bytecode (Phase 2)
  vm/           # stack machine
  gc/           # mark-sweep
  runtime/      # builtins + native std modules
  resolver/     # module path resolution
  main.rs       # `chezzi run/repl/tokens/ast`
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
| ✅ **M13** | `Iterator[T]` protocol | The language's first **parameterized** protocol bound: `[S: Iterator[T], T]` accepts any iterable (built-ins intrinsically, structs via `next`) and recovers element type `T`. Lazy adapters (Take/Mapped) replace `yield` (a non-goal). Checker/parser/grammar only; both engines parity-tested |
| **Stretch** | Cranelift AOT/JIT backend | Near-Go native speed (optional) |

> Native FFI (Level-2 compiled-in bindings) **shipped in M6c** — see the *Standard library* note
> above. Level-3 (dynamic `cdylib`/C-ABI plugins, userdata) remains a future idea.

## Verification

- **Per-phase Rust unit tests** — lexer token streams, parser AST shapes, checker accept/reject cases.
- **Golden tests** — `examples/*.chz` + `*.expected`; harness runs each through both tree-walker and VM, asserts identical stdout.
- **Manual end-to-end** via the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval** — feed the grammar cheatsheet + `--errors=json` to a model, measure first-try compile rate; failures feed grammar/error-message work.
- **Perf check** — after M5, benchmark a loop-heavy script tree-walker vs VM; target ~10x.
