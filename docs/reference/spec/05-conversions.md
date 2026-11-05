## Type conversions & casting

Chezzi has **no `as` cast operator**, no `Into`/`TryFrom`, and no value-level conversion protocol; a
**bound-only** `Convert[S]` protocol exists as a generic bound (below). A conversion is a method on the
source value, display is interpolation, and there is no implicit `int`→`float` widening at a slot
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
| `"{x}"` | **anything** → `str` | never fails: the `Stringable` display (`print` uses the same path, Go `fmt.Sprint`) |
| `ord(s)` / `chr(n)` | `str` ↔ codepoint `int` | narrow, single-purpose |

There is no `bool` conversion (Go, Rust): test the value with `x != 0` or `s.len() > 0` (`if 0:`
is already a type error). A `bool` does not convert to a number either; write `if b: 1 else: 0`.

| Old call | Type error help |
|----------|-----------------|
| `int(x)` on a `float` / a `str` | ``convert with `x.to_int()`, which gives int?`` / `int!` |
| `float(x)` on an `int` / a `str` | ``convert with `x.to_float()`, which gives float`` / `float!` |
| `int(x)` / `float(x)` on a `bool` | ``a bool is not a number; write `if x: 1 else: 0` `` (`1.0`, `0.0` for `float`) |
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
- No `Into`/`TryFrom`, and no value-level conversion protocol. `Convert[S]` is **bound-only** (below):
  `c: Convert[int]` as an annotation is rejected, since a value cannot invoke a static ctor.
- `cast[T](val: Any) -> T?` (a checked downcast off `Any`) is **deferred**: it needs runtime type tags,
  because generics are erased ([`roadmap.md`](../../design/roadmap.md)).

**`Convert[S]`: bound-only conversion protocol.** A reserved builtin, structural, target-keyed
protocol. A type **witnesses** `Convert[S]` by declaring a **static** method `fn convert(x: S) -> Self`
(no `self` receiver); an instance `convert(self, …)` does NOT witness it.
- It is usable **only as a generic bound** `[T: Convert[S]]`. It is rejected as a value-annotation type
  (param/field/return/binding, also nested `List[Convert[int]]`/`…?`/tuple, a type alias, and a
  protocol that *embeds* a static-ctor protocol), by the static-slot rule of any static-ctor protocol.
- **`T.convert(x)` through the bound** works by **witness passing**: a type parameter whose bound
  carries a static requirement, and whose body needs it, gets one hidden trailing parameter per static
  requirement of its bounds, holding the concrete type's runtime identity key and the method that
  meets the requirement there. The CALLER fills it, because the caller knows `T` and `S` (Rust picks
  the `impl From<S>` at the call the same way), so `T.convert(n)` dispatches to the real type and the
  right overload while generics stay erased (nothing monomorphized; no type argument reaches the VM).
  A generic caller forwards its own witness for the same requirement. `[T: Convert[int] +
  Convert[str]]` is two witnesses, and the argument's type picks one at `T.convert(x)`. The same
  mechanism serves `T.default()` on any user protocol with a static requirement.
- It does not work for: a type parameter of the enclosing TYPE, a witness-taking fn read as a function
  VALUE, and an undetermined `T`. A `spawn`/`defer` statement target is an ordinary call site. Surface:
  `syntax.md §7a`.
- Direct `Type.convert(x)` needs no protocol and is the right spelling when the type is known. See
  [`roadmap.md` §3](../../design/roadmap.md).

**Several `convert`s: overloads.** A struct or enum may declare one static `convert` per source type,
as Rust's several `impl From<S>`. The argument's type picks the overload:

```chezzi
struct Rat:
    n: int
    d: int
    fn convert(x: int) -> Rat:
        return Rat(x, 1)
    fn convert(x: str) -> Rat:
        return Rat(x.len(), 3)

a := Rat.convert(3)       # the int overload: Rat(3, 1)
b := Rat.convert("ab")    # the str overload: Rat(2, 3)
```

- `Type.convert(x)` runs the one overload whose parameter takes the type of `x` (an `int?` overload
  takes `3` and `None`). There is no widening: `Rat.convert(1.5)` is *type Rat has no convert for float
  (it has: int, str)*. An argument that fits two overloads (`[]` against `List[int]` and `List[str]`)
  is *type annotations needed*.
- Each overload takes one parameter named `x`, of a concrete type (not a type parameter), and declares
  its return type, `Self` or `Self!E`; any other is *an overloaded convert returns Self or Self!E,
  found int*.
- Two overloads may not overlap, that is, one value may not fit both: `int` and `int?` overlap (`3`),
  `int?` and `str?` overlap (`None`), `int!str` and `int!int` overlap (`3` wraps into both), a
  protocol overlaps a type that satisfies it (also as `Sq?`), and two protocols always overlap. The error names the first one: *conflicting convert for int: convert(x: int) at 3:8*.
- Only `convert` overloads. Any other repeated method name, and an instance `convert(self, …)` declared
  twice, is *method '…' is already defined*.
- A trace, `<fn …>` and every message call an overload `convert`.
- A free `S` in `[S, T: Convert[S]]` is not inferred from an overloaded type: pin it.
- A bound picks too, by the direct call's rule: `fn make[S, T: Convert[S]](x: S) -> T` called as
  `r: Rat = make("ab")` runs the `str` overload, and `make([])` on a type with `str` and `List[int]`
  overloads runs the `List[int]` one (Rust's `make(vec![])` infers `Vec<i32>` the same way). A source
  that fits two overloads (`[]` against `List[int]` and `List[str]`) is *type Rat does not satisfy
  Convert (type annotations needed: …)*. The declaration order never matters.
- As a value, `Rat.convert` names several fns, so its position's fn type picks one:
  `f: fn(int) -> Rat = Rat.convert` is the `int` overload (an annotated binding, a parameter, a
  return, a typed list element), and so does a generic slot's param type: `[1, 2].map(Rat.convert)`
  is the `int` overload. With no fn type there, `g := Rat.convert` and
  `print(Rat.convert)` are *type annotations needed: Rat.convert has 2 overloads (int, str)*, as
  Rust's `let g = Rat::from;` is `E0283`. A type with one `convert` reads as a value as before.
