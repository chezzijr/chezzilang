## 12. Imports & modules

```chezzi
import std.io                    # use as  io.read(...)
import std.io as fs              # module alias → fs.read(...)
import read, write from std.io   # pull names in (no braces)
import read as r from std.io     # named + alias
import core.db.pool              # local module → <root>/core/db/pool.chz
pool.get()                       # …by its last segment,
core.db.pool.get()               # …or by its full path (un-aliased import only)
import math from std             # a std module by name, like `import std.math` (Python `from os import path`)
```

### Variant import

`import V from Enum` makes an enum variant a bare name in the importing module (Rust's
`use Enum::Variant`). The enum is one this file declares, or a name a from-import of this file binds.

```chezzi
enum Color:
    Red
    Green
import Red, Green from Color     # variants become bare names here
print(Red, Green)                # Red Green
match Color.Red:
    Red: print("r")              # a bare imported variant is a pattern head too
    Green: print("g")
import Green as G from Color     # an alias binds `G` to `Color.Green`
import Red from Color            # error: 'Red' is already imported (one bind per name)
```

- **Not imported, not bare.** `print(Green)` without the import is `'Green' is a variant of enum
  'Color'; write it qualified as 'Color.Green'`.
- **A module wins.** `import Point from geo` is a module import whenever a module `geo` resolves; it
  is a variant import only when none does. `import V from a.b` (a dotted path) is always a module.
- **Collisions.** A variant import and a same-module `fn`, `struct`, `enum`, alias or protocol of the
  same name, or a second import of the name, is `'V' is already imported` (Rust: E0255 / E0252).
- **Shadowing.** A local binding of the same name shadows the variant. A top-level `Red := …` is
  `'Red' is already imported` in either order (Rust E0530).
- **`T?` and `T!E` are prelude enums** whose names the prelude keeps to itself. User code writes the
  types `T?` and `T!E`, the values `x`, `?x`, `!e` and `None`, and the patterns `?v`, `!e` and `None`.
  The enums cannot be named, qualified or type-applied; an alias of one (`type F = int?`) is a type
  spelling, never a variant head.

### Resolution and placement

**Resolution:** walk up from the file for `chezzi.toml`; if found, that directory is the project root,
else the script's own directory is. `std.*` is reserved for the stdlib. `a.b.c` → `<root>/a/b/c.chz`.
No `./` relative imports.

**`import` is top-level only.** An `import` inside a function body or a nested block is a parse error
(`import must be a top-level declaration`), like every top-level-only declaration (§3).

**The named-import form is `import X from M`**, not Python's `from M import X`: every import starts
with `import` (`from` at statement start is *unexpected 'from' in expression*). Semantics are Python's,
with one Go-style exception: importing the same module or name twice in one file is
`'math' is already imported`.

- `import std.math` plus `import math from std` is accepted: both name one module, bound once and
  initialized once. `import math from std` twice is the duplicate error.
- A std protocol is a member of its std module: `import Add from std.ops`, `import Eq, Hashable from
  std.cmp`, `import Iterator from std.iter` (the table is in [Protocols](09-protocols.md)). A bound
  `[T: Add]` (on a fn, method, struct, enum or protocol, or in a `where` clause) or an embed of `Add`
  without the import is `unknown name 'Add'`. `Any` and `Error` need none.
- A same-module top-level `fn`, `struct`, `enum`, `protocol` or `type` alias may not take a whole-module
  import's bound name (`import lib as f` plus `fn f`, `import lib` plus `struct lib`, `import a.b` plus
  `enum b`), in either order: the same error on the later declaration (Go: `f redeclared in this
  block`). A from-imported type (`import P from lib`) beside `fn P` is not a collision.

### Reserved names

**Only keywords are reserved.** A builtin fn (`print`, `ord`), a builtin type (`int`, `List`), a std
type (`int32`, `Match`) or a std protocol (`Comparable`) is an ordinary name. A module-level
declaration or import of it shadows the builtin in that module (as Go and Python do):

```chezzi
struct List:                    # this module's List; the builtin List is not visible here
    n: int
l: List = List(3)
fn ord(c: str) -> int:          # calls to ord() in this module reach this fn
    return 7
import std.math as int          # `int` is the module in this file: int.abs(-9)
fn f[List](x: List) -> List:    # a type parameter shadows inside its fn
    return x
```

Lookup tries the module's own declarations and imports first, then the builtins. In a module that
declares `struct int`, `int` in a type position is that struct.

**The std protocols** are `Any` and `Error` (global, in `std/prelude.chz`) and these, each a member of
its std module: `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg`, `BitAnd`, `BitOr`, `BitXor`, `Shl`, `Shr`,
`Arithmetic`, `Num`, `Index`, `IndexSet`, `Slice` and `Contains` (`std.ops`); `Eq`, `Comparable` and
`Hashable` (`std.cmp`); `Iterator` and `Iterable` (`std.iter`); `Stringable` (`std.string`);
`PathLike` (`std.path`); `Convert` (`std.convert`).

- Their shapes are plain `protocol` decls in those `std/*.chz` files, the only source. Intrinsic
  conformance (for example `int` satisfies `Add` with no method) and operator binding (`+`→`add`,
  `<`→`compare`, `for`→`Iterator`, `[]`→`Index`, `[a..b]`→`Slice`) are built into the compiler.
- The syntax a protocol backs (an operator, indexing, `for`, `in`, `T.convert(...)`, interpolation, map
  keys, `!` and `recover:`) needs no import. Only a bound or a value type that names the protocol does.
