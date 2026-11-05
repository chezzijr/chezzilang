## 7. Structs

Type declarations (`struct`, `enum`, `protocol`, `type`) are top-level only. A nested type is a parse
error (Python accepts a `class` inside a function).

```chezzi
struct Point:
    x: int
    y: int

    fn dist(self) -> float:           # method: first param is 'self'
        return math.sqrt((self.x*self.x + self.y*self.y).to_float())   # needs: import std.math

p := Point(3, 4)        # construct positionally
print(p.x)              # field access
print(p.dist())         # method call
```

No inheritance (by design). Composition only.

**Struct instances are REFERENCE values (Python's object model, not Go's).** `q := p` binds the same
instance, so a write through either name is visible through both, and a callee can mutate a struct
passed to it. The same holds for a struct in a `List` or as a `Map` value:

```chezzi
q := p
q.x = 99
print(p.x)              # 99  — one object, two names (CPython prints 99; Go would print 3)
```

Every struct has an intrinsic `copy()` that returns a **shallow** duplicate (Python's `copy.copy`;
there is no deep copy). A `List`/`Map`/`Set`, nested-struct or handle field is shared by both structs
afterwards. A struct that declares its own `copy` method, or holds a fn-typed `copy` field, keeps it. A
struct holding a `Channel`, `Shared` or `Socket` copies and shares the handle. `List`, `Map` and `Set`
have a shallow `copy()` too (see [`stdlib.md`](../stdlib/01-builtins.md) §2).

```chezzi
struct P:
    x: int
    tag: str = "none"

p := P(1, "hi")
print(P(p.x))            # P(x=1, tag='none')  -- a hand-written duplicate drops the field
print(p.copy())           # P(x=1, tag='hi')    -- copy() carries every field

q := p.copy()
q.x = 99
print(p.x)                # 1  -- independent instances
```

