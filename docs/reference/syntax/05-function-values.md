### Function values

**Keyword arguments through a function VALUE (Swift-style labels).** A `fn(...)` type carries its
parameters' labels, so a binding that holds **one known function** accepts keyword arguments. Such a
binding is never written after it is made, and it is one of:

- an unannotated `g := some_fn` or `g := fn(...): ...` (a closure literal)
- a nested `fn`'s own name, or an alias of such a binding (`g := v` over a nested `fn v`, then `h := g`)
- a turbofish instantiation of a generic fn (`g := f[int]`, `g := lib.f[int]`)

A write to either name voids an alias.

```chezzi
fn greet(name: str, greeting: str):
    print(greeting, name)

g := greet
g(name="Bob", greeting="Hi")           # "Hi Bob" — by label, through a value
g(greeting="Hi", name="Bob")           # same — labels may be reordered
g("Bob", "Hi")                         # positional through the value still works

fn apply(f: fn(name: str) -> None):
    f(name="X")     # ERROR: keyword arguments through a function value need a binding that
                    # holds one known function (...); pass the arguments positionally
    f("X")          # fine — positional through any value
```

Any other callee takes no keyword arguments, and the call is a compile error that names the positional
workaround: a parameter, a list/map slot, a loop variable, a match binding, an annotated binding, a
returned value (`mk(a)(x=1)`), or a binding reassigned anywhere (even after the call, in a loop, or
from a nested fn; `h := f; h = f2` with identical labels too). Labels are surface-only (Swift SE-0111):
`fn(a: int, b: int)` and `fn(b: int, a: int)` are one type, so `fs := [f, ren]; fs[1](a=1, b=2)` could
bind by the wrong function's labels.

- `fn(str) -> None` and `fn(name: str) -> None` are the same type and assign both ways.
- A value call through a binding certain to hold one fn binds that fn's slots and fills every default
  it can, a hole before a supplied argument included: `h := f; h(1, c=9)` over `fn f(a, b=2, c=3)`
  passes `b=2` (CPython agrees).
- A default that names a type parameter is filled by the callee, so a value call may omit it only at
  the end: `h := f[int]; h(1, c=9)` over `fn f[T](a: T, b: List[T] = List[T](), c: int = 3)` is
  refused. Call the function by name for that shape.
- A certain binding takes named arguments only through its declaration's slots. A builtin takes
  exactly its keyword-only params by name (`p := print; p(1, 2, sep="-")`); `p := ord; p(c="a")`
  and `f := math.sqrt; f(x=4.0)` report `'p' takes no named arguments` and `'f' takes no named
  arguments`, as the direct calls do.
- A keyword call through a fn-typed struct field or a tuple slot (`h.f(x=4)`, `t.0(x=4)`) is refused:
  member-call syntax resolves as a method first. Bind it first (`g := h.f` then `g(x=4)`).
- A binding typed from `fn a(x: int = 1)` may be called with no argument, so a function that requires
  one cannot be assigned into it (`h := a; h = b` over `fn b(x: int)` is a type error). A defaulted
  function flows into a plain `fn(int) -> int` slot.
- Named arguments through a value evaluate in parameter-declaration order, and work in `defer`/`spawn`
  position (`defer d(name="Zoe")`). The checker rewrites the call to a positional one.
- Diagnostics print an omittable parameter as `int = …`: `[a, b]` over those two functions reports
  `fn(int = …) -> int vs fn(int) -> int`.

