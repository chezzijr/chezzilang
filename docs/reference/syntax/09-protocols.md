### Protocols

**Protocols** are Go-style structural interfaces: a block of body-less method signatures. A type
satisfies a protocol by *having* the methods; there is no `implements`. `Self` in a signature is the
conforming type. Another module uses a protocol after `import Drawable from shapes` or as the
qualified `shapes.Drawable`.

A built-in `List`/`Map`/`Set`/`str`/`bytes`/`bytearray` satisfies a user protocol the same way, from
its own methods: `protocol Sized: fn len(self) -> int` is satisfied by `[1,2,3]`, `{"a": 1}`,
`{1, 2}`, `"abc"`, `b"ab"` and `bytearray.from_iter(b"ab")`. `int`/`float`/`bool` have no methods, so they
satisfy only an empty protocol. The handles (`Channel`, `Shared`, `RwShared`, `Atomic`, `Executor`,
`Socket`, `Listener`, `Writer`, `Reader`) do not witness a user protocol.

**Where the std protocols live.** Each std protocol is declared in a std module. A bound or a value
type names it after an import (`import Add from std.ops`). Without the import the name is
`unknown name 'Add'`. An operator, a hook or a `for` loop needs no import: `a + b` and
`for x in xs` work with no protocol name in the program. `Any` and `Error` stay global.

| Module | Protocols |
|---|---|
| `std.ops` | `Add`, `Sub`, `Mul`, `Div`, `Mod`, `Neg`, `BitAnd`, `BitOr`, `BitXor`, `Shl`, `Shr`, `Arithmetic`, `Num`, `Index`, `IndexSet`, `Slice`, `Contains` |
| `std.cmp` | `Eq`, `Comparable`, `Hashable` |
| `std.iter` | `Iterator`, `Iterable` |
| `std.string` | `Stringable` |
| `std.path` | `PathLike` |
| `std.convert` | `Convert` |

`Iterator[T]` as a value type (a generator's return type) needs `import Iterator from std.iter`, or
the qualified `iter.Iterator[T]` after `import std.iter as iter`. The `protocol` declarations in
`std/*.chz` are the only source of these shapes. The examples below omit the import lines.

**Protocol parameter names.** Parameter names are part of a protocol's contract (as Swift's argument
labels), for user and built-in protocols. A call through a protocol binds named arguments by the
protocol's names, so a method conforms only with the same names in the same positions:

```chezzi
protocol Rep:
    fn replace(self, old: str, new: str) -> str

struct Doc:
    text: str
    fn replace(self, new: str, old: str) -> str:   # before: accepted, and p.replace(old="a", new="b")
        return self.text.replace(old, new)         #   swapped the two arguments
# ERROR: type Doc does not satisfy Rep (method 'replace' parameter 1 is named 'new', but Rep declares 'old')

struct Doc2:
    text: str
    fn replace(self, old: str, new: str) -> str:   # after: same names, conforms
        return self.text.replace(old, new)
```

- Built-in protocols take their names from `std/prelude.chz`: `compare`/`eq` and the operator methods
  (`add`/`sub`/`mul`/`div`/`mod`) name their operand `other`; `Index.index` names `k`;
  `IndexSet.set_index` names `k`, `v`; `Slice.slice` names `r`; `Contains.contains` names `item`. So
  `fn compare(self, o: P) -> int` does not make `P` `Comparable`; write `other`.
- A built-in type conforms through its native method's names (`str.replace(self, old, new)` satisfies
  `Rep`). A direct call to a native method still takes no named arguments.
- The hooks follow the same names: a misnamed `index`, `set_index`, `contains` or `slice` does not make
  the type indexable, index-assignable, `in`-testable or sliceable, and the error says why:
  `cannot index into V (method 'index' parameter 1 is named 'i', but Index declares 'k')`,
  `cannot apply + to W and W (method 'add' parameter 1 is named 'o', but Add declares 'other')`. `==`
  dispatches a user `eq` by shape, whatever the operand is called.

**`Self` in a struct/enum method.** `Self` is usable in an inherent method's signature and body (param
type, return type, local annotation) and names the enclosing type: `fn dup(self) -> Self` in
`struct P` returns a `P`; for `Box[T]` it carries the receiver's type args. A `-> Self` method that
returns another type is a type error. Outside a method (a free-fn parameter, a struct field, a
top-level annotation) it is `unknown type 'Self'`.

### Ordering: `Comparable`

