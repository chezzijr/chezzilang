## 5. Functions

```chezzi
fn add(a: int, b: int) -> int:     # param types REQUIRED; '-> T' optional
    return a + b

fn double(x: int): x * 2           # inline expression body, no '-> T' → the expression's type (int)

fn log(msg: str):                  # block body, no '-> T' → returns nothing ('None')
    print(msg)

# closures / anonymous functions — body after ':'
twice := fn(x: int) -> int: x * 2
nums.map(fn(x): x * 2)             # param/return types inferred in closures
```

**Closure-parameter inference.** An unannotated closure parameter gets a concrete type from its
context (never dynamic). Sources, in priority order:

1. The **expected type** of the slot: a call argument for a `fn(...)` parameter
   (`s.update(fn(x): x + 1)` → `x: T` of `Shared[T]`; `[1,2,3].map(fn(x): x + 1)` → `x: int`;
   `Mapped(it, fn(x): x * 2)` → `x` from the field type), an assignment/`:=` to a `fn`-typed target,
   a struct `fn`-field, or a `fn`-typed return position.
2. A `match` whose scrutinee is the bare param (`fn(x): match x: E.A: …; E.B: …` → `x: E`).
3. A member access owned by exactly one type: a method only `str`/`bytes` has
   (`fn(x): x.upper()` → `x: str`) or a field/method one struct declares (`fn(x): x.f`).

Arithmetic, comparison, indexing and a member shared by several types (`x.len()`) do not pin; they
are checked against a type a higher source resolved. Call sites are then type-checked like a named
function. A parameter nothing resolves (`g := fn(x): x + 1`) is
`cannot infer type of parameter 'x'; add a type annotation`. A closure passed to a generic slot whose
type parameter only it would pin (`store(fn(a): a + 1)` for `fn store[T](x: T) -> T`) must be annotated too.

**Return-only type params recover from a closure/fn body.** A generic HOF whose type parameter
appears only in its return (`fn applyone[U](x: int, f: fn(int) -> U) -> U`,
`fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]`) recovers `U` from the body of the closure/fn
passed for `f`, as `.map`/`.fold` do. `applyone(5, fn(x): x * 2)` yields `int`;
`mymap([1,2,3], fn(x): "{x}")` yields `List[str]`. The body may be an expression, a `.method(…)`
call or a nested generic call (`fn(x): ident(x)`). When a sibling argument or a slot already pins `U`
(`fn f[U](init: U, g: fn(int) -> U) -> U` with `init = 0`), the closure's return is checked against
it (`fn(x): "{x}"` is a type error). `fn make[U]() -> U` stays un-inferable.

**An annotation pins a generic call.** A let's declared type, a declared return type and a parameter
type each pin the called generic's type parameters, which then fix dependent closure params:

- `h: Heap[int] = Heap([], fn(x, y): x < y)` checks (`T=int` gives `x, y: int`). The same works for
  `fn mk() -> Heap[int]: return Heap([], fn(x, y): x < y)` and `take(Heap([], fn(x, y): x < y))`
  with `take(h: Heap[int])`.
- The annotation fills only type params the arguments leave free. A turbofish or a concrete argument
  wins; a conflict is the usual mismatch error.
- It reaches every branch of an `if`/`match` value
  (`h: Heap[int] = if rev: Heap([], fn(x, y): x > y) else: Heap([], fn(x, y): x < y)`) and both
  operands of a binary operator (`xs: List[G[int]] = mkl() + mkl()`). So `c: List[Any] = [1] + ["a"]`
  prints `[1, 'a']`, as CPython does.
- Gap: it does not reach a generic ctor inside a container literal
  (`a: List[Heap[int]] = [Heap([], fn(x, y): x < y)]`). Annotate the closure params or use a turbofish.

**A parameterized protocol bound infers its type args from the type that satisfies it.** In
`fn produce_as[R, T: Produces[R]](x: T) -> R`, `produce_as(IntProducer())` binds `T` from the
argument and recovers `R` from `IntProducer.produce`'s return type:

