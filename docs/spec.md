# Chezzi — Language Design Spec

A fast, statically-typed, Python-feel scripting language. Hand-built in Rust.

> **Status:** design locked, pre-M1. This doc is the source of truth for the language; the build roadmap lives at the bottom.

## Goals (ranked)

1. **Learn** — hand-build the guts (lexer, parser, type checker, VM, GC). No transpiling, no codegen shortcuts.
2. **Usable tool** — bytecode VM for ~10x over a tree-walker; real modules so programs split into files.
3. **LLM-friendly** — static types as guardrails, explicit signatures, machine-readable compiler errors, small orthogonal grammar.

Closest existing cousins (read, don't copy): **Crystal**, **Nim**, plus *Crafting Interpreters* for the implementation path.

## Locked decisions

| Decision | Choice |
|----------|--------|
| Implementation host | **Rust** |
| Execution model | **Tree-walk first → bytecode stack VM** |
| Type system | **Static, local inference** (Go-style: explicit fn signatures, inferred locals) |
| Surface syntax | **Indentation blocks** (Python-feel; lexer emits INDENT/DEDENT) |
| Errors | **Result/Option + `?`** (errors as values, no hidden control flow) |
| Memory | **Mark-sweep GC** (hand-built; primitives unboxed) |
| Name / ext / binary | **Chezzi** / `.chz` / `chezzi run foo.chz` |

## Language v1 — feature set

**Core:** `int float bool str`, `list[T]`, `map[K,V]`, `fn`, `struct`, `enum`, `if/else`, `for/while`,
`Result[T]` & `Option[T]` + `?`, closures (`fn(x): x*2`), built-in generics (`list`/`map`/`Result`).

**Included:**
- **Pattern matching** — `match` on enums, exhaustiveness-checked.
- **String interpolation** — `"hi {name}, sum {a+b}"`. First-class; string ops are a UX priority.
- **Struct methods** — `fn dist(self)` on structs. Light OOP, no inheritance.
- **Pipe `|>`** — functional chaining. Implemented **last** (M6).

**Deferred (YAGNI v1):** classes/inheritance, concurrency, user-defined generics, macros, package registry, native backend.

### Syntax sketch

```chezzi
fn add(a: int, b: int) -> int:        # explicit signature, inferred locals elsewhere
    return a + b

name := "thuan"
print("hi {name}")                     # interpolation

struct Point:
    x: int
    y: int
    fn dist(self) -> float:            # struct method
        return sqrt(self.x*self.x + self.y*self.y)

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
```

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
  core-type methods (`s.upper()`, `xs.push()`, `m.get()`).
- **Std modules v1:** `std.io`, `std.math`, `std.str` (rich — UX priority), `std.os`.
  Native-Rust for io/os/math; some written in Chezzi (dogfooding).
- **Later:** `std.list`, `std.map`, `std.json`, `std.time`.

> **Future idea — native FFI (NOT scheduled; not part of any current milestone).** Because Chezzi
> is written in Rust, the native-stdlib mechanism could later double as a foreign-function
> interface: bind a Rust library and expose it as a module, instead of reimplementing everything in
> Chezzi early on. Sketch for when/if we pick it up:
> - **`NativeFn`** — a Rust fn registered as a callable Chezzi value (member of a native module),
>   added to both `interp::Value` and `vm::Obj` (parity-tested).
> - **`Host` trait** — the engine-agnostic context a native fn uses (`arg_int`/`arg_str`,
>   `new_str`/`new_list`, `raise`, …) so a binding is written once and works on both the interp
>   (Rc values) and the VM (heap handles). This trait would *be* the stdlib's native API too.
> - **Userdata** — an opaque value wrapping `Box<dyn Any>` so Chezzi can carry but not inspect a
>   native Rust object (`File`, `Regex`, …). Lua-style userdata / Python capsule.
> - **Dependency policy if pursued:** default build stays **zero third-party crates** (Rust `std`
>   only); crate-backed bindings ride behind **Cargo features** (`--features regex`), erroring
>   "module not available" otherwise. FFI is the unsafe, explicit-opt-in seam — never the core.
> - **Compiled-in bindings are easy** (native fns are Rust in the same crate — no ABI/marshalling).
>   The hard, host-independent parts are deferred: *dynamic* `cdylib` plugins over a stable C ABI,
>   and the dual-backend (interp+VM) duplication tax.
>
> Current milestones (M6+) do **not** include this — record-only so future sessions have the design.

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
| **M6** ← next | Stdlib fill-out + pipe `\|>` operator + core-type methods | `std.io/math/str/os` usable; pipe chains run |
| **Stretch** | Cranelift AOT/JIT backend | Near-Go native speed (optional) |

> Native FFI / Rust-library bindings are a **future idea, not on this roadmap** — see the
> "Future idea — native FFI" note under *Standard library* above.

## Verification

- **Per-phase Rust unit tests** — lexer token streams, parser AST shapes, checker accept/reject cases.
- **Golden tests** — `examples/*.chz` + `*.expected`; harness runs each through both tree-walker and VM, asserts identical stdout.
- **Manual end-to-end** via the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval** — feed the grammar cheatsheet + `--errors=json` to a model, measure first-try compile rate; failures feed grammar/error-message work.
- **Perf check** — after M5, benchmark a loop-heavy script tree-walker vs VM; target ~10x.