```chezzi
protocol Comparable:                 # std protocol — this is its declaration in std/cmp.chz, where
                                     # `Eq` needs no import; a program imports it from std.cmp
    Eq                               # embedded: ordered ⇒ equatable (Rust's `Ord: Eq`)
    fn compare(self, other: Self) -> int

struct Point:
    x: int
    y: int
    fn compare(self, other: Point) -> int:   # ⇒ Point satisfies Comparable, structurally.
        return (self.x + self.y) - (other.x + other.y)
    fn eq(self, other: Point) -> bool:       #   `eq` is OPTIONAL here — a struct already satisfies
        return (self.x + self.y) == (other.x + other.y)   # `Eq` structurally. Write one only to
                                                          # OVERRIDE `==`, and keep the two agreeing.

print(max(Point(1, 2), Point(3, 0)).x)   # works: Point is Comparable
```

`< <= > >=` dispatch to `compare` for any `Comparable` value, including a bare `T: Comparable`
(negative/zero/positive means less/equal/greater).

- `int`, `float`, `bool` and `str` are `Comparable` (`false < true`, as in Python and Rust). So are a tuple and a `List[T]` when every element type
  is: they order lexicographically (the first unequal element decides, a shorter prefix sorts
  first). A `T?` derives no ordering: `a < b` on two `int?` is `cannot compare`, and `xs.sort()` on a
  `List[int?]` is `type int? does not satisfy Comparable`. `print((1, 2) < (1, 3))  # true`;
  `[(2, "b"), (1, "z")].sort()` gives `[(1, "z"), (2, "b")]`; `min`/`max`, `sort_by_key` with a tuple
  key and `std.cmp` work. A tuple with a non-`Comparable` element is rejected.
- A struct has no default ordering: write `compare`. `Map`, `Set` and `T!E` are unordered.
- Operators on a `NaN` pair are all `false`; `sort`/`min`/`max` use the total order of bare floats.
- `compare` alone is enough: a struct/enum satisfies the embedded `Eq` structurally. Keeping `compare`
  and `eq` in agreement is your job; the checker cannot verify `eq(a, b) == (compare(a, b) == 0)`.

```chezzi
print(Point(1, 1) < Point(5, 5))     # true  — `<` calls Point.compare
```

```chezzi
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
    # no `eq` needed — Ver satisfies Comparable, and `==` stays structural
```

### Equality: `Eq`

`==` / `!=` do not use `compare` (a `compare` that ignores fields would redefine equality). The
prebuilt **`Eq`** protocol (`eq(self, other: Self) -> bool`) owns them: a struct/enum that defines
`eq` owns its `==` / `!=`. Without `eq`, `==` is structural (field by field).

**`Eq` satisfaction is exactly "`==` works on it".** Structural `==` is an automatic derive (Go gives
structs `==`; Rust spells it `#[derive(PartialEq, Eq)]`). So `where T: Eq` accepts:
`int`/`float`/`bool`/`str`, `bytes`, tuples, `List`/`Map`/`Set`, `T?`/`T!E`, any struct or enum, a
function value, and a protocol-typed (existential) value.

- A function value compares by identity: two loads of one `fn` def (or builtin) are equal; two calls to
  a factory that makes a fresh nested `fn` are not; two closures with equal captures are not (CPython
  and Rust agree; Go rejects `f == g`).
- A named fn is one value: `P.mk == P.mk`, `E.A == E.A`, `PA.mk == P.mk`, `lib.Q.mk == Q.mk`,
  `T.get == T.get`, `math.sqrt == math.sqrt`, `json.decode[int] == json.decode[int]` and an `extern`
  fn are `true` in any module, in a spawned task, after a `spawn:` capture, a `Channel` or an
  `Atomic.load`.
- A native, builtin or extern fn value spawns (`g := math.sqrt; spawn g(9.0)`) and submits
  (`ex.submit(time.now_ms)`) like any fn.
