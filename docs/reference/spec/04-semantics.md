## Standard library

The library surface (global builtins, methods on the built-in types, the runtime types, the native
`std.*` modules written in Rust and the pure-Chezzi `std.*` modules) is specified in the
[standard library reference](../stdlib/01-builtins.md). In short:

- **Builtins (no import):** `print`, `Range`, casts (`int()`/`str()`/`float()`/`bool()`), `ord`/`chr`,
  the container constructors, `panic(msg)`, and the core-type methods (`s.upper()`, `xs.push()`,
  `m.get()`, `set.add()`).
- **Native modules** bind Rust functions through the FFI seam (see [Native FFI](#native-ffi)):
  `std.math`, `std.io`, `std.os`, `std.fs`, `std.time`, `std.process`, `std.rand`, `std.regex`,
  `std.request`, `std.net`, `std.ffi`, `std.encoding`, `std.crypto`, `std.uuid`.
- **Pure-Chezzi modules** are `.chz` files under `std/`: `std.string`, `std.csv`, `std.path`,
  `std.datetime`, `std.collections`, `std.concurrency.*`, `std.cmp`, `std.bisect`, `std.memoize`,
  `std.duration`, `std.flag`, `std.log`, `std.iter`, `std.json`, `std.cancel`.

## Numeric semantics

**Integers.** The one integer type is `i64`. Every overflow is a *recoverable panic*
(`"integer overflow in <op>"`, catchable by `recover:`), never a silent wrap and never a host crash:
arithmetic (`+ - * /`), left shift (`<<`, when a significant bit is shifted out), negation,
`MIN / -1`, `math.abs(MIN)`, and integer `List.sum()` (checked add). `%` never overflows: `MIN % -1`
is `0` (as in Go and Python), because a true remainder always fits the dividend's range. Integer `/`
and `%` by zero fault. A constant int expression that always faults is a type error at `chezzi check`,
with the run-time text: an overflow (`9223372036854775807 + 1`), `1 / 0`, `1 % 0` and `1 << 64`. A
constant divisor of zero, or a constant shift amount outside `0..64`, is a type error
whatever the int left operand is: `x / 0`, `x % 0`, `x << 64`, `x >> -1`, and each compound form
(`x /= 0`, `x <<= 64`). Go and rustc reject the same forms. A float left operand (`1.5 / 0`, `1.0 / 0.0`), a left operand of a
type with a `div` hook, and a generic `T` left operand stay legal. A divisor or shift amount that is
not a constant faults at run time, and `recover:` catches it. Integer `/` truncates toward zero and `%`
takes the dividend's sign.

**Floats are total IEEE-754.** A `float` op *never* faults: `1.0/0.0` is `inf`, `-1.0/0.0` is `-inf`,
`0.0/0.0` and `5.0%0.0` are `NaN`, and `math.sqrt(-1.0)` is `NaN`. `inf`/`NaN` are ordinary values;
inspect them with `math.is_nan` / `math.is_inf` / `math.is_finite`. Casting a non-finite float to `int`
(`int(1.0/0.0)`) still faults: `inf`/`NaN` have no integer value.
- **Float `%` is `fmod`: the sign follows the DIVIDEND** (`-7.0 % 3.0` is `-1.0`, `7.0 % -3.0` is
  `1.0`), the rule of integer `%`, C/Rust `%` and Go `math.Mod`. Python's floored `%` follows the
  divisor (`-7.0 % 3.0` is `2.0`) and raises `ZeroDivisionError` on a zero divisor; Chezzi's is total.
- **Ordered comparisons with `NaN` are total:** `< <= > >=` against a `NaN` are `false` (never a fault),
  as in IEEE-754 / Python / Rust. `nan == nan` is `false`, `nan != nan` is `true`.
- **Inside a container an element matches by `identity or ==`** (Python's rule), so ONE `nan` value
  stored in a list is found by `==` / `in` / `index_of` / `unique` (`n := nan_expr; xs := [n]` ⇒
  `[n] == [n]`, `n in xs`), while two separately computed `NaN`s stay unequal. The bare `==` operator is
  unchanged.
- **One total order for sorting.** `sort()`, `sort_by_key`, `.min()`, `.max()` and `Comparable`'s
  `.compare()` share `f64::total_cmp` (`NaN` sorts to one end; `+0.0` and `-0.0` are EQUAL, so a stable
  sort keeps their input order, like CPython). The operators stay IEEE: `nan < 1.0` is `false` while
  `nan.compare(1.0)` is nonzero. `a.compare(a)` is `0` for a `NaN` `a`, although `a == a` is `false`.
  `compare` is callable on a scalar only through a `[T: Comparable]` bound.

**No `int`→`float` widening at any slot** (rule D3; Rust/Kotlin, not Go's untyped-constant rule). An
`int`-typed expression, literal or not, is never accepted where a `float` is expected: a typed binding,
an assignment, an argument (also through a function VALUE and to an `extern` `double` param), a
`return`, a default, a list/map element, a `Channel`/`Shared`/`Atomic` value, a `yield`, a generic
type-argument binding (`mx(1, 2.5)` with one `T`), or an `if`/`match` join
(`x := if c: 1 else: 2.5`). The error names the fix: `expected float, found int — write 1.0 (or
float(x))`; the mixed literal `[1, 2.5]` is `list elements differ: int vs float`. A `float` sink spelled
through a type alias (`type F = float`) obeys the same rule.
- Nothing coerces at runtime, so a `float` slot always holds a real float (`x: float = 3.0` makes
  `x / 2 == 1.5`).
- Arithmetic and comparison on VALUES still mix: `2 * 1.5` is `3.0`, and `i * f`, `x + 1` (`x: float`),
  `f += 1` and `i < f` promote by runtime tag.
- An `Any` slot keeps an `int` an `int` (`xs: List[Any] = [1, -2.5]` stores `1`), as CPython and Go's
  `var x any = 1` do.
- Lossy conversions are type errors too (`y: int = 2.3`, `-> int: return 2.3`).
- Protocol widening is a separate rule and stays (`Box[Named] = Box(A())`).

Full sink list: `syntax.md` §3.

## Type rules

- **Return inference.** Sibling `return` branches of an un-annotated fn merge with a join that does
  **not** widen `int`→`float`: `if c: return 1 else: return 2.0` **conflicts** (write `2.0` in both), and
  so do `return ?1` / `return ?2.0`.
- **Carrier joins.** An `if` / `match` / `??` join of two `T!E` values keeps `T!E`: with
  `a() -> int!str` and `b() -> int!str`, `x := if c: a() else: b()` is an `int!str`, so `x?` propagates
  a `str`. No join rewrites an error type to `Error`, and two different error types (`int!str` beside
  `int!int`) do not join. A bare `!e` beside a `T!E` sibling takes that sibling's type, as a plain value
  beside a written `None` takes the optional. A carrier beside its plain payload (`T?` or `T!E` beside
  `T`) does **not** join by itself: the error names the annotation that makes it legal
  (`x: int? = ...`, `xs: List[int?] = [...]`). See `syntax.md` "Return types".
- **Protocol existentials are sendable** (like Go's `chan interface`): `Channel[Error]`,
  `Channel[int!]` and `Channel[Drawable]` over any user protocol type-check. The erased witness crosses
  the airlock by deep copy, and each widening site checks the concrete witness's own sendability. A
  witness that cannot serialize (one that holds a `Module` handle) is rejected at the **runtime
  airlock**.
- **Map/set keys.** Any CONCRETE `Hashable` type is a valid map/set key. `Hashable` embeds `Eq`, so a
  generic `[T: Hashable]` may build, index, or mention a `Map[T, _]`/`Set[T]` and compare `T`.
- **Enum methods.** `fn name(self, …)` blocks after an enum's variants work like struct methods:
  generic-enum type params are in scope, an enum can define `str`/`hash`/`add`/`compare` to satisfy
  `Stringable`/`Hashable`/`Add`/`Comparable` (`compare` alone is enough for `Comparable`, because an
  enum satisfies `Eq` structurally), it can pass into protocol-bound generics, and it can overload
  `+`/`-`/`*`/`<`.
- **No `newtype`.** `newtype` is an ordinary identifier; a one-field `struct` with methods is the
  distinct-type idiom:

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

- **Static methods** ("no self ⇒ static"): a struct/enum method whose first parameter is not `self` is
  called `Type.method(args)`. It enables named and validating constructors (`Rect.square(5)`,
  `Email.parse(s) -> Email!str`). A protocol may require a static method, and a generic bounded by it
  calls it through the type parameter (`T.zero()`, witness passing). See `syntax.md §7a`.
- **Turbofish at the declaration site.** Explicit type args go where the generic is DECLARED: on the
  type (`Box[int].Has(5)`, `Box[int].empty()`) or on the member (`s.first[int, str](1, "x")`,
  `Box[int].make[str](x)`). `recv.name[X](args)` parses as a method turbofish on any receiver; a head
  that denotes data indexes then calls (`h.fs[k](10)`). See `syntax.md §7a/§7b`.
- **Expected-type inference.** A surrounding annotation (a binding's declared type, a declared return
  type, a parameter type) pins the type parameters of a generic constructor or call that the arguments
  left free (precedence: **turbofish > arguments > annotation**), so `h: Heap[int] = Heap([], fn(x, y):
  x < y)` type-checks. A generic fn used as a VALUE is pinned the same way (`g := ident[int]`,
  `h: fn(int) -> int = ident`); an unpinned one is solved within ONE statement, else rejected at the
  read. The hint does not reach a generic ctor nested inside a container literal. **Untyped constants**
  (Go's model): a literal or constant expression meets the slot it lands in, and a value outside a C
  width there is `constant 256 does not fit int8 (-128..127)`. See `syntax.md` "A GENERIC fn as a value".

## Native FFI

Chezzi is written in Rust, so the native-stdlib mechanism doubles as a foreign-function interface:
bind a Rust fn and expose it as a module member instead of reimplementing it in Chezzi.

- **`NativeFn`**: a Rust fn registered as a callable Chezzi value (a member of a native module),
  carried by `vm::Obj` (`Native`).
- **`Host` trait** (`src/native/mod.rs`): the context a native fn uses (`arg_int`/`arg_float`/
  `arg_str`, stdout/stderr/stdin, args/env/cwd), so a binding is written once and runs on the VM.
  Results flow back as a `NativeRet`, lowered to a `Value` *after* the call (GC-safe). `NativeRet`
  carries structs and maps, so a native fn can return a structured value.
- **Dependency policy:** the **core** (lexer/parser/checker/compiler/VM/GC) is Rust `std` only. The
  **runtime** links a small fixed set of crates *unconditionally* (no Cargo features): `regex`
  (`std.regex`), `ureq`+TLS (`std.request`), `libc`/`polling`/`socket2` (the netpoller + `std.net`), and
  `libffi`/`libloading` (the dynamic C-ABI FFI). See `Cargo.toml`.

**Dynamic C-ABI FFI (`extern "lib":`).** An indentation block of statically-typed C signatures, bound at
module init by `dlopen`+`dlsym` and called at runtime through `libffi`, on the same `Host`/`NativeRet`
seam as any native fn. `extern` fns become ordinary module globals, so normal call dispatch and type
checking apply. The checker enforces **C-marshallability** on the resolved type, so a
non-marshallable param is a compile error. The user surface (marshalling types, `owned_str`, `str?`,
`ptr`, fixed-width integers, `float32`, structs by value, callbacks, varargs) is in
[`syntax.md` §12b](../syntax/17-ffi.md); the `load_*`/`store_*`/`alloc` builtins are in
[`std.ffi`](../stdlib/06-native-network-ffi.md#stdffi).

| Shipped | Notes |
|---|---|
| Scalars | `int` ↔ C `long`, `float` ↔ `double`, `bool` ↔ `_Bool` (1 byte; bind an int-returning predicate like `isdigit` as `-> int` and test `!= 0`), `str` → `char*` |
| Fixed-width ints | `int8`..`uint64`, imported per name from `std.ffi`; a param truncates (C cast), a return sign- or zero-extends |
| `float32` | C `float`; the program sees `float` |
| `ptr` | opaque `void*` handle, never auto-freed, cannot be forged from an int |
| `owned_str` / `str?` | return-only: copy **and** libc-`free` a `malloc`'d `char*` / `NULL` → `None` (`owned_str?` composes both) |
| Structs by value | scalar fields or nested C-marshallable structs, any depth; libffi computes the layout |
| Sync scalar callbacks | a `fn(scalars) -> scalar` extern param becomes a libffi trampoline that C calls back synchronously; a fault is caught and re-raised |
| C varargs | a trailing `...`; each vararg gets C's default promotions |
| Memory access | `std.ffi` `load_*`/`store_*` and `alloc`/`alloc_zeroed`/`free`, so `qsort`/`bsearch` of a Chezzi list works |

<a id="level-3-ffi-limits"></a>
**Level-3 FFI limits (known + by design):**
- **Integer width:** bare `int` (i64) marshals as C **`long`**, 64-bit on every supported **LP64** unix
  target. Bind `int32_t` and friends with the width names. A `uint64` above `i64::MAX` wraps negative.
  There are no C-spelling aliases (`c_int`/`c_short`/…), because their width is platform-dependent.
- **Non-unix is unsupported:** on LLP64 (Windows x64) C `long` is 32-bit, so the checker **rejects
  `extern` on non-unix targets** and the `cffi` module is `#[cfg(unix)]`-gated.
- **`char*` ownership:** a plain `str` return is **borrowed** (copied, never freed), so a fresh `malloc`'d
  `char*` leaks unless you declare it `owned_str`. Only libc `free` backs `owned_str` (if `free` cannot
  be resolved, the return falls back to the leak instead of aborting). `owned_str` is your assertion that
  the buffer is `malloc`'d: declaring a static return `owned_str` corrupts the heap.
- **Non-reentrant C:** `extern` calls are **not** serialized: two OS-thread workers can be inside C at
  the same time. Calling a **non-reentrant** C function (`strtok`, `gmtime`/`localtime`, `setlocale`,
  anything with static buffers or careless `errno`) concurrently races at the C level. Use only
  thread-safe entry points.
- **Untyped, manually freed handles:** one opaque `ptr` type serves every C handle (passing the wrong
  handle is C-level UB), and the program must call the library's own destroy (`fclose(f)`); forgetting
  leaks. A `NULL` `ptr` return is `<ptr null>`, not a fault.
- **Struct fields:** a `str`/`owned_str` field (at any depth), a recursive struct and a generic struct
  are rejected at the checker. A `bool` field is a 1-byte C `_Bool`; for an int-width C boolean field
  use `int8`/`uint8`/`int32` and test `!= 0`.
- **Unsafe memory:** a bad pointer segfaults; double-free / use-after-free / out-of-bounds
  `store_*`/`load_*` are UB. Only the NULL base pointer is checked (a recoverable error).
- **Blocking:** a slow C call runs inline and pins its worker (an `extern` fn is not a `Kind::Blocking`
  native).

Implementation notes: an extern fn is a `vm::Obj::Cffi(Arc<Cffi>)`. The `Cffi` keeps its `Library`
alive, stores the resolved symbol as a `usize` (libloading `Symbol` is `!Send`) and rebuilds the `Cif`
per call (libffi `Cif` is `!Send`), so it is `Send + Sync`. Struct layouts and per-field offsets are
rebuilt per call through `ffi_get_struct_offsets`, so the platform ABI is libffi's. A struct return
uses a buffer of `max(struct_size, sizeof(ffi_arg))`. See `src/native/cffi.rs` and
`examples/ffi*.chz`.

**Still deferred:** `str` struct fields, stored/cross-thread callbacks, a GC-tracked auto-freed
owned-buffer type + bulk-copy helpers + `realloc`, a custom user-named deallocator, and a Rust
`Box<dyn Any>` "userdata" handle for compiled-in Rust libraries. Design notes and the callback
feasibility ladder: [`ffi-and-packaging.md`](../../design/ffi-and-packaging.md).
