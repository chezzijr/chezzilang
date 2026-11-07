## 9. Errors — `T!E` / `T?` + `?`

Errors are **values**, not exceptions. No hidden control flow.

```chezzi
fn safe_div(a: int, b: int) -> int!:        # int! == int!Error
    if b == 0:
        return !"divide by zero"             # a str IS an Error (see below)
    return ?(a / b)

fn calc() -> int!:
    x := safe_div(10, 2)?     # '?' unwraps a success, or returns the error from THIS function
    y := safe_div(x, 0)?      # on an error, calc() returns it immediately
    return ?(x + y)
```

**Types and values.** A plain value wraps into the carrier its slot expects:

| type | means | values |
|---|---|---|
| `T?` | `T` or absent (`T?`) | a plain `T`, `None`, `?x` |
| `T!E` / `T!` | `T` or an error `E` (`T!E`; `T!` = `T!Error`) | a plain `T`, `!e`, `?x` |
| `None` | returns nothing (an annotation only: `fn log(m: str) -> None`) | — |
| `None!E` | nothing, or an error (also written `!E`) | `!e`; a bare `return` or falling off the end is success |
| `T??` | nested optional (`T??`) | `None` is the OUTER absent, `?None` the inner one |

```chezzi
fn save(path: str) -> None!str:
    if path == "":
        return !"empty path"     # prefix `!` builds an error value; there is no `fail` keyword
    write(path)                  # falling off the end returns None

fn find(xs: List[int], k: int) -> int?:
    for x in xs:
        if x == k:
            return x             # wraps: x
    return None

x: int? = 5                      # 5
rs: List[int!str] = [1, !"disk", 3]   # [1, !disk, 3]
```

- **`!e` builds an error value.** Its operand must satisfy `Error` (`!5` is `int does not satisfy
  Error`); the prefix applies to the whole postfix operand (`!wrap(e)`). `!e` is an ordinary expression
  and works at any typed slot.
- **Implicit wrap at every typed slot** (the full list is "Success-coercion" in §3): a plain `T`
  becomes `?v` at `T?` and at `T!E`. Assignment targets include `x = 5`, `s.f = 5`, `xs[i] = 5`,
  `x, y = 5, 0`; elements include `List[T].from_iter(...)` / `Map[K, V].from_iter(...)` / `Shared[T](...)`. Each branch of
  an `if`/`match` value wraps on its own (`x: int? = if c: 5 else: None`).
  - A carrier is never re-wrapped (`x: int?? = ?5` is an error).
  - No int→float step (`fn f() -> float?: return 1` is an error).
  - A slot that mentions an unpinned type parameter (`-> T?`) does not wrap.
  - An operator operand is not a slot (`x: int? = 1 + 2` wraps the sum, `?3`); nor is the return of a
    fn with no `->`.
  - With no written type, a value wraps only beside a written `None` (the join rule, §3).
- **`?x` builds a present/success value**, its carrier from the expected type: `?5` at `int?` and at
  `int!E`; `x: int?? = ?None` is `?None`.
- **No expected type.** `y := ?5` is `int?`; a later use does not re-read it (`take(y)` with
  `take(r: int!str)` is a mismatch: write `y: int!str = ?5`). Inside one statement `?x` meets its
  sibling: `r == ?2` beside an `int!` compares as `int!`. `w := !"disk"` is the error
  ``a `!` value needs its type from an annotation: add `-> T!E` to the function, or annotate the binding, e.g. `w: int!str = !e` ``.
- **A `None` needs its type on its own statement.** `z := None` is an error; write `z: int? = None`. A
  literal `None` beside a value joins to that value's optional (`[None, 7]` is `List[int?]`). No
  message prints an open type: `a: int = None` is `cannot assign None to variable of type int`.
- **Set elements and map keys** never wrap: no carrier is `Hashable`.

**`None!E`'s success value** prints `None`, as Python prints its void value:

```chezzi
fn f() -> None!str:
    return

fn main():
    print(f())      # None
```

