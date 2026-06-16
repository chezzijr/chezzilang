# Chezzi — Language Design Spec

A fast, statically-typed, Python-feel scripting language. Hand-built in Rust.

> **Status:** core language **feature-complete through M18** (scalars + collections, generics +
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
  open int/str/bool domains — including `true | false` — still require a `_`). Variants are bare by
  default but may also be written **qualified** as `Enum.Variant` (value, constructor, or `match` arm)
  — an equivalent spelling, not a per-enum namespace (variant names stay program-global).
- **String interpolation** — `"hi {name}, sum {a+b}"`. First-class; string ops are a UX priority.
  Supports Python-style **format specifiers** after a `:` — `{expr:[[fill]align][sign][0][width][.precision][type]}`,
  e.g. `{name:>10}` (right-align width 10), `{f:.2f}` (2 decimals), `{n:04d}` (zero-pad), `{pct:.1%}`
  (percent), `{255:x}` (hex). Type chars: `d f x X b o e %`. **Width and precision are capped at 4096**
  (a larger spec is a parse error — never a giant allocation). String `.N` truncates; an unknown type
  char or a type/value mismatch is reported before any output (runtime-prefixed; not caught by
  `check`). A bare interpolated ternary works; parenthesize to give it a spec (`{(if b: 1 else: 2):>5}`).
  The spec parser+formatter is shared by both engines (`src/fmtspec.rs`) → byte-identical output for
  well-formed programs. Full grammar in [`syntax.md` §10](syntax.md).
- **Literal forms** — int (`42`, `0xFF`/`0b1010`/`0o17`, `_` separators), float (`3.14`, scientific `6.022e23`/`1e3`/`1.5e-9` — any exponent ⇒ float), str in either `"…"` or `'…'` (interchangeable: same escapes & interpolation), also **triple-quoted** `"""…"""` / `'''…'''` (same escapes/interpolation, but unescaped quotes allowed inside) with escapes `\n \t \r \\ \" \' \0` and `\u{HEX}` unicode (1-6 hex digits). See `docs/syntax.md §2/§10`.
- **Membership & assignment ops** — `x in xs` membership (`bool`; list/set element, map **key**, str substring); compound assignment `+= -= *= /= %= &= |= ^= <<= >>=` (= `x = x OP v`; bitwise forms int-only); and multi-target / tuple-swap assignment `a, b = b, a` (RHS evaluated first). See `docs/syntax.md §3/§4`.
- **Struct methods** — `fn dist(self)` on structs. Composition + structural `protocol`s, no classes or inheritance (Rust/Go style).
- **Pipe `|>`** — functional chaining. Implemented in M6 (parse-time desugar to a call).
- **Tuples** — `(1, "a")`, fixed-arity, immutable; nestable in patterns.
- **Transparent type aliases** — `type UserId = int` (M10).
- **Bitwise ops** — `& | ^ << >>` (int-only, M8/M11).
- **`recover:` block** — panic-recovery boundary → `Result[T, Error]` catching any runtime fault beneath it (M11).

**Shipped post-v1 (M7–M18):**
- **M7** — user-defined generics + structural protocols (generic fns/structs, `Comparable`; `std.cmp`).
- **M8** — tier-1 stdlib (`std.json`/`process`/`fs`/`time`), the `set` type, iterable strings (`s.chars()`).
- **M9** — tier-2 stdlib (`std.regex`, `std.request`) — first runtime crate deps.
- **M10** — type-system depth: `Stringable`/`Hashable` + operator protocols (`Add`/`Sub`/`Mul`), generic enums, type aliases, multi-bound generics (`T: Add + Mul`), any-`Hashable` map/set keys.
- **M11** — panic recovery (`recover:`) + Go-style `Result[T, E]` with the built-in `Error` protocol (`message(self) -> str`).
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
calling C functions via dlopen+libffi) — v1 marshals scalars only (int/float/bool/str→`char*`);
structs-by-value, callbacks, varargs, opaque pointers / userdata, and `char*` ownership transfer are
still deferred. See the FFI subsection below + [`docs/syntax.md`](syntax.md).

