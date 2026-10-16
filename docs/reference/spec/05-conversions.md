## Type conversions & casting

Chezzi has **no `as` cast operator**, no `Into`/`TryFrom`, and no value-level conversion protocol; a
**bound-only** `Convert[S]` protocol exists as a generic bound (below). A small fixed set of explicit
builtins does conversion, and there is no implicit `int`→`float` widening at a slot (rule D3). A
conversion is therefore always visible in the source.

**Scalar conversion constructors** (global builtins; see [stdlib §1](../stdlib/01-builtins.md)):

| Form | From → To | Failure |
|------|-----------|---------|
| `int(x)` | `int`/`float`/`bool`/`str` → `int` | truncates a float; **faults** (recoverable) on a non-finite float or an unparseable string |
| `float(x)` | `float`/`int`/`bool`/`str` → `float` | **faults** on an unparseable string |
| `bool(x)` | `int`/`float`/`bool`/`str` → `bool` | never fails: a truthiness cast (int `0`/float `0.0`/`-0.0`/empty `str` → `false`; `NaN` → `true`; non-empty `str` is `true`, **not** a parse) |
| `str(x)` | **anything** → `str` | never fails: the `Stringable` display cast (`print`/interpolation use the same path) |
| `ord(s)` / `chr(n)` | `str` ↔ codepoint `int` | narrow, single-purpose |

`int()`/`float()`/`bool()` **reject** a `List`/`Map`/`Set`/tuple/struct/enum/function/`T?`/
`T!E`/`bytes`/`bytearray`/`Shared`/`Channel`/`Atomic`/`AtomicInt`/`RwShared`/`Executor`/`Socket`/
`Listener`/`Writer`/`Reader`/`ptr` argument at CHECK time: the runtime always faults there, so the
checker catches it early.

**Safe (non-faulting) string parse:** `s.to_int() -> int?`, `s.to_float() -> float?` (`None` on bad
input). The siblings that carry an error message return a `T!E`: `s.parse_int() -> int!str`,
`s.parse_float() -> float!str` (`?n` or `!msg`). Use these over `int()`/`float()` for untrusted input.

**No implicit `int` → `float` conversion at a slot** (rule D3): write `1.0`, or `float(x)` for a typed
value (Rust's `let x: f64 = 1;` is E0308 too). The reverse (`float` → `int`) is a lossy type error as
well. Arithmetic and comparison on VALUES still promote by runtime tag (`2 * 1.5` is `3.0`, `i < f`).
Full rules: [Numeric semantics](04-semantics.md#numeric-semantics).

**Stringable:** every scalar (`int`/`float`/`bool`/`str`) intrinsically satisfies `Stringable`, so
`[T: Stringable]` generics accept them (an erased `v.str()` body dispatches to the same native
`stringify` that `str(x)` uses). Structs/enums opt in with their own `str(self) -> str`.

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
`Hashable`→`hash`, `Error`→`message`, `Iterable`→`iter`, `Index`→`index`, `IndexSet`→`index`+`set_index`,
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
  carries a static requirement, and whose body needs it, gets a hidden trailing parameter holding the
  concrete type's runtime identity key, so `T.convert(n)` dispatches to the real type while generics
  stay erased (nothing monomorphized; no type argument reaches the VM). The same mechanism serves
  `T.default()` on any user protocol with a static requirement.
- It does not work for: a type parameter of the enclosing TYPE, a witness-taking fn read as a function
  VALUE, and an undetermined `T`. A `spawn`/`defer` statement target is an ordinary call site. Surface:
  `syntax.md §7a`.
- Direct `Type.convert(x)` needs no protocol and is the right spelling when the type is known. See
  [`roadmap.md` §3](../../design/roadmap.md).
