## 12b. Dynamic C-ABI FFI — `extern "lib":`

Call C functions in a shared library, with static type-checking. An `extern "lib":` block (indentation,
not braces; `{` is a map literal) lists body-less C signatures. Each becomes a module-global callable,
bound at module init by `dlopen` + `dlsym` and called through `libffi`. A missing library or symbol
fails at startup. An extern fn exports like a top-level `fn` (`mod.name(...)`, `import name from mod`)
and is a first-class value.

> **The library name goes to `dlopen` as written, except the two aliases `libc` / `libm`.** Those try
> a per-platform list in order (Linux: `libc.so.6`/`libc.so`, `libm.so.6`/`libm.so`; macOS:
> `libSystem.B.dylib`/`libc.dylib` or `libSystem.B.dylib`/`libm.dylib`; other unix:
> `libc.so`/`libm.so`), so `extern "libc":` and `extern "libm":` work on Linux and macOS. Every other
> name is platform-specific; choosing a portable name is your job.

```chezzi
extern "libm":
    fn cos(x: float) -> float
    fn sqrt(x: float) -> float

extern "libc":
    fn strlen(s: str) -> int

print(cos(0.0))        # 1.0
print(sqrt(4.0))       # 2.0
print(strlen("hello")) # 5
```

An `extern "lib":` block is top-level only (a parse error inside `if`/`for`/`fn`). An extern fn named
after a builtin (`print`, `int`, `Channel`) shadows it in its module. It is a value, so it follows the
redeclaration rule of a `fn` (§3, "Declaring one name twice at module level"): beside a `struct`, `enum`
or alias of its name it is the value a call reaches.

### Marshalling

| Chezzi | C | Notes |
|---|---|---|
| `int` | `long` | for a fixed-width C int use `int8`..`uint64` (below) |
| `float` | `double` | `cos(2)` is a type error naming `write 1.0` (no int→float, §3) |
| `float32` | `float` | from `std.ffi`; the program sees `float`. A param rounds (`16777217.0` → `16777216.0`, `1e39` → `inf`); a return widens exactly (`sqrtf(2.0)` is `1.4142135381698608`) |
| `bool` | `_Bool` (1 byte) | a pre-C99 predicate that returns any nonzero `int` (`isdigit`) must be bound `-> int` and tested `!= 0` |
| `str` | `const char*` | a `char*` return is copied into a `str`; see `owned_str` and `str?` below |
| `ptr` | `void*` | opaque handle (below) |
| no return / `-> None` | `void` | `None` is return-only |
| a `struct` of these | struct by value | below |

Any other param or return (list/map/set/tuple/enum/generic struct/…) is a *not C-marshallable* error.
A `str`/`bool` where a number is expected is rejected. Calls run inline, so a slow C call holds its worker.

**Structs by value.** A `struct` as an extern param or return passes a C struct by value. Field order
and types define the C layout; libffi computes size, alignment and offsets for the platform ABI:

```chezzi
# div_t div(int numer, int denom) — returns a small POD struct BY VALUE.
struct DivT:
    quot: int32
    rem: int32

extern "libc":
    fn div(numer: int32, denom: int32) -> DivT

r := div(17, 5)
print(r.quot)   # 3
print(r.rem)    # 2
```