**A variadic is part of the fn type (Go's rule).** `fn(...T) -> R` is the type of a Chezzi variadic
(`fn(int, ...str) -> R` with a fixed param first), and `fn(T, ...) -> R` the type of a C variadic
(`extern`). The variadic part sits last. `fn(int, ...str) -> R` and `fn(int, List[str]) -> R` are
different types, and a call through any value of a variadic type packs its surplus positionals as a
direct call does. `print` is a `fn(...Any, str = …, str = …) -> None` (its defaulted `sep` and `end` let it fit a `fn(...Any) -> None` slot), so a `fn(Any) -> None` slot does not accept it.
Go rejects the same shape: `cannot use fmt.Println (value of type func(a ...any) (n int, err error))
as func(any) value in argument to ap`.

**An arity error names the callee as written:** `'h'` for a binding, `'k'` for a parameter,
`'closure'` for a callee with no name (`fs[0](1)`, `mk()(1)`). The message is the direct call's
message: `'h' expects 2 argument(s), got 1`.

**Parameter types are invariant.** `h: fn(Any) -> Dog = idd` over `fn idd(d: Dog) -> Dog` is a type
error, and so is `h: fn(int) -> str = wide` over `fn wide(a: Any) -> str` (Go and Rust agree). The
return type is covariant.

**A GENERIC fn as a value.** A generic function becomes a value once its type parameters are pinned,
by a turbofish or by a known concrete `fn(...) -> ...` type (an annotation, a HOF parameter, a return
position, an assignment target). The value has the substituted concrete type.

```chezzi
fn ident[T](x: T) -> T:
    return x

g := ident[int]                      # turbofish pins T=int  ⇒ g : fn(int) -> int
print(g(5) + 1)                      # 6
p := pair[str, int]                  # one type argument per parameter ⇒ p : fn(str, int) -> (str, int)

h: fn(int) -> int = ident            # annotation pins T=int
print(h(5) + 1)                      # 6

fn applyit(f: fn(int) -> int, x: int) -> int:
    return f(x)
print(applyit(ident, 5) + 1)         # 6 — HOF parameter pins T against the param type

print([1, 2, 3].map(ident))          # [1, 2, 3] — a builtin HOF slot fn(int) -> U also pins T=int

fn getf() -> fn(int) -> int:
    return ident                     # return position pins T against the declared return type
```

The pin is strict:
- an unsatisfiable target (`g: fn(str) -> int = ident`) is a type error
- a bound violation (`addone[str]` where `str` is not `Add`) is rejected
- a turbofish arity mismatch (`pair[int]` for `pair[A, B]`) is an error
- the value keeps its concrete type (`g := ident[int]` then `s: str = g(5)` is rejected)

A builtin container HOF slot pins like a user HOF: `[1,2,3].map(conv)` for `conv[T](x: T) -> str` is
`List[str]`, and `[1,2,3].fold(0, add)` for `add[T: Add]` pins `T=int` and enforces the bound. At
runtime the value is the underlying function (generics are erased), so an indirect call costs nothing extra.

**Path values (Rust's rule).** A type path is a value when it names something callable: a static
method, or an instance method through its type (the receiver is the first argument, keyword `self`),
at any type-argument arity, bare, imported or module-qualified. An alias head pins its own type
arguments; one that fixes them takes no more (`type BI = Bx[int]`: `BI[str].make`, `x: BI[str]` →
*already fixes its type arguments*). A call through a path applies its value (`P.get(p)`,
`(Bx[int].get)(b)`). A caller's type parameter pins a generic fn value like a concrete type: inside
`fn o[U]`, `h: fn(U) -> U = ident`, `ap(Bx.get, b)`, `?ident` at `(fn(U) -> U)?` and a default
`f: fn(U) -> U = ident` all check.

```chezzi
m := Bx[int].make                    # fn(int) -> Bx[int]
g := Bx[int].get   # g(b): the receiver is the first argument
type B = Bx[int]
m2 := B.make   # an alias head pins its args
```

Refused paths, each with its own message:
- a bound method `b.get`: `a bound method is not a value`
- a type `Bx[int]`: `'Bx' is a type, not a value — constructors are not values`
- a protocol method `Show.show`: `a protocol method is not a value`
- a native handle's method (`io.Reader.close`): `a native method is not a value`

**A type name is never a value.** A struct, an enum, a protocol, an alias of any type, a payload
variant and a builtin type name (`int`, `str`, `List`, `Shared`, `int32`, `Self`) are types. Rust and
CPython accept a constructor as a value; Chezzi does not. Call the constructor, or wrap it in a closure:

```chezzi
struct P:
    x: int
enum E:
    V(int)
    A
[1, 2].map(P)                  # error: 'P' is a type, not a value
[1, 2].map(E.V)                # error: 'E.V' is a type, not a value
[1, 2].map(str)                # error: 'str' is a type, not a value
[1, 2].map(fn(e): E.V(e))      # ok: [V(1), V(2)]
```

For a name with a constructor the message is `'E.V' is a type, not a value — constructors are not
values: call it (`E.V(…)`) or wrap it in a closure (`fn(e): E.V(e)`)`. These stay values: a
payload-less variant (`E.A`), a top-level fn, a method through its type (`P.mk`), a lambda, a nested fn.

**A bare generic fn value must be pinned on its own statement.** `g := ident` with no turbofish and no
expected type is an error where it is bound; a later `g(5)` does not pin it. Inside one statement it
solves through an argument (`apply(ident, 5)`), a typed binding (`f: fn(int) -> int = ident`), or a
join with a sibling: `if`/`elif`/`match` branches, `??`, a list/map literal, `==`, `in`, list `+`, an
inline `recover: expr`. `(o ?? g)(5)` with `o: (fn(int) -> int)?` pins `T = int`, and so does
`if c: inc else: g`.

```chezzi
fn main():
    g := ident    # 'ident' is generic and T is not determined here, so it cannot become a function
    print(g)      # value — instantiate it (`ident[<T>]`), or give this position a concrete function
                  # type (`fn(<T>) -> <T>`), writing a real type in place of each `<…>`
```

- The same error holds in a `[...]`/`{...}` literal, in a `return` from a fn with an inferred return
  type, and as a generic constructor / free fn argument (`b := Bx(ident)`,
  `print(take(ident))` on `fn take[U](f: U) -> int`).
- Two unpinned generics joined (`if c: g else: h`, `g == h`) stay rejected.
- A generic with two or more type parameters takes them all in one turbofish (`pair[str, int]`); the
  diagnostic offers `pair[<A>, <B>]`. Rank-N polymorphism is future work (Go and Rust refuse it too).

**Argument position asks the same question.** A bare generic fn passed to a HOF (a user one, a builtin
container HOF: `map`, `filter`, `fold`, `sort_by`, `sort_by_key`, `min_by`, `max_by`, `take_while`,
`drop_while`, `count`, `position`, `Shared.update`, `RwShared.write`; or a
`std.concurrency.collection` method: `ConcurrentList.for_each`, `ConcurrentMap.fold_entries`) gets the
same error when the slot cannot determine its type parameters.

- `[1,2,3].filter(pred)` for `pred[T](n: int) -> bool` and `[1,2,3].map(mk)` for
  `mk[T](n: int) -> List[T]` are errors (Go: `cannot infer T`).
- The check runs at the end of the call: `[1,2,3].fold(0, pick)` for `pick[T](a: T, b: T) -> T` is
  pinned by the accumulator. A user generic method may order the slots either way
  (`b.app(ident, 5)`, `b.app2(5, ident)`).
- An empty receiver suppresses only the report: `[].map(ident)` and `[].map(mk)` check and print `[]`.
  The pin still runs: `[].fold(0, add)` prints `0` (as Go's `Fold([]int{}, 0, add)`). A method's own
  type parameter left open is still reported: `Bx(0).two(ident, ident)` on
  `fn two[U](self, f: fn(U) -> U, g: fn(U) -> U) -> List[U]` is an error.

**The HOF may be generic itself.** The callee's type parameters are pinned first (by other arguments,
a turbofish or the annotation), then the passed fn unifies against the concrete slot. For
`fn applyg[U](f: fn(U) -> U, n: U) -> U`, `applyg(ident, 5)`, `applyr(5, ident)`,
`applyg[int](ident, 5)` and `xs: List[int] = mklist(ident)` all run, and
`fn twop[A, B](f: fn(A) -> B, a: A) -> B` infers `B` from the passed fn (`twop(text, 5)` is `str`).
Refused: `nopin(ident)` on `fn nopin[U](f: fn(U) -> U) -> int` (nothing determines `U`),
`twop(mk, 5)` (`B` only through `mk`'s open `T`), and `applyg(mk, 5)` (shapes do not match).

A parameter or field **default value** does not pass its concrete type to the read
(`fn run(f: fn(int) -> int = id)` is refused); write `= id[int]`. For an imported generic fn write
`= lib.idt[int]`, or `= idt[int]` after `import idt from lib`.

**Imported generic fns follow the same rule.** A from-imported (`import max from std.cmp`) or qualified
(`cmp.max`) generic fn pins from the expected type: `g: fn(int, int) -> int = cmp.max` and
`[1, 5, 3].fold(0, cmp.max)` work, and `f := cmp.max` alone is `'cmp.max' is generic and T is not
determined here` (Go: `cannot use generic function cmp.Max without instantiation`). Inside
`fn pick[T](a: T, b: T)`, `f := cmp.max; f(a, b)` is that error too.

**`?` inside a closure.** A closure body may use `?` (§9) only when the closure has an explicit
`-> …!`/`-> …?` return type. The `?` propagates to the closure's return, not the enclosing function.
Otherwise it is a type error.

```chezzi
fn parse(s: str) -> int!: ...
rs := ["2"].map(fn(s: str) -> int!: ?(parse(s)? * 2))   # ? lands in the closure's own return type
```
