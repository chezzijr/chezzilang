## 7b. Generics & protocols

This chapter covers generics and type aliases. [Protocols](09-protocols.md) and the
[operator hooks](10-protocol-hooks.md) follow.

**Generic functions** take type parameters in `[…]` after the name. A parameter may carry a
**bound**: a protocol the instantiating type must satisfy. Type arguments are inferred from the call
or given explicitly: `id[int](42)`, and the struct form `Pair[int, str](1, "one")`. For a generic
**enum** the args go on the type, not the variant: `Box[int].Full(9)` (§8). Explicit args pin the
type; inference fills the rest.

A bound names a std protocol, so the program imports it from the std module that declares it
(`import Comparable from std.cmp`; `Any` and `Error` are global). An operator needs no import: `a < b`
works on a type with a `compare` method whether or not the program names `Comparable`. The table of
protocols and modules is in [Protocols](09-protocols.md); the import rule is in [Imports](16-imports.md).

```chezzi
import Comparable from std.cmp

fn first[T](a: T, b: T) -> T:        # unbounded: works for any type
    return a

fn max[T: Comparable](a: T, b: T) -> T:   # bounded: T must be Comparable
    if a < b:
        return b
    return a

print(max(3, 7))                     # 7   (int is Comparable)
print(max("apple", "banana"))        # banana
```

**Where inference takes `T` from.** The same rule holds in every position of a generic call or a
generic fn value: an argument, `print`'s pack, an annotated binding and a `return`.

1. `T` comes from the call's own arguments. A conflict among them is an error, whatever the outer
   slot is: `fn pick[T](a: T, b: T) -> T`, then `print(pick(5, 1.5))`, `y: Any = pick(5, 1.5)` and
   `oa(pick(5, 1.5))` over `fn oa(v: Any)` all report `argument to 'pick' has type float, expected int`.
2. An expected type fills only a `T` that the arguments leave open, and only with a concrete type:
   `ys: List[int] = tag([])` checks, and `oc(empty())` over `fn oc(v: Counter)` fills `T := Counter`.
   A protocol (`Any`, `Stringable`, any other) never fills `T`, nullable or not (`Any?`), because
   `Any` is an ordinary empty protocol. So `print(ident)`, `oa(ident)`, `print(Holder.build())` and `oa(empty())` report
   `T is not determined here`, as Go reports `cannot infer T`. A concrete type that holds a protocol
   still fills its params: `g: Bag[Named] = Bag([A()])` gives `T := Named`.
3. Inside an open `T`, a closure param takes `T` from the arguments: `print(apply(1, fn(a): a + 1))`
   and `oa(apply(1, fn(a): a + 1))` print `2` and `a:2`.

`tests/generic_hint_grid.rs` runs each case in each position. The owner is `hint_bindings`
(`checker/ty_util.rs`).

A type parameter may carry several bounds with `+`: `fn fma[T: Add + Mul](a: T, b: T, c: T)`.

A bound may also be written in a **`where` clause** after the return type. Each entry names a declared
type parameter and lists its protocols (`+`-joined; entries comma-separated); the checker merges them
into the `[T]` parameter. A `where` entry needs a bound: `where T:` and `where T, U: Eq` are
``a `where` entry needs a bound (`where T: Proto`)``:

```chezzi
import Comparable from std.cmp
import Add, Mul from std.ops

fn max[T](a: T, b: T) -> T where T: Comparable:   # same as `fn max[T: Comparable](a, b)`
    if a < b:
        return b
    return a

fn combine[A, B](a: A, b: B) where A: Add + Mul, B: Comparable:   # multi-entry, multi-bound
    ...
```

A `where` entry that names neither a declared type parameter nor the enclosing type's own parameter is an error.

### Conditional methods and conformance

A `where` on a *method* may name the enclosing struct/enum's own type parameter. The method is then
callable only when the receiver's type argument satisfies the bound (Rust's
`impl<T: Ord> Box<T> { fn top(…) }`), like the built-in `List[T].sort` / `sum`. The bound is
checker-only; it compiles to nothing.