- `Convert[S]` (`fn convert(x: S) -> Self`, a static method, Rust's `From`) is bound-only
  (`[T: Convert[str]]`; a value annotation is rejected) and callable through the bound (§7a).
- `PathLike` (`fn as_path(self) -> bytes`) is the path input of the std filesystem surface:
  `str`/`bytes`/`bytearray` satisfy it and `path.Path` satisfies it structurally, so `fs.exists("x")`,
  `fs.exists(b"x")` and `fs.exists(p)` all work. See [`stdlib.md`](../stdlib/01-builtins.md).
- A new protocol that does not back syntax belongs in its owning module, imported by its consumers.

**A keyword cannot be bound, by any form.** `None`, `true`, `false` and the other keywords are rejected
wherever a name is declared (a `:=` or typed binding, a destructuring target, a parameter, a loop or
comprehension variable, an `else` or `wait`-arm binder, a fn, type, field, variant or generic-parameter
name, an import alias), with one message:

```
None := 5              # error: expected identifier, found reserved keyword 'None' (a keyword cannot be used as a name)
for true in [1, 2]:    # error: expected identifier, found reserved keyword 'true' (a keyword cannot be used as a name)
```

So `None` always means the absent value (or "returns nothing"), and no enum has a variant `None`. A
local, a parameter or a loop variable may shadow a builtin function or type name (`print := 5`,
`fn f(int: int)`, `for len in xs:`); a later call of the shadowed name reports `int is not callable`.
`self` is not a keyword: `self := 5` in a method rebinds it for the rest of the body.

### What an import binds

**`import X from M` is a snapshot** (as in Python): the value is copied at import time. A later write to
the module's global (`M.bump()`) is not visible through the bare name; read `M.COUNT` for the live
value. A container is the same heap object, so mutating through the binding works; rebinding it is
rejected:

```chezzi
import COUNT, LST from lib.st
LST.push(7)     # ok — same heap object as lib.st's LST
COUNT = 99      # error: cannot assign to 'COUNT' imported from module 'lib.st' (a from-imported
                #        global is a snapshot copy — call a mutator fn in that module, or use a
                #        Shared). Writing through the module (`st.COUNT = 5`) is rejected too:
                #        a module global is writable only from inside its own module.
COUNT := 99     # ok — a fresh binding this module owns; `COUNT = 100` after it is fine too
```

**A module global that holds a function is callable through the module:** `m.G()` (CPython and Go
agree). A non-callable member is `module 'm' member 'N' is not callable (it has type int)`.

```chezzi
# l.chz
import k
BARE := k.one     # a global whose TYPE is a fn
# main.chz
import l
print(l.BARE())   # ok — calls through the module member
```

**Types are module-scoped** like functions (exported by default, no `pub`), visible elsewhere only
through an import:

```chezzi
import core.geo
p: geo.Point = geo.Point(1, 2)   # qualified construct + annotate
c := geo.Color.Red               # qualified enum variant
xs: List[geo.Point] = []         # qualified type inside a generic
print(p)                         # Point(x=1, y=2)  — bare name, no `::`

import Point from core.geo       # named import → bare use
q := Point(3, 4)
import Point as Pt from core.geo # rename (user types only; FFI widths can't be renamed)

c0 := geo.Counter.zero()         # qualified type as a STATIC-method receiver
col := geo.Color.first()         # enum static via the qualified type (variant still wins on a clash)
```

A module-qualified generic fn takes the member turbofish and the expected-type hint, so a return-only
type param is reachable through `M.f`:

```chezzi
import lib.geo                       # fn empty_list[T]() -> List[T]
xs := geo.empty_list[int]()          # turbofish
ys: List[str] = geo.empty_list()     # …or solved from the annotation
```

`module.Type.static_method(args)` works like `module.Type(args)`. A variant name wins over a static
method on `module.Enum.x`.

**A module is reachable by its bound name and by its full path.** After an un-aliased
`import std.concurrency`, both work in every position (expression, annotation, constructor, static
method, variant, `match` pattern, generic bound):

```chezzi
import std.concurrency
a := concurrency.Shared(0)        # Go reading: the last segment is bound (unchanged)
b := std.concurrency.Shared(0)    # Python reading: the full path of the same module
```

- **The longest imported prefix wins.** With `import a.b` and `import a.b.c`, `a.b.c.X` is module
  `a.b.c`, member `X`.
- **No implicit children or parents.** `import pkg` does not give `pkg.deep.X`, and
  `import std.concurrency.collection` does not give `std.concurrency.X`. A bare `import std` is an
  error (`std` is a namespace): write `import std.math`.
- **`as` binds only the alias.** After `import pkg.deep as d`, `pkg.deep.X` is an error that names `d`.
- **Shadowing.** A local, a parameter, a type parameter or any module-level name spelled like the path
  head hides the full path (as in Python).
- **Same last segment.** `import a.math` plus `import b.math` is legal: `a.math.f()` and `b.math.f()`
  work, and a bare `math.f()` is `'math' is ambiguous` with a hint to `import a.math as am`. The pair
  still binds the name `math` once, so a third binding (`math := 3`, `fn math`) is `'math' is already
  imported`.
- A **struct pattern** stays two-level: after `import pkg.deep`, write `deep.Point(x)` (or `Point(x)`
  after `import Point from pkg.deep`); `pkg.deep.Point(x)` is `struct patterns use two-level paths`.
- **`import X from <package>`** works only for `std` (`import math from std`). For a user package
  write `import pkg.deep` (or `import pkg.deep as deep`).

A bare type whose module was imported whole but not named-imported is a check-time error
(`unknown type 'Point'; import it from geo`). Two modules may declare the same type name. Each user
type has one internal identity key (`<module-key>::Name`), but output prints the bare name, so two
`Point`s both print `Point(...)`. Reserved/native types (`T!E`/`T?`, the std types,
FFI widths) are global. `Iterator` is a member of `std.iter`. An imported `type` alias is transparent: its body resolves in the defining
module (with any FFI-width license).