```
protocol Produces[R]:
    fn produce(self) -> R

struct IntProducer:
    fn produce(self) -> int:
        return 7

fn produce_as[R, T: Produces[R]](x: T) -> R:
    return x.produce()

x := produce_as(IntProducer())   # R = int
```

- Conformance is structural, so the witnessing method is unique. Every protocol param is recovered
  independently (`Pair[A, B]` pins both), on a generic method too.
- Precedence: turbofish > arguments > this recovery > a result annotation. A recovered type that
  disagrees with an argument is discarded, and the mismatch is reported.
- The witness may be a **builtin** (`take([1, 2, 3])` against `Popper[R]` infers `R = int?`), a
  **protocol existential** (a value annotated `Produces[int]` gives `R = int`), or come from an
  **embedded** protocol (`protocol Q[S]` with the embed line `Produces[S]`, used as `[T: Q[S]]`,
  recovers `S`).
- The one case it cannot reach: no requirement method mentions the param
  (`protocol Tagged[R]: fn tag(self) -> str`). The error is `cannot infer type parameter R for
  'produce_as'; add a result annotation or explicit type arguments`, and both escapes work.

```
struct W:
    fn take[R, T: Popper[R]](self, x: T) -> R:
        return x.pop()

v := W().take([1, 2, 3])        # 3 — inferred
v2: int? = W().take([1, 2, 3])  # 3 — or pinned by the annotation
```

A method that cannot conform for any instantiation (wrong arity, wrong parameter type, missing) keeps
its own conformance message. A generic witness method recovers nothing.

**A bound's type arguments mean what they mean at the declaration.** A method's `[U: Conv[T]]` inside
`struct Box[T]` names the receiver's `T`: `Box(v="s").pick(i)` needs `Conv[str]`. An abstract argument
satisfies `P[X]` only through its own bound `P[X]` for the same `X` (as in Rust and Go): inside
`fn go[X, Y: Conv[str]](y: Y, x: X)`, the call `need(y, x)` to `fn need[S, B: Conv[S]](u: B, s: S)` is
`type Y does not satisfy Conv[X]`. An embed line resolves in its own protocol, so `protocol Bag[T]:`
with the embed line `Contains[List[T]]` is accepted.

**A return-only type parameter must be pinnable** (Rust's `E0282`):

```
fn empty[T]() -> List[T]:
    return []

xs := empty()              # cannot infer type parameter T for 'empty';
                           # add a result annotation or explicit type arguments
xs: List[int] = empty()    # ok
xs := empty[int]()         # ok
```

Any sink on the same statement counts: an argument slot (`takes(empty())`), a declared return
(`return empty()`), a `match` arm, a struct field at construction. A parameter that also appears in a
value parameter gives the open-binding error instead (`x := tag([])`, see "Inference" in §10). A
parameter default is checked where it is spliced. A later statement never supplies the type:
`xs := empty()` then `xs.push(1)` is still the error.

### Bodies and return types

**An inline expression body returns its value.** A named function whose body is on the same line
after `:` and is a bare expression returns that value, like `fn(x): expr`. This is the only implicit return:

```chezzi
fn ten(): 10               # inferred '-> int'; ten() == 10
fn dbl(x: int): x * 2      # usable as a value / .map argument: [1,2,3].map(dbl) == [2, 4, 6]
fn answer() -> int: 42     # annotated inline-expr body is valid (the expr is the implicit return)
```

An inline body may be an **if-expression** (the implicit return; `else` is required). On its own line,
and after any `:` (`for i in xs: if c: a() else: b()`, a statement `match` arm), `if c: a() else: b()`
is that if-expression used as a statement. An inline `fn` body may also be a multi-line `match` or a
`recover:` block (`fn g() -> int: match n:` + arms). Other compound statements need an indented block.

```chezzi
fn pick(n: int) -> int: if n > 0: 1 else: 2     # pick(5) == 1, pick(-5) == 2
```

An inline body may be a **closure literal**: `fn (` opens a closure, `fn name` a declaration.

```chezzi
fn adder(n: int) -> fn(int) -> int: fn(x: int) -> int: x + n    # adder(1)(2) == 3
```

**An inline body is exactly one simple statement**, after every `:` that opens a block (a fn, a
method, `if`, `for`, `while`, a `match` arm, `parallel:`, an `else` guard).

- Simple statements: a let, an assignment, `return`, `yield`, `break`, `continue`, `pass`, `assert`,
  an expression (a closure literal and the one-line `if … else` included), `spawn f()`. So
  `if c: spawn f()` is legal.
- `for`, `while`, `fn name`, `parallel:`, `wait:`, `spawn:` and both `defer` forms need an indented
  line. Inline, each is `a nested block must be indented, not written inline after ':'`.
- An inline `if` is the if-expression and needs its `else`. An inline `match` is a block expression,
  which is no whole value after the `:` of an `if` or `for`.

Only a bare-expression inline body returns implicitly. `fn a(): x = 5` returns `None`;
`fn a(): return 10` is an explicit return. `fn a(): foo()` returns `foo()`'s value (`None` if `foo` is
void). An annotated inline body is checked like `return <expr>`: `fn a() -> int: "x"` is a type error,
and a non-None expr against `-> None` (`fn a() -> None: 10`) is *"function returns nothing, cannot
return a value"* (a void call against `-> None` is legal).