```chezzi
import Comparable from std.cmp

struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:   # conditional on the RECEIVER's own T
        return self.val

    fn max2(self, other: T) -> T where T: Comparable:
        if self.val < other:                 # the body MAY use the bounded op (`<` needs Comparable)
            return other
        return self.val

b := Box(5)
print(b.top())                       # 5   — int is Comparable
print(b.max2(9))                     # 9

import std.net
fn f(s: net.Socket):
    bad := Box(s)
    bad.top()                        # ERROR at check time: type Socket does not satisfy Eq
                                     # (Comparable embeds Eq; the first unmet bound is named)
```

The body may use the bounded operation. The bound is enforced on an instance call (`b.top()`) and on a
static factory (`fn of(x: T) -> Box[T] where T: Comparable` rejects `Box.of(q)` when `q` is not
`Comparable`).

**Conditional conformance.** When the conditional method is a protocol's required method (for example
`compare(self, other: Self) -> int where T: Comparable` makes `Box[T]` structurally `Comparable`; no
`eq` is needed), the conformance is conditional: `Box[int]` is `Comparable` (`Box(1) < Box(2)` and a
`[U: Comparable]` generic accept it), `Box[Tag]` is not. The bound holds everywhere conformance is
asked: operators, generic bounds, protocol-typed parameters.

**A conditional hook never falls back to the hookless path.** A conditional method that is an
operator's hook is the hook for every instantiation. An operator on an instantiation that fails the
bound is a compile error that names the bound, even where the type has another way to satisfy it:

```
import Comparable from std.cmp
import Iterator from std.iter

struct Box[T]:
    v: T
    fn next(self) -> int?: return None
    fn iter(self) -> Iterator[str] where T: Comparable: return ["a"].iter()
for q in Box(Tag(1)):   →  type error: cannot iterate over Box[Tag]
                            (method 'iter' requires Tag: Comparable)
```

`Box[Tag]` does not iterate through `next`; a `struct Z[T]` with a conditional `hash` is no key as
`Z[Tag]`; `Box[Tag]` is not a `StructuralEq` payload when `Box` has a conditional
`eq`. `for`, comprehensions, `List()` / `Set()` / `Map()`, `x[k]`, `x[k] = v`, `x[a..b]`, `in`, the
arithmetic operators, unary `-` and a `Set` / `Map` key all report the bound. Exception: `print`
accepts every value, so a conditional `str` runs for every instantiation.

**The declaration decides a hook, never the instantiation.** A method whose signature conforms for one
instantiation only is the hook of none. It is an ordinary method, the near-miss warning applies, and
the operator reports its ordinary error:

```
struct Key[T]:
    v: T
    fn hash(self) -> T: return self.v      # `T` is not `int`: never the Hashable hook
s: Set[Key[int]] = Set()                    →  type error: Set element type must implement Hashable

struct Box[T]:
    v: T
    fn compare(self, other: Box[int]) -> int: return -1   # the operand is not `Self`
Box(1) < Box(2)                             →  type error: cannot compare Box[int] and Box[int]
```

Write the signature for every instantiation (`other: Self`, `-> int`) and put a restriction in a `where` bound.

**`==` honours the bound too.** A conditional `fn eq(self, other: Self) -> bool where T: Comparable`
is the hook of `==`/`!=`:

```
import Comparable from std.cmp

Box[Tag] == Box[Tag]  →  type error: cannot compare Box[Tag] and Box[Tag] for equality
                         — Box[Tag]'s `eq` requires Tag: Comparable
```