Every field is a C scalar (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`/`float32`) or a C-marshallable
struct, nested to any depth (raylib's `Camera2D` binds directly):

```chezzi
import float32 from std.ffi

struct Vector2:
    x: float32
    y: float32

struct Camera2D:
    offset: Vector2
    target: Vector2
    rotation: float32
    zoom: float32
```

- Rejected: a recursive struct (*is recursively defined and cannot be C-marshallable*), a `str` field at
  any depth (the error names the struct and the field), a generic struct (`Pair[int]`), and a zero-field
  struct (*struct 'Empty' has no fields and cannot be C-marshallable*).
- A `type P = Point` alias works like the struct. The struct may be declared before or after the `extern`
  block, or in another module (`import DivT from core.cdefs`, or `cdefs.DivT` after `import core.cdefs`).
- A `bool` field is a 1-byte `_Bool` (matches C `_Bool` or `char`, not `int`). For an `int`-width C
  flag use `int8`/`uint8` (or `int32`) and test `!= 0`.

### C variadic functions (`...`)

A bare `...` as the last extern param declares a C variadic function; extra args are typed from their
static types:

```chezzi
import std.ffi

extern "libc":
    fn snprintf(buf: ffi.ptr, n: int, fmt: str, ...) -> int

buf := ffi.alloc(64)
defer ffi.free(buf)
print(snprintf(buf, 64, "%s-%ld-%.2f", "x", 42, 3.14159))   # 9
print(ffi.load_str(buf))                                    # x-42-3.14
```

| Chezzi arg | C vararg | `printf` spec |
|---|---|---|
| `int` | `long` | `%ld` (not `%d`) |
| `float` (incl. a `float32` result) | `double` | `%f` / `%g` |
| `bool` | `int` (0/1) | `%d` |
| `str` | `const char*` | `%s` |
| `ptr` | `void*` | `%p` |

- Any other value is *cannot be passed to a C variadic parameter*; too few fixed args is *expects at
  least N argument(s)*.
- The bare `...` is legal only last in an `extern` fn. It differs from Chezzi's `...xs: T`, which an
  extern fn rejects.
- A C variadic read as a value (`f := printf`) takes only its declared params.
- Calls use `ffi_prep_cif_var`, so `double` varargs are ABI-correct.

### Callbacks

**Sync scalar callbacks.** A `fn(...)` extern param passes a Chezzi closure to C as a function pointer
that C calls during the extern call. Params must be C scalars (`int`/`float`/`bool`/`ptr`/`int8`..
`uint64`; no `str`, struct or nested callback); the return may also be `None` (as `ctypes.CFUNCTYPE(None, ...)`):

```chezzi
extern "libapply.so":
    # f is a sync scalar callback: C calls it back during apply(), on this thread.
    fn apply(x: int, f: fn(int) -> int) -> int

print(apply(10, fn(n: int) -> int: n * n))   # C runs f(10) -> 100, then returns 100 + 1 = 101
```

- A fault (or panic) in the callback is re-raised as the extern call's error (recoverable; CPython's
  `ctypes` prints it and returns `0`). The callback runs on the calling thread.
- If C calls it again after a fault (`qsort` keeps comparing), the FIRST fault is re-raised; later calls
  run no Chezzi code and return zero. C's loop still runs to its end.

**A callback that C stores aborts the process.** The trampoline is valid only during the extern call.
If C keeps it (`signal`, `atexit`, GLib/GTK, `pthread_cleanup_*`) and calls it later:

```
chezzi FFI: callback invoked after the extern call that received it returned; stored/cross-thread callbacks are not supported
```

A call from another thread, before or after the return, aborts the same way:

```
chezzi FFI: callback invoked from a thread other than the one that made the extern call; stored/cross-thread callbacks are not supported
```

- The message goes to fd 2 and the process dies with SIGABRT (status 134). Buffered stdout is lost, as
  on any crash. The checker cannot catch this: the same `fn(int) -> int` param is correct for `qsort`.
- Each extern call that passes a callback leaks its trampoline (about 400 B plus a page pair from
  libffi's pool). When the pool is full the call raises the recoverable error `cannot allocate a callback
  trampoline for argument N to 'f': the FFI closure pool is exhausted`.

### C output

**C writes do not go through the runtime's stdout.** A Chezzi `print` is line-atomic, unbuffered, and
faults when the reader closes (`stdout closed (broken pipe)`). A C function writes the fd itself: its
bytes sit in the C library's buffer (block-buffered on a pipe, so they usually appear last), and a
closed reader is invisible (`chezzi run x.chz | head -1` over a loop of C writes runs forever; SIGPIPE
is ignored process-wide). Python `ctypes` and Go `cgo` behave the same.

```chezzi
extern "libc":
    fn puts(s: str) -> int

print("chezzi-1")
_ := puts("ffi-2")
print("chezzi-3")
_ := puts("ffi-4")
# piped:  chezzi-1  chezzi-3  ffi-2  ffi-4   <- NOT source order; io.flush() does not change it
```

To use the runtime's stdout, return the string and `print` it. If C must write, check its return value,
and declare the exact width: `puts` returns a C `int`, and a bare `int` is C `long`, so `-1` arrives as
`4294967295`:

| declared return | `puts`'s value once the reader is gone | `if r < 0` |
|---|---|---|
| `fn puts(s: str) -> int` | `4294967295` | **never fires** |
| `fn puts(s: str) -> int32` | `-1` | fires, at the same write |

```chezzi
import std.io
import int32 from std.ffi

extern "libc":
    fn puts(s: str) -> int32     # C `int` — NOT bare `int`, which is C `long`

fn main():
    i := 0
    while i < 200000:
        if puts("line") < 0:
            io.eprint("stdout is gone, stopped at i={i}")   # the runtime will NOT fault for you
            return
        i = i + 1

main()
```

Under `| head -1` it stops at `stopped at i=1638` (the C library sees the dead pipe when its 4 KiB
buffer first reaches `write(2)`). Declared `-> int`, it runs to 200 000.

### Opaque handles (`ptr`)

A handle-based C library (`FILE*`, `sqlite3*`, a create/use/destroy context) returns a `void*` you pass
back. Declare it `ptr`, imported from `std.ffi` (`import ptr from std.ffi`, or `ffi.ptr` after
`import std.ffi`), also in an `extern` block.

- A `ptr` is untyped (one type for every handle, as `ctypes`), holds no readable data, and is **never
  auto-freed**: call the library's destroy yourself, or it leaks.
- It supports only `==`/`!=` with another `ptr`, and passing/returning. No methods, fields or arithmetic.
- A returned NULL is not a fault. `null()` and `is_null(p)` come from `std.ffi`.
- It prints as `<ptr null>` / `<ptr>`, never the address. It is sendable (crosses `spawn`/channels by value).

```chezzi
import null, is_null from std.ffi

extern "libc":
    fn fopen(path: str, mode: str) -> ptr
    fn fclose(f: ptr) -> int

f := fopen("/dev/null", "r")
if is_null(f):              # or: f == null()
    print("open failed")
else:
    print(fclose(f))       # 0 — the FILE* handed straight back; zero-copy
```

`std.ffi` exports the values `null()` / `is_null(p)` and nine width type names: `int8`/`int16`/`int32`/
`int64`/`uint8`/`uint16`/`uint32`/`uint64` and `float32`. Type names are imported per name
(`import int32, uint32 from std.ffi`), like `ptr` and `owned_str`; a module that does not import one
cannot use it, in an annotation or an `extern` signature.

The memory behind a `ptr` is readable and writable with `std.ffi` `load_*`/`store_*` (every C scalar
width plus `load_str`, each with an `_at(p, off)` byte-offset form). `alloc(nbytes)`/`alloc_zeroed(nbytes)`
(libc `malloc`/`calloc`) make a buffer you release with `free(p)` (`defer ffi.free(p)`). This is unsafe
like `ctypes`: a bad pointer segfaults; double-free, use-after-free and out-of-bounds are undefined;
only a NULL base pointer is guarded (recoverable error). A `ptr` cannot be made from an int. See
[`stdlib.md`](../stdlib/01-builtins.md) §std.ffi.

### `str` returns: borrowed, owned, nullable

A plain `str` return is **borrowed**: the `char*` is copied and never freed (a `malloc`'d return
leaks), and `NULL` is a recoverable fault. Two return-only forms change that:

- **`owned_str`** (`import owned_str from std.ffi`): the C function gives you a `malloc`'d `char*`
  (for example `strdup`). Chezzi copies it and frees it with libc `free`. It is a plain `str` to the
  program. `NULL` still faults.
- **`str?`**: `NULL` becomes `None`; a non-null pointer is a borrowed `?str` (for example `getenv` of an
  unset variable). This is the only way to make a `NULL` return non-fatal.
- **`owned_str?`**: nullable and freed (`NULL` → `None`, nothing freed).

Both are return-only: as a parameter they are *not C-marshallable*. `owned_str` is legal only in an
`extern` signature: `fn f(x: owned_str)` is *'owned_str' is a return-only extern marshalling type and
cannot be used as a general type annotation*.

```chezzi
extern "libc":
    fn strdup(s: str) -> owned_str   # owned malloc'd char* — copied AND freed (no leak)
    fn getenv(name: str) -> str?     # nullable — NULL → None instead of a fault

print(strdup("hi"))                  # hi   (the C buffer is freed after the copy)
match getenv("HOME"):
    ?v: print(v)
    None: print("unset")
```

- **Every `char*` return is UTF-8-validated.** A non-UTF-8 buffer (latin-1, or a pointer into the middle
  of a codepoint) faults with the byte offset, for `str`, `owned_str`, `str?` and `owned_str?` (an
  `owned_str` buffer is freed first). Read binary bytes with `std.ffi`'s `load_uint8_at`.
- `free` is found once with `dlsym("free")`. A custom deallocator is not supported.
- `owned_str` asserts the buffer is `malloc`'d: declaring a static or literal return `owned_str` frees
  memory you do not own and corrupts the heap.

### Fixed-width integers (`int8`..`uint64`)

Bare `int` is C `long`. For a fixed-width C integer, import a width name from `std.ffi`:

```chezzi
import int32, uint32 from std.ffi   # bring the width TYPE names into this module
```

- Without the import: *unknown type 'int32'; import it from std.ffi (`import int32 from std.ffi`)*. A
  bad name: *module 'std.ffi' has no member 'int99'*.
- An alias marshals as its width: after `import int8 as Byte from std.ffi`, `fn f(b: Byte)` passes an
  `int8_t`. `type int32 = …` beside `import int32 from std.ffi` is two declarations of one name;
  without the import, `type int32 = str` is an ordinary alias.
- The import is per module: a struct's `int32` field declared in module A works from module B with no
  import in B, but a bare `int32` written in B needs B's own import.
- To the program each width is a plain `int` (`float32`: a plain `float`). Widths are valid as param and return.

| name | C type | libffi type | as a parameter | as a return |
|------|--------|-------------|----------------|-------------|
| `int8`   | `int8_t`   | `sint8`  | must fit -128..127          | sign-extend i8 → i64  |
| `int16`  | `int16_t`  | `sint16` | must fit -32768..32767      | sign-extend i16 → i64 |
| `int32`  | `int32_t`  | `sint32` | must fit i32                | sign-extend i32 → i64 |
| `int64`  | `int64_t`  | `sint64` | any int                     | i64 (no change)       |
| `uint8`  | `uint8_t`  | `uint8`  | must fit 0..255             | zero-extend u8 → i64  |
| `uint16` | `uint16_t` | `uint16` | must fit 0..65535           | zero-extend u16 → i64 |
| `uint32` | `uint32_t` | `uint32` | must fit 0..4294967295      | zero-extend u32 → i64 |
| `uint64` | `uint64_t` | `uint64` | any int (its bit pattern)   | reinterpret u64 → i64 |

**Rule 1: a width type describes the C boundary only.** Write it only in an `extern` fn's param or
return type (a callback type there too) and in a struct field's own type. Elsewhere it is
``'int8' describes the C boundary only: write it in an extern fn signature or a struct field, and use
`int` here``.

| Position | `int8` written there |
|----------|----------------------|
| extern fn param / return, a callback type inside one | allowed |
| struct field's own type | allowed |
| local / global annotation (`x: int8 = 5`) | error (rule 1) |
| param / return of a normal fn, method, lambda (`fn f(a: int8)`) | error (rule 1) |
| type argument (`List[int8]`, `int8?`, `(int8, int)`, `Map[str, int8]`) | error (rule 1) |
| type alias body (`type W = int8`) | error (rule 1) |

**Rule 2: every store into a width slot is range-checked.** The slots are an extern param, a struct
field (constructor, field store, `json.decode`), an `ffi.store_<w>` store and a callback's return.
Chezzi never wraps silently (Go and Rust reject the same programs; `ctypes.c_int8(300).value` is `44`).

- A **constant** (a literal or a constant expression: `1 << 8`, `127 | (127 + 1)`, `3e38 + 3e38`) is
  untyped until it meets a slot (Go's model). Out of range it is a compile error at an extern argument,
  a struct constructor, a field store, a field default and an `ffi.store_<w>` argument:
  `constant 300 does not fit int8 (-128..127)`, `constant 256 does not fit int8 (-128..127)` for
  `1 << 8`. A non-constant int (`[300].len()`) is not checked at compile time.
- A **runtime value** out of range is a recoverable fault: `value 300 does not fit int8 (-128..127)`, at
  `S(big())`, `s.q = big()` and `s.q += 300` (which is `s.q = s.q + 300`). `json.decode[S]` returns a
  failed result with the same text.
- **`uint64`**: a constant must be `0..INT64_MAX`; at runtime any int passes as its bit pattern, and a
  value above `INT64_MAX` read back from C is negative (`UINT64_MAX` reads as `-1`).
- **`float32`**: a finite value outside the f32 range is a compile error as a constant and a fault at
  runtime; NaN and inf pass.
- A value read back from C is in range by construction; returns are not checked.

**Explicit wrap: `ffi.cast_<w>(v)`** (Go's `int8(v)`, Rust's `v as i8`): `ffi.cast_int8(300) == 44`,
`ffi.cast_uint32(-1) == 4294967295`, `ffi.cast_float32(1e39)` is `inf`. `std.ffi` also declares
`INT8_MIN`/`INT8_MAX` … `INT64_MIN`/`INT64_MAX`, `UINT8_MAX` … `UINT32_MAX`, `UINT64_MAX` (`-1`, the bit
pattern) and `FLT_MAX`.

A field `q: int8` reads as an `int`, and arithmetic on it is plain `int`. A return sign-extends (signed)
or zero-extends (unsigned): `int32` returning `-1` is `-1`; `uint32` returning `0xFFFFFFFF` is `4294967295`.

```chezzi
import int32, uint32, int8, cast_int8 from std.ffi
extern "libc":
    fn atoi(s: str) -> int32      # parse to a C int; -1 sign-extends back to i64 -1
    fn htonl(x: uint32) -> uint32 # unsigned in+out; a high-bit result stays positive
    fn abs(x: int8) -> int8       # signed round-trip

print(atoi("-1"))             # -1
print(htonl(128))             # 2147483648   (0x80000000, zero-extended → positive)
print(abs(cast_int8(255)))    # 1            (255 wraps to -1, then abs); `abs(255)` is a compile error
```

The `std.ffi` type names are `native type` decls in `std/ffi.chz` (`native type ptr`,
`native type int8 = int` … `native type float32 = float`); `native type` is std-only. A std file's
constants are ordinary `NAME: const T = value` lets (`std/math.chz`'s `pi`, `std/ffi.chz`'s `INT8_MAX`).
See [`examples/ffi_int.chz`](../../../examples/ffi_int.chz).

### Limits

- **C `long` width:** bare `int` (i64) is C `long`, 64-bit on LP64 unix. LLP64 (32-bit `long`) is
  unsupported: the checker rejects `extern` there. There are no C-spelling aliases (`c_int`, `c_short`).
- **A `str` argument's C buffer is kept for the life of the program** when the call could return a
  pointer into it (a `ptr` return like `strchr`, or a `ptr` param that may be an out-param like
  `strtol`'s `endptr`). One buffer is kept per distinct text. A C function that **writes** through a
  `str` param (`strtok`) must get an `ffi.alloc`'d `ptr` instead: a kept buffer is restored to its
  original bytes on the next lookup.
- **Extern calls are not serialized** across workers. A non-reentrant C function (`strtok`,
  `gmtime`/`localtime`, `setlocale`, static-buffer APIs) races when several tasks call it.
- **Not supported:** stored or cross-thread callbacks (they abort, above), a GC-tracked auto-freed buffer type, bulk-copy helpers, `realloc`, a
  custom deallocator, `str`/`owned_str` struct fields, and Rust `Box<dyn Any>` userdata. Design notes:
  [`ffi-and-packaging.md`](../../design/ffi-and-packaging.md) §1b.