**A multiline body never returns implicitly**, even with one statement: `fn a():\n    10` evaluates
`10` and returns `None`. Use `return`.

**Return types.** A named function's return type is written, never inferred from `return` statements.
Param types are required. With no `-> T` the body shape decides:

| Body | No `-> T` means | Example |
|---|---|---|
| **block** (statements) | returns nothing (`None`) | `fn log(m: str):` + `print(m)` |
| **inline expression** | the type of that one expression, as for a lambda | `fn ten(): 10` is `-> int` |

- A block body with no `-> T` that returns a value is an error at the returned expression:
  `'f' declares no return type, so it returns nothing; add '-> int' to 'f'`
  (`add a '->' return type to 'f'` when the value's type is not fully known).
- An inline body is typed only if every call in it has a type. A callee declared below is fine
  (`fn c(): d() + 1` above `fn d(): 2`). A recursive or mutually recursive inline function must write
  its type:
  `cannot infer return type of 'fact': a call in its body has no known type yet (return types that depend on each other are not inferred); add a -> annotation`.
  A function that only calls such a function reports the same text.
- An inline body with an open slot (`fn f(): []`) is `cannot infer return type of 'f'; add a -> annotation`.
- A function whose sole body is a diverging call (`fn boom(): panic("msg")`) is
  `None` (named function or lambda). `fn b() -> int: panic(...)` is valid: bottom fits any return.
- A generator writes `-> Iterator[T]` (§6).
- An inline body that forwards a call has that call's declared type: `fn f(): g()` with
  `g() -> int!str` is `-> int!str`.

**A `!e` value takes its type from an annotation or an expected type**, never from sibling `return`s
or branches. `fn f(c: bool) -> int!str:` returning `1` and `!"bad"`, `x: int!str = if c: 1 else: !"bad"`
and `take(!"bad")` are typed by the written type. With neither, the `!e` reports once:
``a `!` value needs its type from an annotation: add `-> T!E` to the function, or annotate the binding, e.g. `w: int!str = !e` ``.
A later operand of the same statement does not type it: a `T!E` sibling (`[(if c: 1 else: !"bad"), r]`),
a generic bound (`Bx[int!str].from2([(if c: 1 else: !"bad")])`) and a compare
(`(if c: 1 else: !"bad") == r`) report the same error, because nothing wraps the `1`.

Methods follow the same rules. A method's return type reaches call sites and protocol satisfaction (a
`compare(self, o)` returning `bool` fails `Comparable`, which requires `-> int`).

**Returns on every path.** A multiline function with a declared non-void return type must return a
value on every path:

```chezzi
fn a() -> int:             # ERROR: 'a' can fall off the end without returning a value
    10                     #   (a multiline 1-stmt body does NOT implicitly return)
fn a() -> int:
    return 10              # fix: an explicit `return`
fn a() -> int: 10          # OK: an inline-expr body implicitly returns its expression
```