**The `Error` type (Go-style).** `E` defaults to the built-in `Error` protocol, with one method
`message(self) -> str`. `str` conforms (its message is itself), so `!"…"` works everywhere. For a
structured error, define a struct with `message` and name it in `T!E`:

```chezzi
protocol Error:                 # built-in; shown for reference
    fn message(self) -> str

struct DbErr:
    code: int
    fn message(self) -> str:
        return "db error {self.code}"

fn query() -> Row!DbErr:        # Row!DbErr
    return !DbErr(503)

match query():
    ?row: use(row)
    !e:  print(e.message())   # message() is declared by DbErr itself
# `line()` / `col()` / `file()` are the bare `Error` existential's origin accessors (see `recover:`
# below) — they are NOT available on a user error type like `DbErr`, which carries only what it
# declares. Spell the sink `Row!` (i.e. `Row!Error`) to get them.
```

**`?` must match the enclosing function's return kind.** A `T!E`-`?` needs a `T!E`-returning fn (its
error type must fit the fn's), a `T?`-`?` a `T?`-returning fn; mixing kinds is a compile error.

- A function must return `T!E`/`T?` to use `?`. There is no `fn main` exception: a None-returning fn
  (named or nested) that uses `?` is a compile error.
- Module top-level code accepts either kind: an unhandled `!e`/`None` exits the program (rc=1).
- A manifest `module:function` entrypoint may be `-> T!` and use `?`; if it returns `!e`/`None`,
  `chezzi run` prints `unhandled error: <msg>` (rc=1).
- `?.` on a `T!E` is this same `?`, so every rule here applies to it.

**Optional chaining `?.`** works on both carriers:

```chezzi
name := user?.profile?.name ?? "anon"   # T?: None anywhere short-circuits, then ?? defaults
n    := fetch()?.len()                  # T!E: propagate the error (`?`), then `.len()` the value
```

- **On a `T?`**, `x?.field` / `x?.method(args)`: `None` gives `None`; `?v` applies the access to `v`
  and re-wraps, so the result is a `T?` (a `T?` field is not flattened: `U??`).
- A method that returns nothing (`c?.bump()`) runs when `c` is present and has no value (Swift and
  Kotlin's `x?.m()`). It is legal where a plain void call is (a statement, an inline fn or closure
  body, an `if` / `match` / `for` body) and gets no discarded-value warning. As a value
  (`x := c?.bump()`) it is `expression returns no value (None) and cannot be used as a value`.
- **On a `T!E`**, `x?.field` means `?` then `.`: propagate the `!e` out of the fn, then access the
  `T`. It is identical to `x? .field`, with the same return-kind rule (module top level accepts
  either), the same behaviour in `recover:`, and the same rejection inside `spawn:` and `defer:`.
  `f()?.len()` is Rust's idiom.
- The `T!E` form does not chain tries through a nested carrier: with `a: X?` and `a.b: Y!E`,
  `a?.b?.c` is an `(Y!E)?` followed by a field access on a `T!E`, an error.
- A `?.` method call takes type arguments as a `.` call does, on either carrier: `r?.into[str]()`,
  `r?.m[int](1)`. A one-arg bracket on a data member indexes then calls, as after `.`:
  `h?.fs[k](10)` is `h?.fs` indexed by `k`, then called with `10`, and so is a bracket that is no
  type (`h?.fs[0](10)`, `h?.m["a"](10)`). A failed call under `?.` reports what `.` reports, once,
  and names no receiver.

