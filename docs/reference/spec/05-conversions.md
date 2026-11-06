## Type conversions & casting

Chezzi has **no `as` cast operator** and no value-level conversion protocol; the
**bound-only** `From[S]` and `TryFrom[S]` protocols exist as generic bounds, and a type converts itself
with `x.into[T]()` or an annotated `t: T = x.into()` (below). A conversion is a method on the source value, display is interpolation, and there is no implicit `int`→`float` widening at a slot
(rule D3). A conversion is therefore always visible in the source.

**A call on a scalar type's name gives its zero value.** A constructor sets fields only, and a scalar
has none, so `int()` is `0`, `float()` is `0.0`, `str()` is `""`, `bool()` is `false` and `bytes()` is
`b''` (Python `int()` is `0`, Rust `i64::default()` is `0`). A call with an argument is a type error
whose help names the form below for the argument's type.

| Form | From → To | Failure |
|------|-----------|---------|
| `n.to_float()` | `int` → `float` | never fails; past 2^53 it rounds to the nearest float (Rust `n as f64`) |
| `f.to_int()` | `float` → `int?` | truncates toward zero; `None` for NaN, ±inf and a value outside i64 |
| `s.to_int()` / `s.to_float()` | `str` → `int!` / `float!` | an error that names the cause (trims first, PEP-515 underscores): `!"to_int: cannot parse 'x' as int"`, `!"to_int: '99999999999999999999' is out of range for int"`, `!"to_float: cannot parse 'x' as float"` |
| `n.to_be_bytes(len, signed)` / `n.to_le_bytes(…)` | `int` → `bytes!` | `len` is 1 to 8; `signed` defaults to `false` (Python `to_bytes`); a negative unsigned value, a value that does not fit and a bad `len` are errors that name the cause |
| `int.from_be_bytes(b, signed)` / `int.from_le_bytes(…)` | `bytes` → `int!` | 0 to 8 bytes (`b""` is `0`); more than 8 bytes, or an unsigned 8-byte value above i64::MAX, is an error |
| `int.from(b)` / `float.from(b)` | `bool` → `int` / `float` | never fails: `true` is `1` / `1.0`, `false` is `0` / `0.0` (Rust `i64::from(true)`) |
| `int.try_from(s)` / `float.try_from(s)` | `str` → `int!` / `float!` | the parse of `s.to_int()` / `s.to_float()`, same errors (Rust `str::parse`) |
| `"{x}"` | **anything** → `str` | never fails: the `Stringable` display (`print` uses the same path, Go `fmt.Sprint`) |
| `ord(s)` / `chr(n)` | `str` ↔ codepoint `int` | narrow, single-purpose |

There is no `bool` conversion (Go, Rust): test the value with `x != 0` or `s.len() > 0` (`if 0:`
is already a type error). A `bool` is not a number (`true + 1` is a type error); convert it with
`int.from(b)` or `float.from(b)`. There is no `float.from(int)`: an `int` past 2^53 does not
round-trip (rustc: `f64: From<i64>` is E0277); write `n.to_float()`. There is no `str.from(x)`:
write `"{x}"`.

| Old call | Type error help |
|----------|-----------------|
| `int(x)` on a `float` / a `str` | ``convert with `x.to_int()`, which gives int?`` / `int!` |
| `float(x)` on an `int` / a `str` | ``convert with `x.to_float()`, which gives float`` / `float!` |
| `int(x)` / `float(x)` on a `bool` | ``a bool is not a number; write `int.from(x)` `` (`float.from(x)` for `float`) |
| `str(x)` | ``render the value with `"{x}"` `` |
| `bool(x)` | ``test the value: write `x != 0` `` for a number, `` `x != None` `` for an optional, `` `x.len() > 0` `` for a type with `len` |
| `T(x)` where `x` is already a `T` | `the argument is already an int` (and so on) |
| any other argument | `List[int] does not convert to int` (and so on) |