These terminate:
- an `if`/`else`, exhaustive `match` or `wait:` whose every branch (and `else`) returns
- a `return` inside `parallel:`
- `while true:` with no `break` of its own (a `break` in an `if`/`match`/`wait:` arm or a `parallel:` body counts)
- a tail call to `panic`. Any other call does not terminate, however its callee ends: `os.exit(n)`
  and a `die()` helper that only panics are ordinary calls (Go has no never type). Write
  `panic("unreachable")` after such a call; the error says so when the last statement is a call.
  A user fn, method, local or parameter named `panic` does not count.

A `for` body may run zero times, so a `return` in it does not terminate. Inline-expr bodies are exempt.

**`None` is the one "nothing" word.** As a type it means "returns nothing" (a return annotation only;
`None?` is not a type). As a value it means absent, legal only where a `T?` is expected. `nil` is an
ordinary identifier. A void result (`print(...)`, `list.push(...)`, `list.sort()`) cannot be a value:
`x := print("hi")`, `print(print("hi"))`, `[print("hi")]` and `1 + print("hi")` are *"expression
returns no value (None) and cannot be used as a value"*. A void call as a statement is legal.

### Default and named arguments

A free function, a struct constructor or a method may give trailing parameters a **default**: any
expression that does not reference another parameter (`= 10`, `= 1 + 2`, `= GLOBAL * 2`, `= compute()`).

- **Scope (as in Python, Ruby, Kotlin).** A default sees the definer's module globals, functions and
  imports. `g.chz` with `K := 7` and `fn f(x: int = K)`, imported by a `main.chz` with its own
  `K := 99`, prints **`7`**.
- **Timing (unlike Python).** A default runs on every omitting call: `f(n: int = bump())` returns `1`,
  `2`, `3` on three calls, and `f(xs: List[int] = [])` gives each call a fresh list. An explicit
  argument never runs the default.
- **Module scope.** A default runs in module scope, where no parameter is bound. `k := 1;
  fn f(k: int = k) -> int: return k` returns `1` from `f()`, as in CPython. `fn f(a: int, b: int = a)`
  is *"unknown name 'a'"*. In an interpolated fragment the same holds: with `n := 100`,
  `fn f(n: int, x: str = "n={n}")` gives `n=100` for `f(3)` and `g := f; g(3)`. A binder inside the
  default is its own (`g: fn(int) -> int = fn(x: int) -> int: x * 2` beside a parameter `x`,
  `xs: List[int] = [n * 2 for n in 0..3]` beside a parameter `n`).
- **Nested fn.** Its defaults apply at a direct call and through an alias (`g := f; g()`), in module
  scope. A default that reads the enclosing fn's param, a local, a sibling nested fn or a local that
  shadows a global is *"a nested fn's default cannot read the enclosing fn's locals: 'n' is local here,
  and a default is evaluated in module scope (pass it as an argument, or read a module-level
  binding)"*. A lambda cannot declare a default.
- **Struct field and method defaults** run in module scope too. A read of a name bound in the type body
  (an earlier field with a default, or a method) is declined: with `a := 100`,
  `struct S: a: int = 1; b: int = a` is *"default value cannot read 'a': 'a' is bound in the body of 'S'
  where the default is written (an earlier field default or method), but a default runs in module
  scope; pass it explicitly or read a module-level binding"*. The field that owns the default is not
  its own binder: `k := 1; struct S: k: int = k` reads module `k`. `Self` in a field default is an unknown name.

Callers may pass arguments **by name**:

```chezzi
fn greet(name: str, greeting: str = "Hello", punct: str = "!") -> str:
    return greeting + ", " + name + punct

greet("Ada")                       # "Hello, Ada!"  — both defaults
greet("Ada", "Hi")                 # "Hi, Ada!"     — override one positionally
greet("Ada", punct="?")            # "Hello, Ada?"  — name an arg, skip a middle default
greet(punct=".", name="Bo", greeting="Hey")        # all named, any order

struct Server:
    host: str
    port: int = 8080               # default field
    tls: bool = false

Server("localhost")                # port=8080, tls=false
Server("db", port=9000)            # named field; tls defaults
```

