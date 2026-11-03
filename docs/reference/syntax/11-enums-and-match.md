## 8. Enums & pattern matching

```chezzi
enum Shape:
    Circle(int)         # variants may carry data
    Square(int)
    Point               # ...or not

fn area(s: Shape) -> float:
    match s:                          # match is exhaustive — compiler checks all variants
        Shape.Circle(r): return 3.14 * (r * r).to_float()
        Shape.Square(n): return (n * n).to_float()
        Shape.Point:     return 0.0
```

An enum body takes one variant per line: `enum E:` then `A B` is a parse error naming the token after
the variant (as Rust's `enum E { A B }`).

**Generic enums** take type parameters like generic structs; a payload may use them, and the enum's
own type for recursive shapes. Type arguments are inferred from the constructor or written in an
annotation, and are erased (a `Tree[int]` and a `Tree[str]` share one runtime shape). Bounds
(`[T: Comparable]`) and several parameters (`Either[A, B]`) work.

```chezzi
enum Tree[T]:
    Leaf
    Node(T, Tree[T], Tree[T])

fn sum(t: Tree[int]) -> int:
    match t:
        Tree.Leaf:          return 0
        Tree.Node(v, l, r): return sum(l) + v + sum(r)   # v is int — T substituted in the match
```

Explicit type args go **on the type**, not the variant (the declaration-site rule, §7b):
`Tree[int].Node(1, Tree.Leaf, Tree.Leaf)`, `Pair[int, str].Both(1, "a")`. The same turbofish supplies
args a payload cannot bind (a nullary variant: `Box[int].Empty`) and drives a generic static method
(`Box[int].empty()`). `Tree.Node[int](…)` is rejected with a hint to write `Tree[int].Node(…)`.

**Enum methods** are `fn name(self, …)` blocks after all variants. `self` is the whole enum value
(a body usually does `match self`). A generic enum's methods may use its type parameters
(`fn get(self) -> T`). Methods satisfy structural protocols (`str(self)` for `Stringable`, `hash(self)`
with `eq(self, other: Self)` for `Hashable`, `add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare` for the operator protocols), pass into
bound generics (`fn twice[T: Add](x: T)`) and overload operators (`a + b`, `a / b`, `-a`, `a < b`), as
struct methods do. There is no `derive`. A `test fn` is not allowed in an enum body (test suites are
struct-only).

```chezzi
enum Light:
    Red
    Yellow
    Green
    fn cost(self) -> int:           # method matching on self
        match self:
            Light.Red:    return 0
            Light.Yellow: return 1
            Light.Green:  return 2
    fn next(self) -> Light:         # a method may return a new variant
        match self:
            Light.Red:    return Light.Green
            Light.Yellow: return Light.Red
            Light.Green:  return Light.Yellow

print(Light.Green.cost())           # 2
print(Light.Red.next().cost())      # 2
```

**Variants are qualified** as `Enum.Variant` everywhere: as a value (`Shape.Point`), a constructor
(`Shape.Circle(2)`) and a `match` arm (`Shape.Circle(r):`). A bare user-variant name is a compile error
that names the enum (unless imported, §12 "Variant import"). Two enums may share a variant name
(`Color.Red`, `Light.Red`). A local or parameter named like the enum wins over it. The built-in
`?x`/`!e`/`None` stay bare.

```chezzi
p: Shape = Shape.Point          # qualified value
c: Shape = Shape.Circle(2)      # qualified constructor
match s:
    Shape.Circle(r): r * r
    Shape.Square(n): n * n
    Shape.Point:     0
```

A `match` arm may use `module.Enum.Variant`, so an enum from a whole-module `import geo` matches
without a named import:

```chezzi
import geo
import geo as g                  # an `as` alias works as the binder too
c := geo.Color.Red
match c:
    geo.Color.Red:      print("red")
    geo.Color.Green:    print("green")
match c:
    g.Color.Red:        print("R")    # aliased binder
    g.Color.Green:      print("G")
match shp:
    geo.Shape.Circle(r): r * r        # payload bindings work as usual
```

The binder is the module name (last path segment or `as` alias), or its full path after an
un-aliased import (`lib.geo.Color.Red` after `import lib.geo`). The checker verifies that the module
owns the enum. `module.Variant` without the enum name is not accepted.

`match` also works on `T!E`/`T?`:

```chezzi
match safe_div(10, 2):
    ?v:  print("got {v}")
    !e: print("failed: {e}")
```

### Patterns

**Scrutinee kinds.**
- an enum, `T?` or `T!E`
- an **int/float/str**: literal arms, which always need a `_`. A float arm (`1.5:`, `-2.5:`) matches
  by `==`: `-0.0` matches `0.0`, and no NaN literal exists (Python `case 1.5:`, Rust `1.5 =>`, Go
  `case 1.5:`)
- a **bool**: exhaustive once unguarded `true` and `false` arms both appear (a `_` after both warns as
  unreachable)
- a **tuple**: exhaustive once its arms cover the cartesian product of the element domains (no `_`)
- a **struct**: destructured positionally (below)

Patterns **nest**: a payload, a tuple element or a struct field may be a binding, a literal, `_`, a
tuple, a struct or another variant, including a nested nullary variant (the `None` in `?None` matches,
it does not bind).

**String patterns** decode like the literal in an expression: `{{`/`}}` are braces (`"{{x}}"` matches
`{x}`) and an all-digit hole is text (`"{4}"` matches `{4}`). A real hole (`"{x}"`) is a parse error
(CPython: `patterns may only match literals`); compare in a guard. A raw `r"{x}"` keeps its braces.

```chezzi
match point:                  # tuple scrutinee
    (0, 0):  "origin"
    (0, y):  "on the y axis"
    (x, y):  "at {x},{y}"     # an all-binding tuple arm is irrefutable (exhaustive)

match maybe_pair:             # nested: a tuple inside ...
    None:         print("none")
    ?((a, b)): print(a + b)

match nested:                 # nested nullary variant — the bare `None` MATCHES (not binds)
    ?None:    "inner none"
    ?(?v): "value {v}"
    None:          "outer none"
```

**Struct patterns** bind fields by position, in declaration order, with the struct's own name as the
constructor. A struct has one constructor, so a lone all-binding `Point(x, y)` arm is irrefutable. Struct
patterns nest (`Line(Point(x, y), _)`), instantiate generics (`Box(v)` on a `Box[int]` binds `v: int`)
and take refutable literal fields (`Point(0, y)`, which needs a `_` or a catch-all binding after it):

```chezzi
struct Point:
    x: int
    y: int

match p:
    Point(x, y): "at {x},{y}"       # single all-binding arm — exhaustive, no `_`

match p:
    Point(0, 0): "origin"
    Point(0, y): "on the y axis"
    rest:        "at {rest.x},{rest.y}"   # a bare name binds the WHOLE struct value (catch-all)
```

- A bare non-variant name is a whole-value catch-all on any scrutinee: `match (1, 2): (0, y): ..;
  rest: print(rest)` prints `(1, 2)`. A guarded one (`x if c:`) closes nothing. A bare variant name
  (`None`, an unqualified user variant) is never a binding.
- Write the constructor bare (`Point(x, y)`, for a local or `from`-imported struct) or qualified
  (`geo.Point(x, y)`, needed after a whole-module `import geo`).
- Only user structs destructure, not a native handle (`Socket`, a `regex.Match`).
- A wrong constructor name, a field-count mismatch (`Point(x)` on a two-field struct), a qualifier that
  is not a module (`E.Point`) and a duplicate constructor arm are checker errors.
- A struct unpacks in `match` only: `Point(x, y) := p` and a struct pattern as a fn param do not exist.
  A one-name group `(x)` is that name at `:=`, `for`, a comprehension and a wait arm.

**Or-patterns** (`p1 | p2 | ...`) match when any alternative matches (first wins), at the top of an
arm and in sub-positions (`(1 | 2, x)`, `?(1 | 2)`). Every alternative binds the same variables with
unifiable types:

```chezzi
match c:
    Color.Red | Color.Green | Color.Blue: "primary"   # a full enum or-pattern is exhaustive WITHOUT a `_`
match shape:
    Shape.Circle(a) | Shape.Square(a): a        # both alternatives bind `a` (same type)
```

`true | false` does not close the bool domain; keep a `_` (int/float/str/bool literal domains are always open).

**Guards** (`pattern if cond:`) match only when the pattern binds and the guard (which sees the
bindings) is true; otherwise the next arm is tried.

- A guarded arm never closes the match; keep a `_`.
- A variant arm with a refutable sub-pattern (a literal, range or nested variant: `?0`, `Pair(0, y)`)
  does not close that variant. Only an unguarded arm with all-wildcard/binding payload does.
- A guarded variant arm may be followed by an unguarded arm on the same variant (`E.A(n) if c` then
  `E.A(n)`).
- An exact-duplicate literal arm (`1:` twice, `"x":` twice, `1 | 1`, `0.0:` then `-0.0:`) is a
  `duplicate match arm` error,
  with the same guard exception (`1 if c:` then `1:` is legal). Range subsumption is not checked.

**Unreachable arm.** An arm after an unguarded irrefutable arm is a warning (`unreachable match arm: an
earlier arm already matches every value`); the exit code is unchanged. A guarded catch-all (`_ if c:`)
does not trigger it.

**Matching on an unannotated closure parameter.** The parameter is inferred from context (§5), so
`fn(x): match x: E.A: …; E.B: …` gives `x: E` with the ordinary exhaustiveness rules. A structural
pattern (variant, tuple, or a `?v`/`!e`/`None` payload) on a value whose type cannot be inferred (for
example `a` in `match x: (a, b): match a: E.A: …` where `x` was only shape-pinned) is
`cannot match a <tuple|variant> pattern on a value of un-inferable type; annotate it`; a trailing `_`
cannot rescue it. Literal/range/`_`/binding patterns are allowed there. A mixed literal match (`1` then
`"b"`) is rejected. Annotate the parameter (`fn(x: (E, int)): …`).

```chezzi
match n:
    x if x < 0: "neg"
    0:          "zero"
    _:          "pos"         # required: guarded arms don't close the match
```

**Range patterns** (`start..end:`) match an int in `start <= v < end` (int-only, refutable, need a `_`):

```chezzi
match score:
    0..60:  "F"
    60..90: "B"
    _:      "A"
```

The range must be non-empty: `5..1`, `3..3`, `0..-5` are the compile error `empty range pattern`
(rustc E0579), in every position. Int literals and range bounds may be negative (`-3:`, `-10..-5:`,
`-10..5`). A range is int-only: a float bound (`1.5..2.5:`) is a parse error.

```chezzi
match temp:
    -3:       "exactly minus three"
    -10..-5:  "cold"
    _:        "warm"
```

### Patterns compare the tag and bind inside it

A pattern **compares** the outer tag and **binds** only inside it.

| Pattern | Matches | Binds |
|---|---|---|
| `?v` | a present `T?`, a successful `T!E` | `v`, the payload |
| `!e` | the error of a `T!E` | `e`, the error |
| `None` | an absent `T?` | nothing |
| `?(?v)`, `?None` | a nested `T??`, layer by layer | `v` |
| `?(a, b)` | a present tuple payload | `a`, `b` |
| `!IoErr(code)` | an error that is the struct `IoErr` | `code` |
| `Color.Red`, `Shape.Circle(r)` | that enum variant | `r` |
| `3`, `1.5`, `"a"`, `1..5`, `(x, 0)` | that literal, range or tuple shape | `x` |
| `name` (a bare lowercase name) | anything: the default arm | the whole value |
| `_` | anything: the default arm | nothing |

`?v` + `None` covers a `T?`; `?v` + `!e` covers a `T!E`. A missing pattern is reported in this syntax
(``pattern `?None` is not covered``, ``pattern `!_` is not covered``), a repeated arm as
`duplicate match arm '?_'`.

A bare name always binds, so a **bare constant in a pattern is rejected**:
`` `LIMIT` is a constant; to compare write `x if x == LIMIT`, to bind use a new name ``. A bare variant
is rejected unless imported (§12 "Variant import"): write `Color.Red`.

```chezzi
LIMIT: const int = 3
match n:
    x if x == LIMIT: print("at the limit")   # compare with a guard
    other: print(other)                       # a new name binds the whole value
```

### `match` and `if` as expressions

```chezzi
# match-expression: multiline, exhaustive, each arm body is a single value-expression
label := match shape:
    Shape.Circle(r): "round"
    Shape.Square(n): "boxy"
    Shape.Point:     "dot"

# if-expression: inline, ternary-style — `else` is REQUIRED
sign := if n > 0: "pos" else: "neg"
# chained: `elif` chains without parentheses (the final `else` stays mandatory)
grade := if s >= 90: "A" elif s >= 80: "B" else: "F"
# as a statement on one line: the same expression, its value discarded
if n > 0: print("pos") else: print("neg")
if s >= 90: print("A") elif s >= 80: print("B") else: print("F")
```

**A block expression is legal where a whole value starts.** A block expression is a multi-line `match`
used as a value, or `recover:` with an indented block.

- Legal: the right side of `:=` and `=` (typed let and destructuring too), a `return` or `yield`
  value, a lambda body, an inline fn body (`fn g() -> int: match n:` + arms), a field or parameter
  default, a statement at the start of a line, a match-expression arm value (a nested `match`).
- Elsewhere it is an error that names the fix: a call argument, a list/tuple/map element, `(E)`, an
  interpolation hole, the body of an inline `if c: E`, an operand (`1 + match n:`, `-match n:`,
  `x ?? match n:`), a branch of the one-line `if … else`, an `assert` or `defer` operand, a compound
  assignment value (`x += match n:`). Bind it first: `v := match n: …`.
- The one-line forms have no such limit: `if a: b else: c`, the inline `recover: expr`, a lambda, a
  `match` arm that is an expression.

Inside brackets a block expression is legal where a value starts: a parameter default
(`fn f(x: int = match n:` + arms + `)`) or a lambda body in a call (`f(fn(x): match x:` + arms + `)`).
The block ends at a closing bracket of its level, at end of file, or at a line indented no deeper than
its opening line; a `,` does not end it. A map literal that breaks after `:` stays one line
(`{"a":` + newline + `1}`).

A block expression ends at its dedent; the next line is a new statement or arm, whatever its first
token (`(c.bump())`, `[1, 2]`, `-1`, `?g()`, or an arm `?_: v`, `(x, _): v`, `-1: v` after a nested
`match`).

- A line that starts with a binary operator, `.`, `?.`, `|>`, `,` or an assignment operator there is a
  parse error.
- Only an `else` guard may follow a block-valued binding (`x := match n:` + arms, then `else:`).
- Each match-expression arm ends its line (`1: 10 _: 20` on one line is an error).
- After a block inside an inline body (`if c: x := match n:` + arms) a dedented `else` belongs to the
  header (`if`, `elif`, a `wait` arm); after a `for`, `while` or match-arm header it is an error.

**The one-line `if … else` statement is the if-expression**, its value discarded. Each branch is an
expression. A statement branch (`if c: return 1 else: return 2`, `if c: x = 1 else: x = 2`,
`if c: break else: continue`, `if c: return else: f()`) is a parse error:
``a one-line `if`/`else` takes an expression in each branch; write a statement branch, or an `else:` guard, on its own indented line``.
Use indented blocks for those. `if c: a()` followed by `else:` or `elif` on the next line is the
statement `if`.

Because the value is discarded, **the statement form's branches may differ in type**:
`if c: print(1) else: xs.len()`, `if c: 1 elif d: "a" else: [1]` and `(if c: print(1) else: 2)` run.
Each branch is a discarded value, so a `T?` or `T!E` branch gets the discarded-value warning. Where the
value is used (`x := if …`, `_ := if …`, an argument, a `return`, an inline fn body, a closure body, a
match-expression arm, an inline `recover: expr`) the branches must have one type.

**All arms and branches must agree on a type**, with no numeric adaptation (§3):
`x := if c: 1 else: 2.5` is `branches have incompatible types: int and float — write 1.0 (or
x.to_float())` (the same for `match`).

- Branches that differ but each fit a known expected type (an annotated binding, a call argument, a
  declared return) take it: `x: Sh = if true: Sq(2) else: Tr(9)` with a protocol `Sh`. Without one,
  `x := if true: Sq(2) else: Tr(9)` is `branches have incompatible types: Sq and Tr`.
- The expected type never allows int-to-float: `x: float = if c: 1 else: 2` is an error.
- At a typed `T?`/`T!E` slot, bare and wrapped branches may mix
  (`x: int? = if n > 0: n else: None`); each bare branch wraps (§3, Success-coercion).
- An unannotated if/match expression never rewrites an error type: two `int!str` branches give
  `int!str`, and `x := if c: ?1 else: ?2` is `int?`. An annotation (`x: int!DbErr = …`) types every branch.
- Statement forms (`match s:` / `if c:` with indented blocks) are unchanged. Loops and multiline
  function bodies are statement sequences; only an inline-expr body (`fn a(): <expr>`, §5) returns implicitly.