**No implicit `int` → `float` conversion at a slot** (rule D3): write `1.0`, or `x.to_float()` for a
typed value (Rust's `let x: f64 = 1;` is E0308 too). The reverse (`float` → `int`) is a lossy type error
as well. Arithmetic and comparison on VALUES still promote by runtime tag (`2 * 1.5` is `3.0`, `i < f`).
Full rules: [Numeric semantics](04-semantics.md#numeric-semantics).

**Stringable:** every scalar (`int`/`float`/`bool`/`str`) intrinsically satisfies `Stringable`, so
`[T: Stringable]` generics accept them (an erased `v.str()` body dispatches to the same native
`stringify` that `"{x}"` uses). Structs/enums opt in with their own `str(self) -> str`.

**Intrinsic conformance implies a callable method.** Where a built-in satisfies a protocol
*intrinsically* (no user method), the protocol's method is callable on it inside an erased generic body
or through a protocol-typed value, and it is defined as **exactly** the operator/primitive form:
- `a.add(b)` ≡ `a + b` (same overflow / divide-by-zero faults, same int↔float promotion), `a.neg()` ≡
  `-a`, and `a.compare(b)` is what `<` orders by.
- `a.eq(b)` ≡ `a == b` for a struct/enum: a struct/enum that defines `fn eq(self, other: Self) -> bool`
  OWNS its `==`. An `eq` with a GENERIC operand is an ordinary method that the operator leaves alone,
  but only on a CONCRETE receiver: **through a protocol bound, `a.eq(b)` is always the protocol's
  equality**, so `≡ a == b` holds there unconditionally (as rustc resolves `a.eq(b)` under `T: Eq` to
  `<T as PartialEq>::eq`, not to an inherent same-named method).
- `c.index(k)` ≡ `c[k]`, `c.set_index(k, v)` ≡ `c[k] = v` (returns `None`), `c.slice(r)` ≡ `c[r]`
  (`r` is a `Range`).
- `x.hash()` is exactly the hash `x` gets as a map/set key. Its numeric value is **unspecified** (a
  build-dependent 64-bit hash, possibly negative); only consistency is guaranteed: equal values hash
  equally, and it agrees with container membership.

The intrinsic grants and their methods are: `Comparable`→`compare`, `Eq`→`eq`, `Stringable`→`str`,
`Hashable`→`hash`, `Error`→`message`, `Iterable`→`iter`, `Index`→`index`, `IndexSet`→`set_index` (it embeds `Index`),
`Slice`→`slice`, `Add`/`Sub`/`Mul`/`Div`/`Mod`→`add`/`sub`/`mul`/`div`/`mod`, `Neg`→`neg`,
`BitAnd`/`BitOr`/`BitXor`/`Shl`/`Shr`→`bitand`/`bitor`/`bitxor`/`shl`/`shr`, `Contains`→`contains`.
**An operator accepts a builtin type exactly when that type satisfies the operator's protocol:**
`"a" + "b"`, `[1] + [2]`, `b"a" + b"b"` ask `Add`; `{1} - {2}` asks `Sub`; `{1} | {2}` asks `BitOr`;
`98 in b"abc"` asks `Contains`; every value satisfies `Stringable`. A type that DEFINES the method always
gets its own (intrinsic dispatch is a resolution fallback, never a shadow).

`line()`/`col()`/`file()` are callable through the `Error` existential but are **not** `Error`
requirements; only `message()` is. (As requirements they would un-satisfy every struct error type that
has only `message()`.)

**A NATIVE-TABLE witness for a USER protocol.** A built-in (`List`/`Map`/`Set`/`str`/`bytes`/
`bytearray`) that satisfies a user-declared protocol does not use the intrinsic-grant table (a user
protocol's name cannot key a row). The witness is a REAL native method that the direct-call path
already type-checks and the VM dispatches by name (e.g. `List.len`, `List.sort`). The checker reuses
that method's signature (receiver prepended) and enforces the SAME gates as a direct call (`List.sum`
needs a numeric element; `List.sort` enforces its own `where T: Comparable`; a method with its own `[U]`
type params, like `List.map`, cannot witness at all). An erased or protocol-typed call lowers to the
same by-name dispatch as a concrete call.

`a.compare(b)` on a **NaN** operand never faults: it answers the **total order** of `sort()` /
`sort_by_key` / `.min()` / `.max()` (`f64::total_cmp`, NaN at one end), while `<`/`<=`/`>`/`>=` stay IEEE
(every NaN comparison is `false`). The method uses the total order, the operators use IEEE.
`a.compare(a)` is `0` for a NaN `a` although `a == a` is `false`, and a `±0.0` pair compares Equal by the
method and by `sort()`/`min`/`max` alike (CPython keeps `sorted([0.0, -0.0])` as `[0.0, -0.0]`).

**Exception to the equivalence:** `Iterator`'s `next` on a *raw* collection is granted but faults (a raw
collection holds no cursor position); iterate it with `for`, or call `.iter()` for a real cursor. See
[`gaps.md`](../../contributing/gaps.md).

**What does NOT exist:**

- No `as` operator (`as` in the grammar is only import aliasing).
- No value-level conversion protocol. `From[S]` and `TryFrom[S]` are **bound-only** (below):
  `c: From[int]` as an annotation is rejected, since a value cannot invoke a static ctor. `Into[T]`
  and `TryInto[T]` are not bounds yet: `[S: Into[float]]` is *type Rat does not satisfy Into*.
- `cast[T](val: Any) -> T?` (a checked downcast off `Any`) is **deferred**: it needs runtime type tags,
  because generics are erased ([`roadmap.md`](../../design/roadmap.md)).

**`From[S]`: bound-only conversion protocol.** A reserved builtin, structural, target-keyed
protocol. A type **witnesses** `From[S]` by declaring a **static** method `fn from(x: S) -> Self`
(no `self` receiver); an instance `from(self, …)` does NOT witness it.
- It is usable **only as a generic bound** `[T: From[S]]`. It is rejected as a value-annotation type
  (param/field/return/binding, also nested `List[From[int]]`/`…?`/tuple, a type alias, and a
  protocol that *embeds* a static-ctor protocol), by the static-slot rule of any static-ctor protocol.
- **`T.from(x)` through the bound** works by **witness passing**: a type parameter whose bound
  carries a static requirement, and whose body needs it, gets one hidden trailing parameter per static
  requirement of its bounds, holding the concrete type's runtime identity key and the method that
  meets the requirement there. The CALLER fills it, because the caller knows `T` and `S` (Rust picks
  the `impl From<S>` at the call the same way), so `T.from(n)` dispatches to the real type and the
  right overload while generics stay erased (nothing monomorphized; no type argument reaches the VM).
  A generic caller forwards its own witness for the same requirement. `[T: From[int] +
  From[str]]` is two witnesses, and the argument's type picks one at `T.from(x)`. The same
  mechanism serves `T.default()` on any user protocol with a static requirement.
- It does not work for: a type parameter of the enclosing TYPE, a witness-taking fn read as a function
  VALUE, and an undetermined `T`. A `spawn`/`defer` statement target is an ordinary call site. Surface:
  `syntax.md §7a`.
- Direct `Type.from(x)` needs no protocol and is the right spelling when the type is known. See
  [`roadmap.md` §3](../../design/roadmap.md).
- **`TryFrom[S]`** is the fallible twin (Rust `TryFrom`): a type witnesses it by a static
  `fn try_from(x: S) -> Self!` (or `Self!E`), called `T.try_from(x)`. A `from` returns `Self`, so a
  conversion that can fail is a `try_from`; `[T: From[str]]` never reaches a `Self!` method.
- **The builtins `int` and `float` witness both** with the statics their prelude decls declare:
  `lift[int](true)` for `fn lift[T: From[bool]](b: bool) -> T`, and `parse[int]("7")` for
  `fn parse[T: TryFrom[str]](s: str) -> T!`. A scalar's witness key is its bare name (`int`); a
  user type's key is `<module>::Name`, so a user `struct int` never collides.

**Several `from`s: overloads.** A struct or enum may declare one static `from` per source type,
as Rust's several `impl From<S>`, and one static `try_from` per source type, as several
`impl TryFrom<S>`. The module that declares them imports the protocol that keys them (as Rust's
`use std::convert::From` brings `From` into scope); a caller needs no import. The argument's type
picks the overload:

```chezzi
import From from std.convert

struct Rat:
    n: int
    d: int
    fn from(x: int) -> Rat:
        return Rat(x, 1)
    fn from(x: str) -> Rat:
        return Rat(x.len(), 3)

a := Rat.from(3)       # the int overload: Rat(3, 1)
b := Rat.from("ab")    # the str overload: Rat(2, 3)
```

- `Type.from(x)` runs the one overload whose parameter takes the type of `x` (an `int?` overload
  takes `3` and `None`). There is no widening: `Rat.from(1.5)` is *type Rat has no from for float
  (it has: int, str)*. An argument that fits two overloads (`[]` against `List[int]` and `List[str]`)
  is *type annotations needed*.
- `Rat.from[S](x)` names the source instead: `Rat.from[str]("ab")` runs the `str` overload, on a
  set or on one `from`, and the argument is then checked against it. On a generic type the `[S]`
  fills the type's param: `Box.from[int](3)` on `fn from(x: T) -> Box[T]` is a `Box[int]`. `Rat.from[float](3)` is *type
  Rat has no from for float (it has: int, str)*. The `[S]` is the protocol slot, not a method type
  argument; on a plain method (no protocol keys it) a `[X]` stays *method 'make' takes no type
  argument(s)*.
- Each overload takes one parameter named `x`, of a concrete type (not a type parameter), and declares
  its return type: `Self` for `from`, `Self!E` for `try_from`. Any other is *an overloaded from
  returns Self, found int*; a fallible `from` is *an overloaded from returns Self, found Rat!str; a
  fallible conversion is try_from*, and *an overloaded try_from returns Self!E, found Rat*.
- A type may declare both `from` and `try_from` for one source: Chezzi has no blanket impl, so the
  two never clash (Rust's `E0119` comes from its blanket `TryFrom` of a `From`).
- Two overloads may not overlap, that is, one value may not fit both: `int` and `int?` overlap (`3`),
  `int?` and `str?` overlap (`None`), `int!str` and `int!int` overlap (`3` wraps into both), a
  protocol overlaps a type that satisfies it (also as `Sq?`), and two protocols always overlap. The error names the first one: *conflicting from for int: from(x: int) at 3:8*.
- A repeated method name overloads when a parameterized protocol the declaring module declares or
  imports keys it ([protocols](../syntax/09-protocols.md#a-parameterized-protocol-keys-overloads)):
  `From[S]`'s `from` and `TryFrom[S]`'s `try_from` by the parameter, `Into[T]`'s `into` and
  `TryInto[T]`'s `try_into` by the return, a user `Parse[S]`'s `parse(x: S)` the same way. With no
  such protocol in scope (`from` overloads without `import From from std.convert`), any other
  repeated method name, an instance `from(self, …)` declared twice and a static `into(x: …)`
  declared twice are *method '…' is already defined*.
- A trace, `<fn …>` and every message call an overload `from`.
- A free `S` in `[S, T: From[S]]` is not inferred from an overloaded type: pin it.
- A bound picks too, by the direct call's rule: `fn make[S, T: From[S]](x: S) -> T` called as
  `r: Rat = make("ab")` runs the `str` overload, and `make([])` on a type with `str` and `List[int]`
  overloads runs the `List[int]` one (Rust's `make(vec![])` infers `Vec<i32>` the same way). A source
  that fits two overloads (`[]` against `List[int]` and `List[str]`) is *type Rat does not satisfy
  From (type annotations needed: …)*. The declaration order never matters.
- As a value, `Rat.from` names several fns, so its position's fn type picks one:
  `f: fn(int) -> Rat = Rat.from` is the `int` overload (an annotated binding, a parameter, a
  return, a typed list element), and so does a generic slot's param type: `[1, 2].map(Rat.from)`
  is the `int` overload. With no fn type there, `g := Rat.from` and
  `print(Rat.from)` are *type annotations needed: Rat.from has 2 overloads (int, str)*, as
  Rust's `let g = Rat::from;` is `E0283`. A type with one `from` reads as a value as before.

**`into`: a conversion on the source.** A struct or enum converts ITSELF with an instance
`fn into(self) -> T` (Rust `impl Into<T> for Rat`), one per target type, and with
`fn try_into(self) -> T!` (Rust `impl TryInto<T>`) when the conversion can fail. The protocols
`Into[T]` and `TryInto[T]` in `std.convert` give the shape, and the declaring module imports them. A
call on an overload set names the target:

```chezzi
import Into, TryInto from std.convert

struct Rat:
    n: int
    d: int
    fn into(self) -> float:
        return self.n.to_float() / self.d.to_float()
    fn into(self) -> str:
        return "{self.n}/{self.d}"
    fn try_into(self) -> int!:
        if self.d != 1:
            return !"{self.n}/{self.d} is not an int"
        return self.n

r := Rat(1, 2)
x := r.into[float]()       # 0.5
s := r.into[str]()         # "1/2"
n := r.try_into[int]()     # an error value: 1/2 is not an int
```

- Two or more `into`s are an overload set. The `[T]` picks the overload whose return is `T`, exactly
  (Rust's `Into<T>`). With no `[T]`, the annotation picks (Rust `let f: f64 = r.into()`):
  `f: float = r.into()`, and a carrier annotation picks a `try_into` by its payload
  (`n: int! = r.try_into()`). `[T]` wins over the annotation, so `x: int = r.into[str]()` is the
  assignment's *cannot assign str to variable of type int*. With neither, `x := r.into()` is *into
  on Rat needs a target type: write `.into[T]()`* (no message names the receiver); a target with
  no overload is *type Rat has no into for int (it has: float, str)*, the one error of the call.
  `Rat.into[float](r)` (or `s: str = Rat.into(r)`) is the same call through the type.
- One `into` is a plain method, as one `from`: `r.into()` calls it, `[r].map(Rat.into)` reads it, it
  satisfies a protocol that requires `fn into(self) -> float`, and it may take more params. A `[T]`
  on it is the same pick: `r.into[float]()` works when it returns `float`, and `r.into[int]()` is
  *type Rat has no into for int (it has: float)*. The receiver's type args are substituted first:
  `Box(7).into[int]()` on `struct Box[T]`'s `fn into(self) -> T` returns `int`.
- Each `into` of an overload set takes only `self` and returns a concrete type (not a type
  parameter): `T` for `into`, `T!E` for `try_into`. A `float!` `into` is *an overloaded into returns
  T, found float!; a fallible conversion is try_into*. Two `into`s with one target are *conflicting into for float: into(self) ->
  float at 3:8*. A static `fn into(x: …)` is an ordinary static method, and two of them are
  *already defined*.
- There is no blanket `into` from a `from` (Chezzi has no blanket impls): `r.into[Big]()` finds only
  a written `into`, even when `Big.from(x: Rat)` exists; call `Big.from(r)`.
- An overloaded `into` is not a method value (`r.into` is rejected, as every bound method) and does
  not witness a protocol requirement named `into`: the call site picks the target.
- In a trace, `<fn …>` and every message an overload is called `into`.

**Several owners of one conversion pair.** `B.from(x: A)`, `B.try_from(x: A)`, `A.into -> B` and
`A.try_into -> B!` may all be declared: each is its own method, called by its own spelling. Rust
forbids `impl Into<B> for A` beside `impl From<A> for B` only because its blanket `Into` of a `From`
would collide (`E0119`); Chezzi has no blanket impl.

| From `A` to `B` | Fallible | Spelled | Called |
|---|---|---|---|
| on the target `B` | no | `fn from(x: A) -> Self` | `B.from(a)`, `[T: From[A]]` |
| on the target `B` | yes | `fn try_from(x: A) -> Self!` | `B.try_from(a)`, `[T: TryFrom[A]]` |
| on the source `A` | no | `fn into(self) -> B` | `a.into[B]()` |
| on the source `A` | yes | `fn try_into(self) -> B!` | `a.try_into[B]()` |