**Null-coalescing `??` takes a `T?` or a `T!E`.** `a ?? b` yields `a`'s inner value if present, else
`b`. It is right-associative (`a ?? b ?? c` = `a ?? (b ?? c)`). On a `T!E` the error is discarded
(Rust's `unwrap_or`), without a warning. On a non-carrier operand:

```
'??' applies to a `T?` or `T!E` value, found int
```

`f()?.len() ?? 0` on a `T!E` is that error: `f()?.len()` is already an `int`. Each operator needs
its two chars adjacent (`x?.f`, `a ?? b`); `a ??b` is a lex error and `oo(x)??` is two `?`
(`02-variables-and-types.md`). `?.` on a non-carrier is
`'?.' applies to a `T?` or `T!E` value, found int`.

**Unhandled errors exit the program only at a `?` or a manifest entrypoint's return.** A top-level
`?` on `!e`/`None` exits with `unhandled error: <detail>` and a non-zero code; so does a manifest
entry fn that returns `!e`/`None`. A bare top-level statement that evaluates to `!e`/`None` does not
exit (the value is discarded), and binding it (`r := compute()`) handles it.

**A discarded `T!E`/`T?` is lost at runtime in every position, so `chezzi check` warns** (Rust marks
both carriers `#[must_use]`):

```chezzi
fn g() -> int!Error: return !"E"
g()                    # warning … the `int!` value returned by 'g' is discarded, and rc stays 0
fn f():
    g()                # warning … the `int!` value returned by 'g' is discarded, and rc stays 0
f()
```

```
warning (line 4, col 5): the `int!` value returned by 'g' is discarded — bind it (`r := g()`), or discard it explicitly (`_ := g()`)
```

The warning is non-fatal. Bind the value (`r := g()`) or discard it explicitly (`_ := g()`). The hint
spells the call back only for a nullary call like `g()`; a call with arguments (`takes(1, "a")`) or a
method call (`xs.pop()`) shows `r := …`.

| statement position | runtime | warns |
|---|---|---|
| any statement position, including module top level | value discarded, rc unchanged | **yes** |
| `defer g()` / `spawn g()` — the **call** forms, in any position | silently swallowed | no — *deliberate*, see below |
| the drop happens on a value typed by a **type parameter** (`fn drop_it[T](x: T): x`) | silently swallowed | no — *a known limit*, see below |

- "Any position" includes nesting in `if`/`for`/`while`/`match`/`parallel:`/`recover:`/`wait:`, a
  `spawn:` or `defer:` block, and a function body.
- No warning where a bare carrier is not a drop: an inline-expr body (`fn f() -> T!: g()`), the
  trailing expression of a value-`match`/value-`if`, and `g()?` / `x ?? d`. The last statement of a
  `recover:` block is an ordinary statement. `o()?.len()` warns (`?.` re-wraps).
- **`defer` is excluded on purpose:** `defer f.close()` is Go's unchecked idiom. Wrap it in a function
  if you want the error. `spawn g()` is excluded too (a task's return value is discarded). The block
  forms `defer:` / `spawn:` warn.
- **A carrier passed through a type parameter is not seen** (as in Rust):

```chezzi
fn g() -> int!Error: return !"E"
fn drop_it[T](x: T):
    x                  # type is `T`, not a carrier — NO warning
drop_it(g())           # prints "after", rc=0; the error is gone
print("after")
```

### `recover:` — the panic-recovery boundary

A **runtime fault** (index out of bounds, divide by zero, integer overflow, a missing map key) is a
*panic* and ends the program by default. A `recover:` block catches any panic beneath it, at any
depth, and yields a `!` (the error or nothing):

```chezzi
total := 0
r := recover:
    rows := parse(file)       # may panic deep inside
    total = rows[0] / rows[1] # OOB / divide-by-zero is caught here
match r:
    ?_:  print(total)      # success: the block ran to its end
    !e: print("recovered: {e.message()}")   # a fault becomes !message
```

**The inline form** `recover: expr` is an expression of type `T!`, legal in every expression position:
`print(recover: a / b)`, `x := (recover: parse(s))`, `f(a, key=recover: g())`, `[recover: 1]`,
`print(1 + recover: 1)`. Its body is one expression that extends to the right like a lambda body
(`recover: a + b` recovers `a + b`); use parentheses to end it earlier (`(recover: a) + b`). For an
assignment, an `assert` or a `return`, use the block form. A body that diverges
(`recover: panic(…)`) is bottom-typed.

**The block form has no tail value.** Its type is `!` (`None!`). The last statement is an ordinary
statement (`x / 2` there is discarded). To get a value out, assign inside the block (`v := 0`, then
`v = <value>`) and read `v` after. Reading the block as a `T` is
`` expected int!, found ! — this value has type `!`: a `recover:` block has no tail value; assign inside it (`v := 0`, then `v = <value>` in the block) ``.

A `?` inside the block goes to the boundary (the `!e` lands in `r`), so one `recover:` handles panics
and propagated errors, even in a function that does not return `T!E`:

```chezzi
fn run() -> str:                       # not a `T!E`-returning function
    n := 0
    r := recover:
        n = parse_int(input)? * 2      # an error here lands in `r`, not propagated out of run()
    match r:
        ?_:  return "got {n}"
        !e: return "failed: {e.message()}"
```

`return`/`break`/`continue` that would leave a `recover:` are rejected (`'break' is not allowed
inside a recover block: a recover block produces a value and cannot leave to the loop`). A `?` on a
`T?` inside it is rejected (use `match`). A `yield` may appear inside it. Use `recover:` at boundaries (a request, a REPL line, a plugin, a test); use
`T!E`/`?` for expected failures.

**`panic(msg: str)` raises the same recoverable fault from your code.** It unwinds and returns no value
(it is not `return !e`). The nearest `recover:` catches it as `!e` with `e.message() == msg`;
uncaught, it ends the program with that message and a non-zero exit code. `defer`s run during the
unwind; if a `defer` panics during the unwind, the newer panic replaces the old one. `panic` is
bottom-typed: it fits any position (a statement, the tail of a branch with no `return`, or
`x := if ok: v else: panic("no")`, which takes `v`'s type).

```chezzi
r := recover:
    panic("boom")                 # raised here, caught at the boundary
match r:
    ?_:  print("ok")
    !e: print("recovered: {e.message()}")   # → recovered: boom
```

**A fault caught by `recover:` carries its origin:** `e.line() -> int?`, `e.col() -> int?`,
`e.file() -> str?`, the coordinate an uncaught fault prints (`runtime error (file:line:col): message`).

- A `!"..."` value made by user code, and an error in flight through `?`, have `None` for all three.
  Only `recover:` (and a `defer`-fault boundary) stamps the origin.
- `panic(e.message())` re-raises at `e`'s origin, not at the `panic` line.
- A fault raised inside a stdlib function (for example `submit` on a shut-down `Executor`) is located
  at your call into the stdlib (Rust's `#[track_caller]`), caught or uncaught; the trace keeps its
  frames. A stdlib `panic(...)` stays unstamped (`None`).
- A fault that `recover:` catches leaves no trace behind, a fault in a recover-block `defer`'s
  callback included: a later uncaught fault prints its own trace (fixed 2026-11-07; before, it
  could print the caught `defer` callback's frames).

### `defer` — block-scoped cleanup

`defer <call>` runs a call when the **enclosing lexical block** exits, on every path: fall-through,
`break`/`continue`, return, a `?` short-circuit, or a panic.

- Deferred calls run **LIFO**. An unwind across several blocks runs inner blocks' defers first.
- A `return`, `?`, `break` or `continue` that leaves a `parallel:` block runs that block's defers,
  cancels its children and waits for them (their defers run), then runs enclosing blocks' defers.
- The receiver and arguments are evaluated **at the `defer` statement** (Go semantics).
- Every indented block is a defer scope: a function body, a loop body, an `if`/`elif`/`else` branch,
  a `recover:` block, a statement `match` arm, and the module top level.
- A `defer` is never an inline body: `if c: defer f()` is a parse error (`a nested block must be
  indented, not written inline after ':'`).

```chezzi
fn process(path: str) -> int!:
    f := open(path)
    defer f.close()           # runs however `process` exits
    n := f.read_int()?        # if this short-circuits, f.close() still runs
    return ?(n * 2)

for path in paths:
    f := open(path)
    defer f.close()           # runs at the END of each iteration — no leak across the loop
    use(f)
```

**What `defer` (and `spawn`) can call.**

- A method call, or a call to a first-class callable value (a function, a closure, or a name bound to one).
- The four universe builtins `print`, `ord`, `chr` and `panic` are first-class: `defer print("done")`
  works, and `f := ord; f("a")` binds one.
- Constructors are not values, so not targets: type / container / runtime constructors (`int`, `str`,
  `List`, `Map`, `Channel`, `Range`, …), user struct/enum constructors, and every dotted spelling of
  one: `defer E.A(3)`, `defer E[int].A(3)`, `defer lib.Col.Val(3)`, `defer lib.Pt(3)`,
  `defer concurrency.Shared(0)` (also `RwShared`/`Atomic`/`AtomicInt`/`Executor`, aliased imports
  too). `chezzi check` rejects them with the message of `defer Shared(0)`. Wrap one:
  `fn log(m: str): print(m)` then `defer log("done")`.
- `time.timer` is a function: `defer time.timer(10)` is legal.
- A module function is a legal target, and a module name is a namespace, not a receiver:
  `defer math.abs(-3)`, `spawn math.abs(-3)`, `defer lib.helper(3)`, `spawn lib.helper(3)`, aliased
  imports and witness-taking generics (`spawn lib.reset(c)`) work (Go: `go pkg.F(x)`). A module is
  not a value: `m := math` is rejected (`module 'math' is not a value`), so `spawn m.abs(-3)` never
  type-checks.
- A static method is a legal target in every call spelling: `defer Holder.build(3)`,
  `defer Gen[int].build(3)`, `defer T.default()` in a generic, `defer lib.Holder.build(3)`, a
  `from`-imported head. Arguments are evaluated at the statement; LIFO order holds. All of these spawn too.
- `defer print(a, b, sep="-")` and `spawn print(...)` take every argument and keyword a direct
  `print` call takes.
- A value bound to one of the four builtins (`f := ord`) can go to a `spawn` and runs in the task.
- A user binding shadows these names in value position (`fn f(ord: int): print(ord)`,
  `for chr in xs:`, a top-level `chr := "…"`). At top level a read above a same-named global's `:=` is
  the builtin (as in CPython); inside a body the name is the global in both source orders.

`defer` inside a `recover:` block runs as the block unwinds, before the boundary binds its value.
Top-level defers run LIFO when the program ends (or while unwinding an unhandled top-level error).
`std.os.exit` halts at once and does not run defers (Go's `os.Exit`).

**Block form `defer:`** groups several cleanup actions (like `spawn`'s two forms).

- The body runs top-to-bottom at scope exit and is LIFO as a unit relative to other `defer`s. It is
  ordinary statements, so builtins are fine.
- It runs in the same task and captures free variables by reference: `x := 1; defer: print(x); x = 99`
  prints `99`, and reassigning an enclosing local mutates it. (The call form evaluates its arguments at
  the `defer` point.)
- A `?` in the block (also `?.` on a `T!E`) is ``'?' is not allowed inside a defer block: a deferred block has no caller to propagate to; handle the error in the block with `match` or `??` ``, whatever the function returns. A `?` in a `recover:`, a nested `fn` or a closure in the block targets that construct. Rust rejects `?` in `Drop::drop`, and a Go deferred func cannot return an error to its caller.
- `return` anywhere in the block (also in an `if`/`for`/`match`/`wait:` arm) is
  `'return' is not allowed inside a defer block: a deferred block has no caller to return to`. A
  `break` or `continue` that would leave the block is `'break' is not allowed inside a defer block: a
  deferred block runs at scope exit, outside the loop`. A `return` in a nested `fn` declared in the
  block is fine.

```chezzi
fn handle(conn: Conn):
    x := 1
    defer:                        # both lines run at scope exit, top-to-bottom
        log("closing")
        conn.close()
    defer:
        log("x = {x}")            # prints "x = 2" — captured by reference, read at exit
    x = 2
```

### Handling a `T?` / `T!E` value: `?`, `??`, `?.`, `else`, `match`

| Form | Does | Leaves the fn? |
|---|---|---|
| `x := f()?` | unwraps; on `None` / an error returns it from THIS fn | on failure |
| `x := f() ?? d` | unwraps; on `None` / an error takes `d` (the error is discarded) | never |
| `f()?.name` | applies `.name` to the value; `None` stays `None`, an error propagates | on an error |
| `x := f() else e: <block>` | unwraps; on failure runs the block, which must leave | on failure |
| `match f(): ?v: … !e: …` | handles every outcome by pattern, and may produce a value | no |

**`else` = handle and leave.** `v := f() else e: <block>` binds `v` on success. On failure the block
runs with the error bound to `e` (`else _:` binds nothing, like `else:`) and MUST leave: `return`
(including `return !e`), `break`, `continue`, `panic(...)`, or an `if` / `match` whose every branch
leaves. `os.exit(n)` is an ordinary call to the checker (Go's `os.Exit`): write `panic("unreachable")`
after it. The block never produces a value; use `??` for a default and `match` for a computed value.

```chezzi
fn load(path: str) -> Config!str:
    text := read(path) else e:
        log("cannot read {path}: {e}")
        return !e
    port := text.to_int() else:        # `else:` ignores the error (the only form for a `T?`)
        return !"not a number"
    save(path) else e:                 # a `None!E` call: the statement form (or `_ := … else e:`)
        return !e
    return Config(port)
```

A destructuring binding takes the guard too; the error is the call's own. For a `T?` the form is `else:`.

```chezzi
fn span(path: str) -> int!str:
    lo, hi := bounds(path) else e:     # bounds() -> (int, int)!str
        return !e
    a, b := find(path) else:           # find() -> (int, int)?
        return 0
    return hi - lo + a + b
```

- A block that can fall through is `else block must leave (return, break, continue, panic)`.
- A guard may stand in any block, a `spawn:`, `defer:` or `recover:` block too. The statement that
  leaves the guard's block is judged as it would be outside a guard: in a `spawn:` block a
  `continue` to a loop inside the task and a `panic` are fine, and a `return` is `'return' is not
  allowed inside a spawn block: …`.
- The guard exists on a `:=` or typed binding and on a bare call. An assignment (`x = f() else e:`,
  `a, b = …`, `self.f = …`, `xs[i] = …`, `x += …`) and `return f() else e:` are parse errors:
  ``an `else` guard belongs on a `:=` binding or a bare call, not on an assignment or `return` ``.
  A value list (`a, b := 1, 2`) takes no guard.
- On the line of an inline `if` or `elif` body, a bare `else:` or `elif` after the body's statement
  belongs to the `if` (`if c: f() else: g()` is the if-expression). `else e:` stays a guard there:
  `if c: save(p) else e: return`. A bare-`else:` guard on that statement needs the indented form:
  `if c: f() else: return` is `` a one-line `if`/`else` takes an expression in each branch; … ``.
  The same error applies to `if c: r := recover: f() else: panic("p")` (an inline `recover:` takes one
  expression). A one-line `elif` takes no `else:` or `elif` after its body:
  ``an `elif` body ends its line; write the next `elif` or `else:` on its own line, and an `else:` guard in an indented body``.
- `else e:` on a `T?` is an error: `` `else e:` needs an error to bind, and int? has none; write `else:` ``.
- On an `int?!E` value `else` handles the outer error, and `v` is the `int?`.
- At module top level there is no `return`: leave with `panic`, `break` / `continue` in a
  loop, or use `?`. `os.exit` ends the program but does not count as leaving for the checker. Inside a nested fn or a generator `return` leaves that fn.
- A `spawn:` block and a `defer:` block reject it, as they reject `?`.

**How a carrier prints** (`print`, interpolation, a container element, an `assert` message, a
fault text): a present or success value prints as its payload (`5`), an absent one as `None`, an
error as `!` and the error (`!boom`; `!'boom'` inside a list), a `None!E` success as `None`. A `?`
prints only before an absent payload, so a present `None` of an `int??` prints `?None`. The text does
not show the wrap depth or carrier kind (`5` at `int?` and `int??`); test those with `match`.
