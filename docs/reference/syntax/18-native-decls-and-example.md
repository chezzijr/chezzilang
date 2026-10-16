## 12c. `native fn` / `native ctor` — universe-builtin signatures in Chezzi (prelude/std-only)

The internal analog of `extern "lib":`. A `native` decl declares the **signature** of a built-in whose
body the engine implements, bound by name. The universe builtins are declared this way in
`std/prelude.chz`, which is linked into every program.

```chezzi
native fn ord(c: str) -> int      # first-class universe FUNCTION
native fn panic(msg: str)         # no `-> ret` → native-controlled/never
native ctor int(x) -> int         # non-first-class scalar CONSTRUCTOR; `x` unannotated → dynamic
native ctor bytearray(x) -> bytearray
```

- **`native fn`** declares a first-class function (`f := ord` works). **`native ctor`** declares a
  constructor that is not a value (`f := int` is a type error, as `f := List` is).
- Bodyless and newline-terminated, like an `extern` signature. It compiles to no code.
- Within `native` decls only: an unannotated param accepts anything, and no `-> ret` means
  native-controlled/never (how `panic` diverges). User code has no `any`/`never`.
- Std-only: in a user `.chz` it is *native fn/ctor declarations are only allowed in standard-library
  modules*. Top-level only.

The universe builtins `print`, `ord`, `chr`, `panic` (fns) and `int`, `float`, `str`, `bytes`,
`bytearray` (ctors) are declared in `std/prelude.chz`. `print` is
`native fn print(...args: Any, sep: str = " ", end: str = "\n") -> None` (the decl is the checker's
signature; the call still uses dedicated opcodes). The `List`/`Map`/`Set` ctors stay built in.

## 12d. `native struct` — native-type signatures in Chezzi (prelude/std-only)

A body-less `native struct Name:` declares a native type's checker signature (its fields) in Chezzi;
the runtime layout and method dispatch stay native. Std-only and top-level only.

```chezzi
# std/regex.chz — a file-backed, import-gated native module
native struct Match:              # regex.Match's SIGNATURE (fields-only)
    text: str
    start: int
    end: int
    groups: List[str]

native fn find(pat: str, s: str) -> Match?!   # a native MODULE MEMBER
```

- **Methods.** A bodyless `native fn` in the body is an instance method and must take `self` first
  (`native fn read(self, n: int) -> str!`), else `native instance method must declare 'self' as its first
  parameter`. A module-level `native fn` may not take `self`. This is how `std.net`'s
  `Socket`/`Listener` declare `read`/`write`/`accept`/`close`.
- A plain `fn` method with a body is allowed and type-checked, so a native struct can mix native
  methods with Chezzi methods (`std.io`'s `Reader.lines()`, a generator over `read_line()`). A std
  module's top level may mix bodyless `native fn`s and bodied `fn`s too (`std.math`'s `divmod` beside
  native `gcd`/`lcm`), and may `import` other modules.
- A `test` method or a field `= default` in the body is a parse error. A `native enum` rejects a bodied
  method (`native enum methods are not supported`).
- **Generic native structs** (`native struct Shared[T]:`) may use their type params in method sigs
  (`native fn get(self) -> T`); each call site substitutes the element type (`Shared[int].set` expects
  `int`). `std.concurrency` declares `Shared[T]`/`RwShared[T]`/`Atomic[T]` and `Executor` this way. A
  method whose type a flat decl cannot express (`RwShared.read`, `submit`) is declared with an
  unannotated closure param and re-typed by the checker.
- **A `native struct` adds methods to an existing reserved type**, never a new nominal type. The
  prelude declares `native struct List[T]` / `Map[K: Hashable, V]` / `Set[T: Hashable]`, whose method
  sigs land on the built-in containers; literals and the turbofish ctor (`List[int]()`) stay built in.
  `map`/`filter`/`fold`/`sort_by`/`sort_by_key` are file-backed generics
  (`native fn map[U](self, f: fn(T) -> U) -> List[U]`); the solver recovers a return-position type param
  from a closure argument's body, for user methods too (`Box(3).apply(fn(x): x + 1)` on
  `fn apply[U](self, f: fn(T) -> U) -> U` gives `U = int`). `sort` is
  `native fn sort(self) -> None where T: Comparable`; `sum` is `native fn sum(self) -> T where T: Add`
  plus a numeric check.
- Std-only: in a user `.chz` it is *native struct declarations are only allowed in standard-library
  modules*; nesting is a parse error.
- **Any file-backed std module** (`std/regex.chz`, `std/math.chz`, `std/io.chz`, `std/os.chz`,
  `std/rand.chz`, `std/fs.chz`, `std/net.chz`, `std/concurrency.chz`) declares its native types and
  functions in-module. The checker reads them as the module's signatures; runtime values stay bound by
  name. `import std.regex` gives `regex.Match` and `regex.find(...)`; `import Match from std.regex` binds
  `Match`. Metadata a decl cannot express (`math.abs`/`sign` numeric polymorphism, hover docs) is added
  by the checker.

## 12e. `native enum` — reserved builtin-enum variant shape in Chezzi (prelude/std-only)

A body-less `native enum Name[T…]:` declares a reserved builtin enum's variant shape. Std-only,
top-level only, and it maps onto an existing reserved type. The only native enums are the two carrier
enums in `std/prelude.chz`:

```chezzi
native enum Carrier[T]:    # the shape of a declaration; the prelude's two are the enums
    Present(T)             # behind `T?` (a value / None) and `T!E` (a value / an error)
    Absent
```

- The body is its variants (ordinary `enum` variant grammar), then optional bodyless `native fn`
  methods with `self` first (variants come first; a self-less or plain `fn` method is a parse error).
  The carrier enums have no methods. Type params work as in an ordinary enum.
- Only the shape is file-backed. `?`, exhaustive `match`, top-level error propagation and
  `?x`/`!e`/`None` construction are built into the compiler, and the checker verifies the decl matches
  its built-in shape. The error carrier is declared with two slots; `T!` → `E = Error` is applied by
  the type resolver.
- Std-only: in a user `.chz` it is *native enum declarations are only allowed in standard-library
  modules*; nesting is a parse error.

## 13. Standard library (v1)

The library reference (every builtin, type method, runtime type and `std.*` module) is
[`stdlib.md`](../stdlib/01-builtins.md).

- Always available: `print`, `Range`, `int()`/`float()`/`str()`, `ord(s)→int` (a 1-character `s`;
  longer or empty faults), `chr(n)→str`, `Set()`/`Set(list)`, `panic(msg)`, and the core-type methods.
- Modules: `import std.X`, then `X.func(...)`.
- `Match` (`std.regex`), `Response` (`std.request`) and `ProcResult` (`std.process`) belong to their
  modules. Reading fields of a returned value needs no import; naming or constructing the type does. A
  user `struct Response` is fine without `import std.request`; importing the type and declaring a
  same-named struct is `type 'Response' is already defined` (for every import-gated std struct).

---

## Full example

```chezzi
import sqrt from std.math

struct Point:
    x: int
    y: int

    fn dist(self) -> float:
        return sqrt(float(self.x*self.x + self.y*self.y))

fn safe_div(a: int, b: int) -> int!:
    if b == 0:
        return !"divide by zero"
    return ?(a / b)

fn main():
    p := Point(3, 4)
    print("dist: {p.dist()}")

    total := 0
    for i in 0..10:
        if i % 2 == 0:
            total += i
    print("even sum: {total}")

    match safe_div(10, 2):
        ?v:  print("div: {v}")
        !e: print("err: {e}")

main()   # no automatic entry point — call it yourself
```