Rules:
- A parameter with a default may not be followed by a required one.
- Positional arguments precede named ones (`f(y=2, 1)` is an error). Each parameter is supplied at most once.
- Every filled slot evaluates in **parameter-declaration order**, positional, named or default:
  `fn f(x = p("d-x"), y = p("d-y"), z = p("d-z"))` called as `f(z=p("arg-z"), x=p("arg-x"))` prints
  `arg-x`, `d-y`, `arg-z`.
- Named and default arguments work on free functions (own module, `from`-imported, `mod.f(...)`),
  struct constructors and methods (`p.greet(punct="?")`). They bind against the declaration the
  checker resolved: any struct or enum receiver of known type, and a protocol or bound type-parameter
  receiver through its implementors' shared parameter shape (if implementors differ, pass positionally).
- A call through a function value uses the same binder and messages (`unknown named argument 'x'`,
  `missing required argument 'a'`, ...).
- A native callee (a `native fn`, a native method, a native static method) binds like a user fn,
  by the parameter names of its decl in `std/*.chz`: `s.insert(v=3)`, `chr(n=65)`,
  `Set.from_iter(xs=[1])`, `sock.read(10, timeout_ms=5)`. An omitted default is filled from the decl,
  at the call site. `s.insert(x=3)` is `unknown named argument 'x' (its parameters are: v)`. A builtin
  with no decl (`Set[int](..)`, `Channel[T](..)`) and an `extern` take no named arguments:
  `'Set' takes no named arguments; pass its arguments positionally`. `print` takes `sep=` (default
  `" "`) and `end=` (default `"\n"`), both `str` (see [`stdlib.md`](../stdlib/01-builtins.md)):
  `print("a", end="")` writes `a` with no newline; `print("a","b", sep="-", end="!")` writes `a-b!`.
  Its decl is `native fn print(...args: Any, sep: str = " ", end: str = "\n") -> None`.
- A static method (no `self`) takes defaults; `S.mk()` fills them.
- Closures and enum variant constructors take no defaults (enum methods do).

**Where a default is compiled.** A non-literal default is compiled once, as a hidden zero-arg function
in the declaring module; an omitting call calls it. A self-contained literal (`= 1`, `= -1`, `= 1 + 2`,
`= None`, `= []`, a brace-free string) is copied inline. Consequences:

1. **`?` cannot propagate out of a default.** `fn f(x: int = getr()?.len()) -> int` is *"a default
   expression cannot propagate with `?` — defaults are evaluated in their defining module, which has
   no caller to propagate to; use `??` or produce a `T?` value"*. A `?` inside its own scope works:
   `fn f(x: int!str = ?(getr()?.len()))` returns `4` on the `?v` path and the `!e` on the other.
   `T?`-mode `?.` and `??` never propagate (`x: int? = geto()?.len()`, `x: int = geto() ?? 0`), and
   `??` on a `T!E` discards the error, so `getr() ?? 0` is legal.
2. **A default resolves in the declaring module, even where the caller cannot name it.** A method
   default in module `a` can be reached from a module `z` that never imports `a` (for example through
   a protocol-typed parameter). The call holds a reference to the definer's provider, so the answer is
   the definer's, as in CPython. An explicit argument reads the caller's scope.
3. **A default that needs itself is a compile error:** `fn f(x: int = f())` is *"the default for 'x'
   of 'f' is cyclic: evaluating it requires evaluating the default for 'x' of 'f' again"*. Mutual and
   indirect provider cycles are caught too.
4. **`Self` and type parameters.** On a non-generic host, `Self` in a parameter default's type is
   replaced by the concrete type (`fn combine(self, other: Self = mkq())`, `List[Self]`). A parameter
   whose type mentions a type parameter (or `Self` on a generic host) is filled by the callee's
   prologue in the declaring module. A struct **field** in that shape gets a provider generic in the
   struct's type parameters, filled by forwarding the call's turbofish:
   `struct Holder[T]: n: int, items: List[G[T]] = mkl()` runs as `Holder[int](1)`, but `Holder(1)`
   (no turbofish) or a struct whose type parameter has a bound keeps the field required. A keyword
   call that supplies a later parameter while omitting an earlier callee-filled one
   (`G(1).m(k=3)` over `fn m(self, xs: List[Self] = mkl(), k: int = 9)`) is a compile error naming the
   parameter to pass.