- A protocol-typed value satisfies `[T: Eq]` (Go 1.20+'s `comparable` admits an interface type) and
  defers the compare to its witness's `eq` at runtime. Other protocols still reject a protocol-typed
  value that does not provide them (`[T: Stringable]`/`[T: Hashable]` over `Sized_` reject).
- An `eq` (its own, or one reached through an element, entry, tuple slot, field or payload) whose
  `where` bounds fail for the instantiation revokes the grant (the `Box[Tag]` case in
  [Generics](08-generics.md#conditional-methods-and-conformance)).
- A bare `T` with no bound is outside the grant.

```chezzi
struct Ver:
    maj: int
    pre: str
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
    fn eq(self, other: Ver) -> bool:                 # `==` asks THIS, not the fields
        return self.maj == other.maj

print(Ver(1, "alpha") == Ver(1, "beta"))         # true  — `pre` differs, `eq` says equal
print(Ver(1, "alpha") != Ver(1, "beta"))         # false — `!=` is the same dispatch, negated
```

`a.eq(b)` and `a == b` are one dispatch, so a `[T: Eq]` body may write either. Dispatch is by runtime
type: if the operands are different struct/enum types the result is structural `false` with no user
code. For an enum the *enum* decides, so one `eq` also answers `Shape.Circle == Shape.Square`.

The operator dispatches only to the exact signature `fn eq(self, other: Self) -> bool`. A wrong
arity, a concrete non-`Self` operand or a non-`bool` return is rejected at the declaration:

```chezzi
struct A:
    v: int
    fn eq(self) -> bool:      # type error: 'eq' … must take exactly one operand
        return true
```

A **generic** operand is legal: it is an ordinary method that shares the name, and `==` stays structural:

```chezzi
enum Opt[T]:
    Has(T)
    Empty
    fn eq(self, x: T) -> bool:               # an ordinary method, NOT the Eq hook
        return true

print(Opt[int].Has(1).eq(7))                 # true  — the method still works
print(Opt[int].Has(1) == Opt[int].Has(2))    # false — `==` stays structural
```

**Through a bound, `.eq()` is the protocol's equality.** A concrete receiver calls the ordinary method
(Rust's inherent-wins rule); a bound type parameter resolves to the protocol, as Rust resolves
`a.eq(b)` under `T: Eq`:

```chezzi
import Eq from std.cmp

struct Key:
    n: int
    fn eq[U](self, o: U) -> bool:            # an ordinary method, NOT the Eq hook
        return true

fn eqm[T: Eq](a: T, b: T) -> bool:
    return a.eq(b)

print(Key(1).eq(Key(2)))                     # true  — concrete receiver: the ordinary method
print(Key(1) == Key(2))                      # false — `==` stays structural
print(eqm(Key(1), Key(2)))                   # false — through the bound: the PROTOCOL's equality
```

A real `eq` hook is dispatched through the bound, so `.eq()` and `==` never disagree.

A user `eq` reaches every equality site: `Map`/`Set` key lookup (`m[k]`, `has`, `get`, `remove`, `in`,
`add`), `x in xs`, `list.contains`/`index_of`/`dedup`/`unique`, set algebra, and the element/field/entry
compares inside `==` on a container:

```chezzi
struct K:
    a: int
    b: str
    fn hash(self) -> int: return self.a
    fn eq(self, other: K) -> bool: return self.a == o.a

x := K(1, "x")
y := K(1, "y")
print(x == y)          # true
print(y in [x])        # true
m: Map[K, int] = {}
m[x] = 10
print(m[y])            # 10
print([x] == [y])      # true — the recursion reaches the element's `eq`
```

**Limits of a user `eq`.** Read these before you write one.

- A **container** checks identity first (`x is y or x == y`, as CPython), so `[x] == [x]` is `true`
  even for an `eq` that always answers `false`. The bare `==` operator always calls `eq`, so `x == x`
  is `false` for that type.
- **`hash` and `eq` must agree.** A `Map`/`Set` probe scans only the buckets of `hash(key)`, so an
  `eq` coarser than `hash` can never match. Key `hash` and `eq` on the same fields: `eq` may read fewer
  fields than the struct has, never fewer than `hash` does. Chezzi does not check this.
- **Keep `eq` reflexive.** A `Map`/`Set` key is a snapshot (see "Keys are value types" in
  [the hooks chapter](10-protocol-hooks.md#hashing-hashable)), so the probe compares against a copy and
  the identity check cannot help:

  ```chezzi
  struct Never:
      a: int
      fn hash(self) -> int: return self.a
      fn eq(self, other: Never) -> bool: return false     # never reflexive — a bug, not a feature

  x := Never(1)
  s: Set[Never] = {x}
  print(x in s)       # false — the stored key is a COPY, so identity never fires
  print([x] == [x])   # true  — same object, identity short-circuit fires
  ```
- **An `eq` that mutates the container being probed** gets the answer from the container as it is
  after the change, never a crash: the probe checks after each compare and restarts if a key was
  added or removed (CPython's `DKIX_KEY_CHANGED`). An `eq` that mutates on every call makes the probe loop
  forever. Do not do it.
- **`Atomic[T].cas` compares structurally and never calls a user `eq`** (it holds a lock during the
  compare). The checker rejects a payload type that reaches a user `eq` on any element, entry, tuple
  slot, field or payload (use `Shared[T]`, which has no `cas`), and the VM turns the `eq` hook off
  during the compare. A type-parameter payload needs the sealed bound `StructuralEq` from
  `std.concurrency` (`fn mk[T: StructuralEq](v: T) -> Atomic[T]`). See [`concurrency.md`](../concurrency/01-model.md).
- **Generic instantiations are erased at runtime, so `Any` can pass `eq` a foreign operand.**
  `a: Any = Box(1)`, `b: Any = Box("x")`, `a == b` checks and calls `Box`'s `eq`, whose body then
  faults on `int` vs `str` (recoverable; CPython raises `TypeError`). Keep the static type for the
  compile-time answer.
- **`match` never calls `eq`.** A literal pattern is `int`/`float`/`str`/`bool` only (a float
  literal matches by `==`, so `-0.0` matches `0.0`), and a struct/enum arm
  matches by variant and binds fields.

**A generic body that compares its own type parameter must bound it by `Eq`.**
`fn f[T](a: T, b: T) -> bool: return a == b` is rejected at the declaration: `cannot compare T and T
for equality — T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)`
(rustc: `error[E0369]`; Go: `incomparable types in type set`). The same check fires at a type
annotation (`struct Reg[K: Hashable]: m: Map[K, int]`).

- It covers `in`/`contains`/`index_of`/`dedup`/`unique` over `List[T]` and a `T` used as a map key or
  set element. A type parameter that is never compared needs no bound.
- `Comparable` and `Hashable` embed `Eq`. A generic keyed on `T` writes `[T: Hashable]`;
  a redundant `+ Eq` still checks.
- A type graph nested deeper than **10 000** links is refused at a `[T: Eq]` bound or an `==` (the
  VM's equality depth cap). A growing (polymorphic-recursion) type graph is refused much earlier.

**`==` between provably-disjoint types is a compile error.** `1 == "a"`, `Box[int] == Box[str]` or two
different structs can only answer `false` (Python answers `False` at runtime; Chezzi follows mypy's
`--strict-equality`, Go and Rust). The question is "can these two ever be the same value?". Legal:

- the runtime's cross-type pairs, at any depth: `int`/`float` (`1 == 1.0`, and `<`/`<=`/`>`/`>=`)
  compare exactly (an integer above 2^53 never collapses onto a nearby float), and `bytes` vs
  `bytearray` (content-equal). `[1.0] == [1]` and `{"k": 1.0} == {"k": 1}` are `true`.
- a protocol existential against a conforming type (`sh: Shape` vs a `Sq`, an `Error` vs your error
  struct), in either order
- two different existentials (`Shape` vs `Error`): one type can conform to both
- an existential nested in a container, generic struct or handle: `List[Error]` vs `List[MyErr]`,
  `Map[str, Error]` vs `Map[str, MyErr]`, `Box[Error]` vs `Box[MyErr]`, `Error?` vs `MyErr?`,
  `(Error, int)` vs `(MyErr, int)`, `Shared[Error]` vs `Shared[MyErr]` (not assignable, but they can
  hold equal values)
- any comparison with an erased type parameter, bare or nested (`a == 1` and `xs == [1]` in
  `fn f[T: Eq](a: T, xs: List[T])`; `a == b` in `fn cmp[T](a: Channel[T], b: Channel[int])`; a free
  `T` in a protocol's arguments, `Container[T]` vs a conforming `Bag[int]`). A `where T: <scalar>`
  bound pins `T`, so `fn f[T](a: T, b: int) where T: str` rejects `a == b`. `T` still needs its `Eq`
  bound: `fn f[T](a: T, xs: List[T]): return a == 1` is `T is not bounded by Eq`.
- A handle (`Channel[T]` vs `Channel[int]`) compares by identity, never by element, so `cmp`'s `T`
  needs no `Eq` bound. The identity survives every crossing (spawn capture or argument, `Channel` send,
  `Shared`/`RwShared`/`Atomic` store, `Executor` submit).

A non-conforming concrete stays an error at every depth (`sh: Shape` vs a `str`, `List[Shape]` vs
`List[str]`, `Container[T]` vs an `int`), and handles with disjoint element types are disjoint
(`Channel[int]` vs `Channel[str]`). For the dynamic answer, widen one side to `Any`: `u: Any = a; u == b`.

### Arithmetic operators

Arithmetic is overloaded through **`Add`/`Sub`/`Mul`/`Div`/`Mod`** (`add`/`sub`/`mul`/`div`/`mod(self,
other: Self) -> Self` for `+ - * / %`) and **`Neg`** (`neg(self) -> Self` for unary `-`). `int`/`float`
satisfy all six. `/` truncates and `%` is the int remainder, so `Div`/`Mod` are `Self -> Self`.

```chezzi
struct Vec2:
    x: int
    y: int
    fn add(self, other: Vec2) -> Vec2:
        return Vec2(self.x + other.x, self.y + other.y)
    fn div(self, other: Vec2) -> Vec2:
        return Vec2(self.x / other.x, self.y / other.y)
    fn neg(self) -> Vec2:
        return Vec2(-self.x, -self.y)

print((Vec2(1, 2) + Vec2(3, 4)).x)   # 4    — `+` calls Vec2.add
print((Vec2(6, 8) / Vec2(2, 4)).y)   # 2    — `/` calls Vec2.div
print((-Vec2(1, 2)).x)               # -1   — unary `-` calls Vec2.neg
```

### Protocol embedding (super-protocols)

A protocol body may list **embed lines** (protocol refs joined by `+`) among its `fn` signatures, in
any order; a body of only embed lines is a bundle. A type satisfies the protocol if it satisfies every
embed (transitively) and has every own method. A type that fails an embed is reported under the
protocol you asked for, with the embed's reason: `type N does not satisfy Ord2 (missing method
'compare')` for `protocol Ord2: Comparable`.

- **`Arithmetic`** is `Add + Sub + Mul + Div`.
- **`Num`** is `Arithmetic + Mod + Neg + Comparable`, satisfied by `int` and `float` only (sealed). A
  `T: Num` body gets `+ - * / %`, unary `-` and `< <= > >= == !=`, each on `T`; `x + 1` on a `T` is
  refused (no int/float mix, Rust's rule).
- An own `fn` that matches an embedded method name is an error. Two embeds that require one method
  with the same signature merge (`Arithmetic + Add` is fine); with different signatures it is an
  error. A cyclic embed is an error.
- The embed set applies at every use: through a protocol value and through a bounded type parameter,
  an embedded method is callable, `in` uses an embedded `Contains`, `[]`/`[a..b]` an embedded
  `Index`/`IndexSet`/`Slice`, unary `-` an embedded `Neg`.
- A protocol value satisfies the protocols it embeds (a `Person` that embeds `Named` is accepted where
  a `Named` is wanted). Type arguments stay invariant: a `Container[str]` never satisfies `Container[int]`.
- An embed's type argument must name a real type. It may use the owner's type parameter, whole
  (`protocol Bag[T]: Contains[T]`) or nested (`Contains[List[T]]`).

```chezzi
import Arithmetic, Neg, Num from std.ops   # Arithmetic embeds Add + Sub + Mul + Div (std/ops.chz)

protocol VectorSpace:       # embeds two protocols and adds its own requirement
    Arithmetic + Neg
    fn dot(self, o: Self) -> int

fn combine[T: Arithmetic](a: T, b: T) -> T:   # +, -, *, / all available on T
    return (a + b) * (a - b) / b

fn twice[T: Num](x: T) -> T:   # int or float, never a mix
    return x + x

protocol Named:
    fn name(self) -> str

protocol Person:            # embeds Named, adds its own requirement
    Named
    fn age(self) -> int

fn only_named(n: Named) -> str:
    return n.name()

fn show(p: Person) -> str:       # an INTERFACE VALUE, not a bound
    return p.name() + " " + only_named(p)   # embedded method; Person satisfies Named
```

**A witness method may not be generic.** `fn show[U](self) -> str` cannot satisfy a requirement:
*method '…' is generic and cannot witness a protocol requirement* (Rust: `E0049`). Put the parameter on
the protocol (`protocol Show[U]: fn show(self) -> U`); it is inferred from the witness.

**Object safety: `Self` in a parameter.** A protocol value erases its witness, so two values of one
protocol may hold different types. A method that takes `Self` is not usable where two witnesses could
meet:

- calling it on a value (`a.add(b)` with `a, b: Vecish`) is rejected
- the operator forms are rejected (every operator method is `(self, Self) -> Self`, so `+ - * / %` and `<`)
- a protocol value cannot be the witness for a type parameter whose bound needs such a method
  (`sum2[T: Vecish](a: T, b: T)`)

Bind the operands with a generic parameter over a concrete type:
`fn plus[T: Vecish](a: T, b: T) -> T: return a + b` called with two `V`s. The value still passes to a
parameter of its protocol and satisfies its embeds, and `Self` in a **return** is fine, so
`fn neg(self) -> Self` and unary `-` work. Rust has the same rule; Go bans `Self` from interfaces.

### Indexing, slicing and membership

Indexing and slicing use the prebuilt **`Index[K, V]`** (`obj[k]` via `index(self, key: K) -> V`),
**`IndexSet[K, V]`** (`obj[k] = v` via `set_index(self, key: K, val: V)`; it embeds `Index[K, V]`, as
Rust's `IndexMut: Index`, because a compound `obj[k] += v` reads first) and **`Slice[R]`** (`obj[r]`
for a `Range` via `slice(self, r: Range) -> R`; the parameter is named `r`). Built-in `list`/`map`/`str`
satisfy them (`str` has `Index`/`Slice`, not `IndexSet`). A generic can be bounded by them; `K`/`V`/`R`
are recovered at the call site.

- `IndexSet[K, V]` requires `Index[K, V]` (as Rust's `IndexMut: Index`).
- A compound `obj[k] += v` types `obj[k] OP v` from `index`'s return and stores it against
  `set_index`'s `val` (§3); both key on the same `K`.
- A plain `obj[k] = v` never reads, so an asymmetric pair (a safe-read `index -> V?`, a wider writer)
  is legal there.
- `index` alone is a read-only type; `set_index` alone is not index-assignable.

```chezzi
import Index from std.ops

struct Ring:
    data: List[int]
    fn index(self, k: int) -> int:
        return self.data[k % self.data.len()]
    fn set_index(self, k: int, v: int):
        self.data[k % self.data.len()] = v
    fn slice(self, r: Range) -> List[int]:
        return self.data[r]

r := Ring([10, 20, 30])
print(r[3])            # 10   — wraps; `index` dispatched
r[1] = 99              # `set_index` dispatched
print(r[0..2])         # [10, 99]   — `slice` dispatched

fn first[C: Index[int, V], V](c: C) -> V:   # works over a list OR a Ring
    return c[0]
```

**Membership (`in`)** uses the prebuilt **`Contains[Item]`** (Python's `__contains__`): a type with
`contains(self, item: Item) -> bool` makes `x in value` call it. Built-in `list`/`set`/`str` test
element/substring membership and `map` tests key membership.

```chezzi
struct Bag:
    items: List[int]
    fn contains(self, item: int) -> bool:
        return item in self.items

b := Bag([1, 2, 3])
print(2 in b)          # true   — `contains` dispatched
print(9 in b)          # false
```

### Parameterized protocols

A protocol may take type parameters (`protocol Container[T]:`). A bound supplies the arguments, and a
type satisfies it with them substituted:

```chezzi
protocol Container[T]:
    fn get(self, i: int) -> T

fn first[X: Container[int]](c: X) -> int:   # T pinned to int; c.get(0) is int
    return c.get(0)
```

- The number of args must match the arity (a bare protocol takes none).
- A parameterized protocol is a value type too: `c: Container[int]` is a valid parameter, return,
  field or reassignment slot (an existential). Its methods and its embeds' methods are callable,
  except a method taking `Self`. The args are checked at every store/pass and erased at runtime. A
  method that returns the protocol's param recovers the arg: `c.get(0)` on a `Container[int]` is `int`.
- Value-position protocols are invariant: `Container[int]`, `Container[str]` and bare `Container` are
  three distinct types (no `Iterator[int]`→`Iterable[int]` subsumption). A bare generic protocol as a
  value type has unbound params, so a struct with a concrete-typed method does not conform; supply the args.

```chezzi
fn first(c: Container[int]) -> int:   # existential value slot
    return c.get(0) + 1               # c.get(0) recovers to int
```

> **A protocol crosses a module boundary like any other declaration.** `mod.Named` and
> `import Named from mod` (with or without `as`) resolve, in a bound too: `[T: mod.Named]`,
> `where T: mod.Named`, `[T: mod.A + mod.B]`, `[S: mod.Container[int]]` and an embed line
> `mod.Named`. A static requirement dispatches as for `[T: Named]`. The last segment is the protocol
> and the rest is the module: `[T: a.b.Named]` names module `a.b` and needs an un-aliased `import a.b`.