A struct is snapshotted only as a `Map`/`Set` **key** or a `Set` **element** (see [Hashing](10-protocol-hooks.md#hashing-hashable)). A `spawn:`/`parallel:` boundary copies nothing: a task gets the struct by handle (M3.4, §11b).

**A bound method is not a value; a method named through its type is** (Rust's path rule). Reading
`p.dist` is a check-time error (Rust E0615): `type Point has no field 'dist' ('dist' is a method — a
bound method is not a value: …)`. `d := Point.dist` is a fn that takes the receiver first (`d(p)` or
`d(self=p)`); a static method `Point.origin` is a fn too. Or wrap it: `f := fn(): p.dist()`. A
fn-typed field (`f: fn(int) -> int`) is a value: `s.f(3)` and `g := s.f; g(3)` work. See "Path
values" in §5.

### 7a. Static (associated) methods — the "no self ⇒ static" rule

A method's first parameter decides its call shape:

- named `self`: an **instance method**, called `value.method(args)`. It is also callable through its
  type, receiver first (`P.get(p)`, `Bx[int].put[str](b, "s")`, `E.tag(e)`), as Rust's
  `Type::method(&v)`; a call with no receiver is an arity error (`'get' expects …`). A native handle's
  method is not (`io.Reader.close()` → *call it on a value*).
- not `self`, or no parameters: a **static method**, called `Type.method(args)`. `value.method` on it
  is an error that points at the other form.

Static methods add to the positional constructor `Name(...)`. Use them for named, alternative and
validating constructors (returning `T!E` / `T?`):

```chezzi
struct Rect:
    w: int
    h: int
    fn square(s: int) -> Rect:        # static: first param is not `self`
        return Rect(s, s)
    fn area(self) -> int:             # ordinary instance method coexists
        return self.w * self.h

struct Email:
    addr: str
    fn parse(s: str) -> Email!str:   # validating ctor
        if "@" in s:
            return ?Email(s)
        return !"missing @"

r := Rect.square(5)        # Type.method(args) — static call
print(r.area())            # 25
match Email.parse("a@b"):
    ?e: print(e.addr)
    !m: print(m)
```

**`Type(...)` only sets the fields; a constructor with logic is a static method** (`Type.new(...)`,
never a module `fn` of the type's name: a module binds each name once, a type included, so
`fn Path` beside `struct Path` is `'Path' is already defined`):

```chezzi
import PathLike from std.path

struct Path:
    raw: bytes

    fn new(p: PathLike) -> Path:   # static: called Path.new(...)
        return Path(p.as_path())   # Path(...) sets the field

p := Path.new("/a/b")              # normalizes any PathLike
q := Path(b"/a/b")                 # sets the field
```

- A nested fn named after a same-module struct shadows it only in its scope (§3).

**Enums** get static methods too (for example `from_str(s) -> Color?`). A variant and a static method
may not share a name (a declaration-time error), so `Color.Red` is always the variant.

A type declares each method name once (`method 'show' is already defined`). The one exception is a
static `convert`: one per source type, picked by the argument's type (`Rat.convert(3)`,
`Rat.convert("1/3")`). The rules are in [`05-conversions.md`](../spec/05-conversions.md).

**Generic static methods** take a **type-level turbofish** (the type's args sit on the type):

```chezzi
struct Box[T]:
    items: List[T]
    fn empty() -> Box[T]:
        return Box([])

b := Box[int].empty()      # turbofish on the TYPE: Box[int].empty()
```

- Multi-param types use the comma form (`Pair[K, V].empty()`).
- A module-qualified base takes it too: `shapes.Tree[int].Leaf(9)` (variant ctor),
  `shapes.Box[int].make(5)` (static method), `shapes.Box[int].make[str]("hi")`, and
  `shapes.Pair[int, str].X`.

A static method may declare its own `[U]` on the member (a type argument is written where its
parameter is declared). Member args are inferred from the call; a member turbofish pins them when they
cannot be (`Box.make[str]("hi")`). The two compose:

```chezzi
struct Box[T]:
    val: T
    fn make[U](x: U) -> Box[U]:       # member declares its own [U]
        return Box(x)

a := Box[int].make(5)                 # U inferred from the arg ⇒ Box[int]
b := Box[int].make[str]("hi")         # combined turbofish: T=int + U=str ⇒ Box[str]
```

- A method type param may not shadow an enclosing one (`fn make[T]` inside `Box[T]` is an error).
- A member turbofish on a member with no type params (or a builtin like `xs.len[int]()` /
  `xs.iter[int]()`) is an arity error.
- Instance methods take the member turbofish, multi-arg: `pair.first[int, str](1, "x")`.

#### Static protocol requirements — calling `T.method(...)` through a bound

A protocol may require a **static** method. A type witnesses it with a static method (an instance
`default(self)` does not), and a generic bounded by the protocol may call it through the type
parameter, to construct a `T` it was never handed:

```chezzi
protocol Default:
    fn default() -> Self          # STATIC requirement: no `self` receiver

struct Counter:
    n: int
    fn default() -> Counter:
        return Counter(7)

fn reset[T: Default](old: T) -> T:
    return T.default()            # dispatches to the CONCRETE type

print(reset(Counter(1)).n)        # 7
```

**How it works.** Generics stay erased (one body per generic fn). A type parameter whose bound has a
static requirement, in a body that needs it, gets a **hidden trailing parameter** that holds the
concrete type's runtime key; `T.method(...)` dispatches on that key like `Type.method(...)`. A generic
that only *has* such a bound (`fn tagged[T: Spawnable](x: T) -> str: return x.tag()`) is unchanged.

**What works:**
- a struct or an enum host
- same-module and cross-module calls in every import spelling (`m.reset(x)`, `import reset from m`,
  `import reset as again from m`)
- a `T` inferred from an argument, pinned by turbofish (`empty[Counter]()`), or fixed by an annotated
  result (`c: Counter = empty()`)
- forwarding an abstract `T` into another generic, transitively, recursively and mutually
- a type parameter declared by a member, instance or static, on a plain or generic host
  (`h.make(x)`, `Holder.build(x)`, `h.make[Counter](x)`)
- the call inside a closure (also one that escapes), a nested `fn`, `defer:`, and `spawn:`/`parallel:`
  (the witness is a value, so it crosses)
- the reserved `Convert[S]` protocol, whose witness is a static `convert(x: S) -> Self`:

```chezzi
import Convert from std.convert

fn make[T: Convert[int]](seed: T, n: int) -> T:
    return T.convert(n)
```

**What does not** (each has a diagnostic that names the workaround):

| shape | why | do this instead |
|---|---|---|
| `T` declared by the enclosing **TYPE** (`struct Bx[T: Default]` … `T.default()` in a method) | the concrete type is erased once a `Bx` *value* exists — only a value could hold the witness | declare the parameter on the **member** (`fn fresh[T: Default](self, …)`), whose witness rides on the call |
| reading a witness-taking fn as a **function value** — `g := reset`, `reset[Counter]` as a value, passing it to a HOF, a cross-module read | a `fn` value erases which declaration it came from, so no witness can be recovered. **A permanent wall, not a v1 limit** | call it directly, or take a factory closure: `fn make[T](mk: fn() -> T) -> T` |
| a `T` **not determined** at the call site | there is no concrete type to build a key from | pin it (`nodet[Counter]()`) or annotate the result |
| a bound witnessed by a **scalar** | it cannot host a static method | use a struct or an enum |
| a **manifest entrypoint** that takes a witness or any declared parameter (`entrypoint = "src.main:main"` where `fn main[T: Default]()` or `fn main(a: int)`) | it is invoked with no arguments, so nothing supplies the key (or the argument) | give the entrypoint a nullary, non-generic signature and construct / read inputs in a helper it calls. Reported by `chezzi check` and by bare `chezzi run`; an explicit `chezzi run <file>` is script mode and runs the top level regardless |

The turbofish call `reset[Counter](Counter(1))` works; only reading `reset[Counter]` as a value fails.

**When the hidden parameter is charged.** A body pays only if it names `T` in expression position
(`T.default()`) or forwards its abstract `T` to a witness-taking fn.

- A call with all-concrete arguments forwards nothing
  (`fn concrete[T: Default](x: T) -> int: return reset(Counter(1)).n`). Naming a module is not a call
  (`lib.plain(1)` costs nothing, `lib.reset(x)` charges).
- Anything not provably concrete charges: a zero-argument call, a local, a turbofish naming `T`, a
  generic constructor head.
- A **member** forward (`h.build[T](x)`, or `h.build(x)` with `x: T`) charges when both hold: the call
  site carries something of the enclosing fn's (a type argument naming its type param, or an argument
  whose parameter annotation mentions it, at any depth: `h.build(xs[0])` with `xs: List[T]`), and the
  method name is declared witness-taking somewhere in the program. So `p.build(1)`, `m.get("a")` and
  `xs.push(x)` on a `List[T]` cost nothing.
- Refused: a member forward whose `T` reaches the argument through a local (`v := x` then
  `h.build(v)`). Give `T` a place in the signature, spell the member turbofish (`h.build[T](v)`,
  always charged), or call `T.default()`.
- An explicit turbofish always charges, even when `T` is in neither a parameter type nor the return
  type (`h.build[T](v)`, `empty[T]()`). That fn's callers must then pin `T` (`outer[SomeType](...)`).

**A type parameter shadows a same-named declaration in its whole body**, in every position: the
annotation `x: Item`, `Item.tag()`, `Item[int].tag()`, `Item(99)`, `Item.Red`, and a same-named
function `foo()`. This is Go's rule (`func fv[foo any](x foo) int { return foo() }` → *missing argument
in conversion to foo*). So `fn f[Item: Tagged](x: Item) -> int: return Item.tag()` beside a
`struct Item` dispatches on the argument's type.

- A type parameter has no constructor and takes no type arguments.
- Through it you reach its bounds' methods: an instance method as a value or a call (`g := T.get`,
  `T.get(v)`, `ap(T.get, v)`, dispatched on the receiver's runtime type, so it works in a method of
  `struct Bx[T: Getter]` too), and a static method only as a call (`T.mk()`).