**`yield`/generators** were originally a deliberate non-goal (lazy sequences are written as adapter
structs over `Iterator[T]`, Rust's `Map`/`Take` model). They are now a **complete, VM-only**
feature: a `fn` declaring `-> Iterator[T]` may `yield` values; calling it returns a
suspendable generator usable anywhere an `Iterator[T]` is (`for` loops, `Iterator[T]` bounds). It is
VM-only — the frozen tree-walk interpreter rejects `yield` (it cannot suspend a native Rust call),
so two-engine parity is **waived** for generators. The adapter-struct model remains the
parity-clean, recommended way to write lazy sequences. Live status is tracked in
[`PROGRESS.md`](../PROGRESS.md).

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

**Multi-line literals.** Inside `[]`, `{}`, and `()` the lexer suppresses layout (newlines /
indentation), so collection literals, call arguments, and parameter lists may span lines. A single
optional trailing comma is accepted before the closer (`[1, 2,]` ≡ `[1, 2]`); a lone comma is still
an error. `(x)` is grouping; `(x,)` is a one-element tuple. (See [`syntax.md` §2](syntax.md) and the
collection/`<params>`/`<argList>` productions in [`grammar.bnf`](grammar.bnf).)

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
  `std.cmp`; `abs` stays native.) **Integer overflow policy:** the one integer type is `i64`; every
  overflow — arithmetic (`+ - * / %`), negation, `MIN / -1`, and `math.abs(MIN)` — is a *recoverable
  panic* (`"integer overflow in <op>"`, catchable by `recover:`), never a silent wrap and never a host
  crash. No `byte`/`u8` scalar (Python model — binary data is the immutable `bytes` *sequence* type, **shipped**, not a
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
  Remaining non-goals: a `byte`/`u8` scalar, encode/decode codecs (base64/hex are a separate `std.*`
  gap), and byte-sequence methods beyond the tables + Display.
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
>   The checker enforces **C-marshallability** (only int/float/bool/str params + returns, void return)
>   on the resolved type, so a non-scalar param is a compile error. The `Cffi` keeps its `Library`
>   alive + stores the resolved symbol as a `usize` (libloading `Symbol` is `!Send`) and rebuilds the
>   `Cif` per call (libffi `Cif` is `!Send`), so it is `Send + Sync` for `--parallel`; the M:N
>   snapshot path shares the `Arc<Cffi>` (same address space — no re-`dlopen`). See `src/native/cffi.rs`
>   + `examples/ffi.chz`. **v1 limits:** scalars only — structs-by-value, callbacks/function pointers,
>   varargs, opaque pointers / userdata, and `char*` ownership transfer / `free` are deferred (a
>   `char*` return is copied immediately; a malloc'd return leaks). A slow C call runs inline (extern
>   names are NOT in `is_blocking`, so it pins its worker under `--parallel`).
> - **FFI v1 limits (known + by design):**
>   - **Integer width (FFI-2):** Chezzi `int` (i64) marshals as C **`long`** — 64-bit on every
>     supported **LP64** unix target. 32-bit C ints, `unsigned`, and other fixed-width C integer types
>     are **out of v1 scope**; there is **no `int32` type** (the language is feature-frozen). A C API
>     taking a 32-bit `int` parameter still works in practice on LP64 (the value is passed in a 64-bit
>     register and the callee reads the low 32 bits), but a value that does not fit the C type's range
>     is the caller's responsibility. **Non-unix is unsupported:** on an LLP64 target (Windows x64) C
>     `long` is 32-bit and would truncate, so the checker **rejects `extern` on non-unix targets** and
>     the `cffi` module is `#[cfg(unix)]`-gated.
>   - **`char*` return leaks (FFI-3):** a `str`-typed return is copied immediately into an owned Chezzi
>     string, but the C pointer is **never `free`d** — v1 has no ownership transfer. A function that
>     returns a freshly `malloc`'d `char*` (rather than a static / interned string) therefore **leaks**
>     that allocation on every call. Prefer C APIs that return a borrowed/static string, or accept the
>     leak for short-lived programs. (Code-commented at `src/native/cffi.rs`.)
>   - **No `--parallel` serialization / non-reentrant C (FFI-7):** `extern` calls are **NOT** serialized
>     under `--parallel` — two OS-thread workers can be inside C code at the same time. Calling a
>     **non-reentrant** C function (e.g. `strtok`, `gmtime`/`localtime`, `setlocale`, anything using
>     `errno` carelessly or static internal buffers) concurrently **races at the C level** (Chezzi
>     cannot guard state it does not own). Use only thread-safe/reentrant C entry points under
>     `--parallel`, or confine such calls to the sequential engines.
> - **Still deferred (Level-3):** **Userdata** (`Box<dyn Any>` for opaque `File`/`Regex` handles —
>   io is whole-string for now), and the deferred FFI features above (structs/callbacks/varargs/
>   userdata). (`std.os.exit` with a real exit-code channel through the run drivers has since
>   **shipped** — see `examples/exit.chz`.)

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
  main.rs       # `chezzi run/test/repl/tokens/ast`
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
| ✅ **M14** | Generics depth | **Method-level type params** (a method's own `[U]`, inferred at call) + **user-defined parameterized protocols** (`protocol Container[T]`, structural conformance with concrete-arg bounds `[X: Container[int]]`) — the special-cased `Iterator[T]` generalized. Checker/parser/grammar only; both engines parity-tested |
| ✅ **M15** | Slicing + indexing protocols | Python-style `xs[a:b:c]` / `s[0:2]` / `xs[::-1]` (open bounds, step, reverse, bounds-clamped) + negative indexing `xs[-1]` (plain index faults out of range, slice bounds clamp — Python's asymmetry); the `..` operator stays the for-loop/match range. Prebuilt **`Index[K, V]` + `IndexSet[K, V]` + `Slice[R]`** structural protocols — built-in `list`/`map`/`str` conform intrinsically, user structs via `index`/`set_index`/`slice(self, start: int?=None, end: int?=None, step: int?=None)`, so `custom[k]`/`custom[k]=v`/`custom[a:b:c]` work and a generic can be bounded by `Index[int, V]`. Both engines parity-tested |
| ✅ **M16–M18** | Concurrency + `defer` | `spawn` / `parallel:` nursery, `Channel`/`Shared`/`Executor`, real OS-thread M:N engine (`--parallel`) with work-stealing + reduction-counting preemption + netpoller + `std.net`; `defer` (call + block forms). Design in [`docs/concurrency.md`](concurrency.md), phases in [`docs/concurrency-tier-d.md`](concurrency-tier-d.md) |
| 🟦 **M19** | Perf track (in progress) | Landed: peephole + const-fold, superinstructions, global-slotting, struct-field inline cache, FxHash, `ConstStr` interning, call-loop flatten, small-string optimization. Behavior-preserving + two-engine parity on every change. Backlog ranked in [`docs/future.md §4`](future.md); measured deltas in [`docs/benchmarks.md`](benchmarks.md) |
| ✅ **M20** | In-language tests | `assert <cond>[, "<msg>"]` (both-engine statement primitive, faults with its source line), the `test fn` marker (free tests + struct **suites** with `before_all`/`after_all`/`before_each`/`after_each` hooks + a shared typed fixture), and `chezzi test [path]` — a Rust-side VM-only runner over `*_test.chz` files (`PASS/FAIL name (file:line) msg`, non-zero exit on failure). Surface in [`docs/syntax.md §9c`](syntax.md) |
| **Stretch** | Cranelift AOT/JIT backend | Near-Go native speed (optional; only once the language has truly stopped moving) |

> Native FFI (Level-2 compiled-in bindings) **shipped in M6c**; **Level-3 dynamic C-ABI FFI v1
> shipped** (`extern "lib":` scalar calls via dlopen+libffi) — see the *Standard library* note above.
> The remaining Level-3 surface (structs-by-value, callbacks, varargs, opaque pointers / userdata)
> is still a future idea.

## Verification

- **Per-phase Rust unit tests** — lexer token streams, parser AST shapes, checker accept/reject cases.
- **Golden tests** — `examples/*.chz` + `*.expected`; harness runs each through both tree-walker and VM, asserts identical stdout.
- **Manual end-to-end** via the `chezzi` CLI subcommand for each phase (`tokens`/`ast`/`run`).
- **LLM-codegen eval** — feed the grammar cheatsheet + `--errors=json` to a model, measure first-try compile rate; failures feed grammar/error-message work.
- **Perf check** — tracked against CPython via the `benches/` harness (`benches/run.chz`, hyperfine);
  baseline + per-bench bottleneck analysis in [`docs/benchmarks.md`](benchmarks.md). After the M19
  phases: ~1.3×–3.9× slower than CPython (worst on call/alloc-bound benches), startup ~11× faster.