`Box(1) == Box(2)` runs and prints `false` (Rust: `error[E0369]` for the unmet case). The rule covers
`!=`, containers and payloads (`[a] == [b]`, `?a == ?b`, tuples, map values, struct fields), `x in xs`,
`list.contains`/`index_of`/`dedup`/`unique`, and every map-key / set-element position, each with a
message naming the site (*contains() compares List[Box[Tag]] elements for equality — …*,
*map key type Box[Tag]'s `eq` requires …*).

**An unmet `eq` bound is refused where the value enters a protocol slot** (a `let`, a field, a
`List`/`T?`/`T!E` element, a return):

```
import Comparable from std.cmp

protocol Tagged: fn tag(self) -> int
struct Box[T]:
    v: T
    fn eq(self, other: Box[T]) -> bool where T: Comparable: return true
a: Tagged = Box(Tag(1))  →  type error: cannot assign Box[Tag] to variable of type Tagged
                             — Box[Tag]'s `eq` requires Tag: Comparable
```

`fn wrap[T: Comparable](x: Box[T]) -> Tagged` compiles. Two protocol-typed operands compare at runtime
through the witness's `eq`, as Go's `interface == interface` does.

### Generic structs

Generic structs take type parameters after the name; fields and methods may use them. Type arguments
are inferred at construction or written in an annotation. An expected type widens an inferred type
argument when every argument fits: `b: Box[Named] = Box(A())` is a `Box[Named]`, while `Bag(zs)` with
an existing `zs: List[A]` stays invariant and is rejected.

```chezzi
struct Pair[A, B]:
    first: A
    second: B
    fn left(self) -> A:
        return self.first

struct Stack[T]:
    items: List[T]
    fn push(self, x: T):
        self.items.push(x)

p := Pair(42, "hi")              # inferred Pair[int, str]
print(p.left())                  # 42  — left() returns A = int
q: Pair[str, int] = Pair("k", 9) # explicit type arguments
```

A method may introduce its own fresh type parameters, inferred from the call (it may not reuse a
struct parameter's name):

```chezzi
struct Box[T]:
    v: T
    fn map_to[U](self, f: fn(T) -> U) -> U:   # U is fresh; inferred from the closure
        return f(self.v)

b := Box(5)
print(b.map_to(fn(x: int) -> str: "n{x}"))    # U = str  → "n5"
```

Generics are **type-erased**: at runtime a `Pair[int, str]` is an ordinary struct value (no
monomorphization).

### Type aliases

`type Name = <type>` makes `Name` interchangeable with the aliased type (not a distinct nominal type).
An alias may name a scalar, a collection, a struct or another alias (cycles are rejected). An alias of
a protocol may name a bound (`type N = Named` then `fn show[T: N]`, also after `import N from m`); an
alias that applies type arguments (`type IntBag = Bag[int]`) may not, so write `T: Bag[int]`.

```chezzi
type UserId = int
type Scores = Map[str, int]
uid: UserId = 7        # UserId and int are the same type
```

An alias of an enum or struct also works in expression and pattern position: a constructor, a static
method call, a variant pattern.

```chezzi
enum E:
    A(int)
    B
struct P:
    x: int
    fn zero() -> P:
        return P(0)
type F = E
type Q = P
F.A(5)            # variant ctor
match F.A(5):
    F.A(n): print(n)   # variant pattern
    F.B: print("b")
Q(1)               # struct ctor
Q.zero()           # static method
```

The same holds for an imported alias (`import F from m`, with `as` too, or `m.F`) and a local alias
of `module.Type`. Each resolves to the aliased type itself; an alias never creates a runtime value:

```chezzi
import F, Q from m           # m declares: type F = E, type Q = P
import m
type R = m.P                 # a local alias of a qualified type
F.A(5)                       # m's E.A(5)
m.F.B                        # qualified alias, nullary variant
match m.F.A(1):
    m.F.A(n): print(n)       # qualified alias in a pattern
    F.B: print("b")
Q(1) == R(1)                 # both construct m's P
R.zero()                     # static method through the alias
```

An alias of a generic instantiation keeps its arguments (`type IB = Box[int]`: `IB.of("x")` is a type
error, and `IB[str](…)` is rejected). An alias of a scalar or a protocol is a type spelling only:
`type M = int` then `M(5)` is `unknown name 'M'`.

**Distinct types: a one-field struct.** `newtype` is an ordinary identifier. Use a one-field `struct`
with methods, as Rust's tuple struct `struct UserId(i64)`. Operators and protocols come from its methods.

```chezzi
struct UserId:
    id: int
    fn is_admin(self) -> bool:
        return self.id < 100

struct Meters:
    v: float
    fn add(self, other: Meters) -> Meters:
        return Meters(self.v + other.v)
```