5. **A cycle through an ordinary function is a runtime fault.** `struct S: n: int = mk().n` with
   `fn mk() -> S: return S()` checks and then faults with
   `maximum call depth (10000) exceeded (infinite recursion?)`, rc 1 (CPython: `RecursionError`).
6. **A protocol-typed or protocol-bounded receiver binds against the PROTOCOL's parameter list** and
   never borrows an implementor's default. With `protocol P: fn f(self, a: int, b: int)` and
   `struct A: fn f(self, a: int, b: int = 10)`, `x: P = A(100)` rejects `x.f(1)` with
   `'f' expects 2 argument(s), got 1`. An `A`-typed receiver takes the default. Go and Rust agree.
   Named arguments bind by the protocol's names (see "Protocol parameter names" in §7b).

A default may be a **variadic call** (`fn f(a: int = sum_all(1, 2), ...xs: int, tail: int =
sum_all(3, 4))`), before the variadic and in the keyword-only tail, as in CPython.

### Variadic parameters (`...name: T`)

A parameter `...xs: T` collects the surplus trailing positional arguments into a fresh `List[T]`:

```chezzi
fn sum_all(...xs: int) -> int:
    total := 0
    for x in xs:                 # `xs` is a `List[int]`
        total = total + x
    return total

sum_all(1, 2, 3)                 # 6
sum_all()                        # 0  — zero args → empty list
```

- At most one variadic per signature. It needs an element type (`...xs: T`) and takes no default.
- Every parameter after it is keyword-only: with a default it is optional, without one it is a
  required keyword argument.

```chezzi
fn labeled(prefix: str, ...xs: int, sep: str = ", ") -> str: ...
labeled("nums: ", 1, 2, 3)             # sep defaults to ", "
labeled("dash: ", 4, 5, 6, sep="-")    # keyword-only `sep`
```

- Allowed on free functions, methods and `native fn` decls; not on closures or `extern` (C)
  functions (see [`ffi-and-packaging.md`](../../design/ffi-and-packaging.md) §5).
- A variadic is part of the fn type: `fn labeled` above has the type `fn(str, ...int) -> str`, and
  `fn(str, List[int]) -> str` is a different type (Go's `func(string, ...int) string` rule).
- Every value path packs surplus positionals like a direct call: a binding, a list slot, a
  parameter, a HOF argument, `defer` and `spawn` (`fs := [sum_all]; fs[0](1, 2, 3)`).
- A keyword-only param is part of the fn type, and the type prints it: `fn g(...xs: int, sep: str)`
  has the type `fn(...int, str) -> str`. A slot may hold a value that takes more keyword-only params
  only when the extra ones have defaults, as for positional defaults: `print` fits
  `fn(...Any) -> None`, `[g, d]` over `fn d(...xs: int, sep: str = "-")` checks, and `[g, k]` or
  `[k, g]` over `fn k(...xs: int)` reports `list elements differ` in either order.
- A call through a value that is not a certain binding (next chapter) omits the keyword-only params,
  and the callee fills their defaults. A required one makes such a call an error:
  `missing required keyword argument 'k'`.

### `Any` (the top type)

`Any` is an empty structural protocol: every type satisfies it, scalars and `None` included. It is the
element type of a universal slot such as `print(...args: Any)`, and can annotate any binding or
parameter (`x: Any = 42`, `fn log(v: Any) -> None`). An `Any` value has no methods: you can pass and
display it, not call methods on it (a downcast `cast[T]` is future work, see
[`roadmap.md`](../../design/roadmap.md)). The prelude defines it as `protocol Any:` with a lone `pass`
body, and any user empty protocol behaves the same (see [`pass`](06-control-flow.md#the-pass-keyword)).
A module that declares its own `Any` shadows it.