- A real local binding wins over the parameter. Rename the type parameter to reach the outer declaration.

Protocol requirements match static-to-static and instance-to-instance, never across. Demo:
[`examples/static_witness.chz`](../../../examples/static_witness.chz).

**Uniform parse rule (any receiver).** `recv.name[X](args)` is a method turbofish on any receiver:

```chezzi
W(1).cast[str]("a")          # call-result receiver
mk().cast[str]("a")          # factory-result receiver
h.w.cast[str]("a")           # field receiver
xs[0].cast[str]("a")         # index receiver
W(1).cast[Map[str, int]](m)  # nested-generic type arg, non-bare receiver
```

The parser keeps both readings of a one-arg `head[k](args)` and the checker chooses, as Go and CPython do:

- A head that is **data** reads index-then-call: a local or global name (`fs[k](10)`), a struct field
  with no method of that name (`h.fs[k](10)`, `hs[0].fs[k](10)`), a module global that is not a fn
  (`lib.fs[k](10)`).
- A fn-like path or a method reads as a turbofish (`idt[int](3)`, `W(1).cast[str]("a")`).
- A fn-typed field is data, so `h.f[int](3)` reports *cannot index into* (Go: `cannot index h.f`).
- A numeric index (`arr[0].handlers[0](20)`) is always index-then-call, and a subscript with no call
  (`obj.items[0]`, `m.data[k]`) is always an index.

**One bracket, both readings, the head decides.** Without a call, the parser reads `head[X…]` as a type
and as an expression. A generic fn or a type head takes type arguments; any value indexes. So every
type spelling works as a type argument in value position, and a parenthesised path is the path value
applied (Rust's `(i64::abs)(-6)`):

```chezzi
g := idt[(int, str)]            # tuple type argument
h := idt[fn(int) -> int]        # fn type argument
d := json.decode[Map[str, int]] # nested generic
print((math.abs[int])(-6))      # 6
K := 1
print(fs[K](10))                # a value head indexes: fs[1](10)
```

A type-only bracket on a value is rejected: `xs[fn(int) -> int]` reports *a subscript takes an
expression, found the type 'fn(int) -> int'*, and `xs[int, str]` *a subscript takes one index, found
2* (Go: `more than one index`).
