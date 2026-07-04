# Chezzi — FFI deepening & package distribution (design, NOT scheduled)

> Status: **brainstorm / design only.** Nothing here is on the M19 (perf-only) milestone. The freeze
> is a **pre-JIT gate**, not a hard language freeze — small, well-scoped semantics fixes still land
> (e.g. module-scoped user types, 2026-06); this is the forward map for the larger FFI/packaging work.
> It exists so [`spec.md`](spec.md) §"Still deferred" points at a real plan instead of just the word
> "deferred". Captures the reasoning from the FFI/packaging design discussion (2026-06).
>
> **Update (2026-06): the C-ABI half of the handle unlock SHIPPED.** An opaque `ptr` type (↔ C
> `void*`) now threads through `extern "lib":` — an untyped, never-auto-freed handle (`Obj::Ptr(usize)`
> / `Value::Ptr(usize)`) with `std.ffi.null()`/`is_null` and `ptr==ptr` identity. This covers
> handle-based **C** libraries (and any lib exposing a C ABI) over a dlopen'd `.so` with **no chezzi
> recompile** — see [`syntax.md`](syntax.md) §12b + `examples/ffi_ptr.chz`. The §3 below still describes
> the **other** handle — the rich Rust `Arc<dyn Any>` userdata for compiled-in Rust crates (Burn),
> which is the still-open forward design (it carries a live Rust object, not a raw address).

## TL;DR

- **Today's FFI has two seams.** Level-2 (compiled-in Rust bindings via `NativeFn`/`Host`/`NativeRet`)
  and Level-3 (`extern "lib":` dynamic C-ABI via dlopen+libffi — scalars, opaque `void*` handles, and
  flat-scalar structs by value). Both are *stateless, value-in / value-out*.
- **Strong libraries (Burn, torch-class) are blocked**, even Rust ones, even though Chezzi is Rust.
  The blocker is the **value model**, not linking: there is no opaque-handle `Value`, so a stateful
  `Tensor`/`Model` has nowhere to live.
- **The fix is one feature: an opaque-handle Value variant ("userdata").** It reuses the existing
  `seed_stdlib_structs` nominal-type mechanism. It must land **pre-JIT** and **co-designed with
  NaN-boxing**, because it changes the value layout the JIT would otherwise freeze.
- **A package registry splits in two.** Pure-Chezzi packages (source, no recompile — trivial). Native
  packages (the hard part). Native distribution is exactly Python's pip/wheels model — and works for
  Python only because **C has a stable ABI and Rust does not.** Chezzi must first freeze a narrow
  `repr(C)` plugin seam to get the same thing.

---

## 1. Where FFI stands (the two seams)

| | Level-2 native seam | Level-3 dynamic C-ABI (`extern "lib":`) |
|---|---|---|
| Mechanism | Rust `fn` compiled **into** the `chezzi` binary, registered in `native_members` | `dlopen`+`dlsym`+`libffi` at module init |
| Lives in | `src/native/` (`Host`/`NativeRet`/`NativeFn`) | `src/native/cffi.rs` |
| Used by | `std.math`/`io`/`os`/`fs`/`time`/`regex`/`request`/`net` | user `extern "lib":` blocks |
| Crosses the airlock | `NativeRet`/`NativeArg` (primitives, list, struct, map, Result/Option) | scalars (int↔long, fixed-width int8..uint64, float↔double, bool↔`_Bool` 1 byte, str→`char*`, opaque `ptr`↔void*) + a flat-scalar struct by value |
| State | **none** — `NativeFn` is a bare `fn` pointer, no captured state | **none** |
| Recompile to add? | **yes** — statically linked | **no** — dlopen at runtime |

Both seams are the **CPython-built-in-C-module model**: stateless functions, data in / data out. Neither
can hold a live foreign object across calls.

A marshallable struct or width alias declared in another **module** can be named at an `extern` boundary
two ways — by **named import** (`import DivT from core.cdefs`, then bare `DivT`) or by the
**module-qualified spelling** (`import core.cdefs`, then `cdefs.DivT` in the `extern` fn) — and both lower
to the identical C type (struct-by-value or scalar width). The checker is the marshallability gate for
either spelling; a non-marshallable type is a clean compile error, never a VM panic.

Every qualified/imported/aliased extern type resolves **module-scoped via the checker** — the single
authority. The checker already resolves each `extern` param/return in its **defining module's**
import/alias scope (which is why `chezzi check` is always correct); it now records the fully-resolved,
width-bearing C type per param/return into an extern-signature table, and **both backends consume that
table** instead of re-resolving alias names themselves. So a module-qualified **width alias** resolves
to its defining module's width even when the calling module declares a colliding bare alias of the same
name, and this holds for **every spelling and every depth**:

- a direct alias (`type Len = int64`),
- a local chain (`type Len = A; type A = B; type B = int64`),
- a **named-import hop** (the defining module reached the width via `import W from other`, e.g.
  `core/w3.chz` = `import W from core.widths` + `type Len = W` where `widths` declares `type W = int64`),
- a **qualified hop** (`type ImpW = base.Base`),
- and any **mix** of the above across modules.

With colliding `type W = int8` (or `Inner`/`Outer`/…) shadows in the calling module(s), an
`extern fn abs(n: mod.Len) -> mod.Len` still marshals as **int64** — never the local int8. Because the
checker walks each module's real import/alias environment, **no hop** can fall back to a flat
last-write-wins alias-name table; the C ABI can't be hijacked by a same-named local alias at any depth.
A by-value struct keeps each field's exact C width (an `int32` field stays 4 bytes), and — crucially —
each **field's** type resolves in the **struct's own defining module's scope**, not the importer's. So a
qualified/imported return struct whose fields are typed via the defining module's local alias
(`core/cdefs.chz`: `type Half = int32` + `struct DivT{quot:Half; rem:Half}`; `main`:
`extern fn div(...) -> cdefs.DivT`) marshals correctly — a colliding/invisible `Half` in the importer
can't drop the field. A cyclic alias chain is a clean "not C-marshallable" error (never a hang).
(2-engine parity: serial `--serial` / default M:N; verified silent-safe — the prior bug returned void with
`check` passing.) There is exactly **one** extern-type resolver — the checker — for single-file source
and multi-file projects alike; the backends do zero type resolution of their own, so a second resolver
cannot drift.

## 1b. Deferred FFI-deepening features — design notes (revisit-in-future)

The v1 `extern "lib":` surface ships: scalars, fixed-width ints (`int8`..`uint64`, `std.ffi`-imported),
`float`/`bool` (`bool` ↔ C `_Bool`, 1 byte), `str` (+ return-only `owned_str`/`str?`/`owned_str?`),
opaque `ptr` handles (with `std.ffi` `load_*`/`store_*` to deref the memory behind them and
`ffi.alloc`/`alloc_zeroed`/`free` to make C-laid-out buffers), and
flat-scalar **structs by value**. A couple of deepenings stay deferred. They're
captured here so a revisit starts from a plan, not a blank page. None is blocking — the v1 surface binds
the large majority of system/compute C libraries (numbers, strings, handles, small structs) with **no
`chezzi` rebuild**.

### #4 Callbacks / C function pointers — **sync scalar callbacks LANDED**
**Unlocks:** passing a Chezzi function to C as a C function pointer so C calls *back* into Chezzi —
needed for **event-driven / async** libraries (GLib/GTK signals, libuv, libcurl write/progress, SDL
audio, GLFW input) and a few stdlib helpers (`qsort`, `signal`, `atexit`). Compute libraries and
handle-based APIs (sqlite `prepare`→`step`→`finalize`, file I/O) need **none** of this.

**What landed (this milestone): synchronous, same-thread, scalar-by-value callbacks.** A
function-typed extern parameter — spelled with the *existing* `fn(a, b) -> r` type (no new grammar) —
whose params and return are all C scalars (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`; **no** `str`,
struct, or nested callback) marshals a Chezzi closure into a libffi `ffi_closure` trampoline. C
receives the trampoline's code address as a `void*`; when C invokes it (synchronously, *during* the
extern call), the trampoline reads the C scalar args, re-enters the engine through one engine-neutral
seam (`Host::invoke_callback`, keyed by arg index so no engine `Value` leaks across the FFI layer),
and writes the Chezzi result back into C's return slot. Wired on the VM (via
`guarded`+`invoke_value`) and consistent under `--parallel` (a sync callback fires
on the calling worker thread, no cross-thread hand-off). The closure is freed when the extern call
returns (sync scope ⇒ **no** GC rooting). Example:

```chezzi
extern "libapply.so":
    fn apply(x: int, f: fn(int) -> int) -> int   # f is a sync scalar callback param

print(apply(10, fn(n: int) -> int: n * n))       # C calls f(10) -> 100, returns 100 + 1 = 101
```

**Fault rule (stronger than ctypes).** The trampoline body is wrapped in `catch_unwind`: if the Chezzi
callback faults (or panics), a zeroed value is written to C's return slot so C unwinds cleanly, the
error is stashed, and it is **re-raised** as the extern call's own error (recoverable via `recover:`).
CPython's `ctypes` instead swallows a callback exception to stderr and returns `0`/`NULL`.

**Hazards this slice handles** (the synchronous subset of the UB-class list): the **unsafe executable
trampoline** (libffi `ffi_closure`), **engine re-entrancy** (the `Host::invoke_callback` seam solves
the `&mut Vm` aliasing), **inbound marshalling** (`CType::Callback{params, ret}`
+ checker support, scalars only), and **unwind safety** (the catch+re-raise above). **Cross-thread
invocation** and **GC rooting across the boundary** are *not* needed here because the callback can only
fire inside the same `ffi_call` on the same thread — they are the next deferred milestone (below).

#### Future work / deferred — the feasibility ladder
Recorded so a revisit starts from a plan, not a blank page:

1. **(landed, this milestone)** Sync scalar callbacks (above).
2. **(LANDED) Pointer-deref builtins** to read/write through a `void*` (a callback arg, an extern
   return, or any held `ptr`): `ffi.load_int`/`load_int8`..`load_uint64`/`load_float`/`load_float32`/
   `load_bool`/`load_ptr`/`load_str` and the `store_*` mirror (no `store_str` — unbounded write
   footgun), each with an `_at(p, off)` byte-offset form. See `stdlib.md §std.ffi` for the full
   surface + the NULL-fault rule + the "unsafe: arbitrary memory" warning. **Callback-WITH-pointer
   already worked** before this — `CType::Ptr` is a callback scalar, so `fn cmp(a: ptr, b: ptr) -> int`
   type-checks and runs — this slice only added the deref builtins so the held `ptr` can be read/written.
   *Python ref:* `ctypes` uses typed `POINTER(c_int)` args and `a[0]` to deref — Chezzi exposes the
   same as explicit typed load/store builtins on a `ptr`. Purely additive (no callback-engine change).
3. **(LANDED) C-buffer alloc layer** — `ffi.alloc(nbytes) -> ptr` (malloc; garbage bytes),
   `ffi.alloc_zeroed(nbytes) -> ptr` (calloc; zeroed), `ffi.free(p)` (free; returns nil). Backed by the
   **libc allocator** (so a buffer may be handed to a C fn that reallocs/frees it). Fill/read with the
   existing `store_*`/`load_*` builtins — there is **no** bulk list↔buffer copy helper (the loop idiom
   is the surface; a `write_ints`/`read_ints` is deferred). Manual free (`defer ffi.free(p)`); a `ptr`
   is never auto-freed. Recoverable faults: negative size, out-of-memory; `free(ffi.null())` is a no-op.
   Double-free / use-after-free / out-of-bounds store_/load_ are documented UB (no bounds/lifetime
   tracking — that's the deferred auto-buffer type). With this, **`qsort`/`bsearch` of a Chezzi `list`
   now fully works**: alloc + `store_*` + a callback comparator + `load_*` compose end-to-end (see the
   `examples/ffi_qsort.chz` capstone golden, run on both engines). Still deferred from this slice: a
   **GC-tracked / auto-freed owned-buffer type**, bulk-copy helpers, and `ffi.realloc`.
4. **(its own milestone) Stored + cross-thread callbacks** — a callback C keeps and calls *after* the
   extern call returns and/or from *its own* thread. Needs **two** new pieces:
   - a **callback registry** that GC-roots the closure (+ its upvalues) until an explicit `unregister`,
     since there is no "done" signal. *Python ref:* `ctypes` punts this onto the user — its docs warn
     *"Make sure you keep references to `CFUNCTYPE` objects as long as they are used from C code.
     `ctypes` doesn't, and if you don't, they may be garbage collected, crashing your program."* A
     registry makes Chezzi safe-by-construction where `ctypes` is footgun-by-default.
   - **thread-safe VM re-entry. ⚠️ The single biggest deferred caveat:** `ctypes` leans on the **GIL**
     to serialize a cross-thread callback onto the interpreter. **Chezzi's `--parallel` OS-thread
     engine has NO GIL**, so a callback fired from a C-owned thread is *strictly harder* than in
     Python — it cannot just acquire a global lock that already exists. It needs **either** a mini-GIL
     (a global callback lock that serializes all C→Chezzi re-entry) **or** thread-marshalling that
     hops the call onto the owning fiber's thread (the Node N-API `threadsafe_function` / JNI
     `AttachCurrentThread` pattern). This is the gating design decision for level 3.

   Note: our level-1 catch+re-raise fault rule already exceeds `ctypes`' swallow-to-stderr-and-return-0,
   and carries forward to levels 2–3 unchanged.

### #5 Variadic functions
**What:** C functions taking a variable arg count (`printf`/`scanf` family; the variadic forms of
`open`/`fcntl`/`ioctl`/`execl`).

**Why it's low priority:** the genuinely-variadic-required surface is tiny — `printf`/`scanf` (Chezzi
has its own formatting + `print`, so you'd rarely call C's), and most "variadic" syscall wrappers have
fixed-arity-per-call-site or array siblings (`execv`, `vprintf`). Chezzi now **has** a variadic
*parameter* surface (`fn f(...xs: T)`), but it collapses to a `List[T]` — it deliberately does **not**
feed the C vararg ABI, which needs concrete per-arg C types (an `int` vs a `double` picks a different
register class), not a homogeneous Chezzi list of one element type. Nor is there call-site spread
(`f(*args)`). So true varargs FFI still needs new machinery, not just this parameter feature.

**Workaround that needs nothing new:** declare a **concrete fixed-arity** extern signature for the exact
call form you need (`open` 2-arg vs a separate 3-arg binding). *Caveat:* on x86-64 SysV, calling a
variadic C fn through a *non*-variadic libffi CIF is technically ABI-incomplete (variadic calls set
`%al` = SSE-register count; libffi has `ffi_prep_cif_var`). Works in practice for **integer/pointer**
varargs; can break for **float** varargs or non-x86-64 ABIs.

**Two forks when revisited:** (a) a Chezzi-level variadic/spread call surface (a language feature, broad
blast radius) feeding `ffi_prep_cif_var`; or (b) an FFI-only typed-arg-list escape hatch —
`printf(fmt, ffi.args([ffi.int(3), ffi.str("x")]))` — that sidesteps the language gap with an explicit
per-arg-type list. (b) is the lower-risk, FFI-contained option.

### `bool` ↔ C `_Bool` — RESOLVED (`bool` means bool)
**Decision (shipped):** Chezzi `bool` marshals as C `_Bool` (1 byte, 0/1) — params, returns, **and**
struct fields. "bool means bool." A struct `_Bool` field now has the correct 1-byte size/offset
(closing the prior footgun where a 4-byte `bool` field mis-sized/offset-shifted a real `_Bool`); no
`int8`/`uint8` workaround is needed for a `_Bool` field anymore. There is **no separate `bool8` type** —
the earlier plan for one is mooted by this re-map.

**Predicates use `int`, not `bool`.** A C function using the pre-C99 int-as-bool idiom — `<ctype.h>`
predicates (`isdigit`, …) return an *arbitrary nonzero* `int` for true, **not** a clean 0/1 `_Bool` —
must be bound `-> int` and tested `!= 0` at the Chezzi call site. Binding such a predicate `-> bool`
would misread (the 1-byte `_Bool` narrowing keeps only the low byte: a `0x100` return reads `false`).
That's the deliberate trade of "bool means C `_Bool`": predicates are an `int` return.

**Implementation note (landed):** the 1-byte `_Bool` return **reads register-width then narrows to a
byte + `!= 0`** (the same libffi rvalue-widening rule the narrow-int returns follow — a 1-byte return
read through a 1-byte buffer is a stack OOB write). See `src/native/cffi.rs` (`ffi_type`/param/return/
`write_field`/`read_field` for `CType::Bool`).

## 2. Why strong libraries are blocked (value model, not linking)

Linking a Rust crate is trivial — Chezzi *is* Rust, so `burn = "..."` in `Cargo.toml` + a
`native_members` entry links fine. **Using** it is blocked by three walls, all in the value model /
GC airlock (`src/native/mod.rs:7-10`):

1. **No opaque-handle `Value`.** A `burn::Tensor<B,D>` is none of the `NativeRet` variants. Best you
   could do is flatten it to `List[float]` across the seam — deep-copied every call, GC-churned,
   zero-copy lost. Fatal for tensors. **This is the explicitly-deferred "userdata" item.**
2. **`NativeFn` is a stateless `fn` pointer** (`mod.rs:197`). A model / device / autodiff graph /
   optimizer has nowhere to live.
3. **GC airlock by design.** Native code never touches an `Rc`/`GcRef`. A live foreign object held by
   a Chezzi value must be GC-tracked, `Send + Sync` for `--parallel`, and survive the M:N snapshot —
   none of which a Burn tensor is automatically (backend-generic, often `!Send` on GPU).

**Honest framing:** Python's edge is *not* `ctypes` — nobody binds torch with ctypes. It's that libs
ship hand-written C-extension modules using the full CPython C-API. Chezzi's equivalent is the
**Level-2 native seam**, not `extern`. The realistic route to Burn mirrors PyO3/numpy: a **Rust binding
crate** (`tch`/`candle`/`burn` itself) exposed through Level-2 — *once the seam can carry a handle.*

## 3. The bridge: an opaque-handle Value ("userdata")

One feature unlocks every handle-based Rust/C library. Four layers:

1. **Value layer** — a new heap object:
   - VM: `Obj::Native(Arc<dyn Any + Send + Sync>)`
   - GC trace = no-op (no Chezzi children); GC collect drops the `Arc` → Rust `Drop` frees the
     tensor / GPU memory. Refcount lifecycle for free.
2. **Seam layer** — extend the airlock (`src/native/mod.rs`):
   - `NativeRet::Handle(Arc<dyn Any + Send + Sync>)`
   - `Host::arg_handle(i) -> Result<Arc<dyn Any+Send+Sync>, HostError>`
   - native fn downcasts: `arg.downcast_ref::<Tensor>()`.
3. **Type layer** — reuse `seed_stdlib_structs` (`src/checker/mod.rs:447`). `Match`/`Response` are
   already **synthetic structs with no AST** — names + field layouts injected into the checker. A
   `Tensor` opaque handle seeds identically: `struct_names.insert("Tensor".into())` — nominal, no
   fields, methods only. **Zero new type-system machinery.**
4. **Binding crate** — `src/native/tensor.rs`, the PyO3/numpy adapter pattern:
   ```rust
   fn matmul(h: &mut dyn Host) -> Result<NativeRet, HostError> {
       let a = h.arg_handle(0)?.downcast_ref::<Tensor<B,2>>()...;
       let b = h.arg_handle(1)?...;
       Ok(NativeRet::Handle(Arc::new(a.matmul(b))))   // handle in, handle out — never copied
   }
   ```
   Register in `native_members("std.tensor")` + seed checker sigs.

Chezzi side:
```chezzi
import Tensor from std.tensor
t = Tensor.from([1.0, 2.0, 3.0, 4.0], [2, 2])
print(t.matmul(t))      # data lives in the Arc handle — zero-copy, no GC churn
```

Stateful APIs (model/device/optimizer) need no extra mechanism — each is just another handle.

**Two gotchas, both already-documented hazards:**
- **`Send + Sync` bound** — required for `--parallel` + the M:N snapshot. A `!Send` GPU backend → gate
  `std.tensor` to a sequential engine, or wrap behind a mutex-actor. Same shape as the FFI-7
  non-reentrant-C race note in `spec.md`.
- **Zero-copy stays inside the handle.** Tensor data never marshals to `List[float]`. That's the whole
  point — it sidesteps the GC-churn problem.

## 4. Types: enrich the *library*, never the *language*

The language keeps its simple types (scalars + list/map/set/tuple/struct/enum + Result/Option +
Iterator). Libraries add **nominal opaque types** via the seeding mechanism above — names with methods,
no structural complexity, **no new type-system features** (no const-generics, no typeclasses, no
dependent types).

Burn's real type is `Tensor<Backend, const Dim, Kind>` — generic backend, const-generic dimension,
float/int/bool kind. Chezzi can't express that. Two options:

| | Approach | Result |
|---|---|---|
| **A. Monomorphize at the boundary** *(recommended)* | one opaque `Tensor`; backend fixed in the Rust adapter; dtype + shape are **runtime** facts | numpy/CPython model — `ndarray` is one type, dtype/shape are runtime attrs. Shape mismatch → `Result[Err]`, not a compile error |
| **B. Phantom nominal variants** | `FloatTensor`/`IntTensor` as separate seeded types | a little more static safety, more boilerplate, still no shape checking |

Pick **A**. It matches the lang's own precedent (`std.json`'s `Json` is a dynamic enum, richness
validated at runtime), matches every successful dynamic-lang ML binding, and reflects that shape/dtype
errors are data-dependent — uncheckable without dependent types.

> **Rule of thumb:** language stays simple (scalars + containers + opaque handles). All library richness
> — backends, dtypes, shapes, broadcasting — lives as **runtime behavior inside the handle + `Result`
> at the boundary**, never as new static types. A library introduces *type names* (opaque), never
> *type-system features*.

## 5. Bootstrapping: won't — and FFI sits below the line either way

There is **no bootstrap/self-host plan** ("std modules *written in Chezzi*" means stdlib like
`std.str`/`std.cmp`, not the compiler). Structurally it never fully bootstraps:

- The VM is the hot core (all of M19 is squeezing it vs CPython). Rewriting it in Chezzi → it runs *on*
  a VM → catastrophically slow → kills the perf track. Self-defeating.
- Ceiling is the CPython model: **front-end self-hostable in theory (never hot), runtime native
  forever.**

```
  Chezzi compiler front-end   ← self-hostable in theory (never hot)
─────────────────────────────  ← bootstrap line
  VM / GC / scheduler          ← native forever
  native seam + handles        ← THE FFI boundary, by definition never self-hosted
```

The native seam *is* the lang/host boundary — the one thing **no** bootstrapped language self-hosts
(CPython's C-extensions stay C; Rust self-hosts but `extern`/lang-items/syscalls stay primitive). So a
Burn binding lives below the bootstrap line. **Binding Rust libs and bootstrapping never collide** —
different layers.

## 6. Package registry: two kinds, three native models

`spec.md` lists the package registry as deferred. When designed, it splits in two.

### 6.1 Pure-Chezzi packages — easy, do first
Just `.chz` source. Registry serves source; the resolver imports it. **No recompile, ever.** Already
proven (`std.str`/`std.cmp` are Chezzi). This is npm-for-source — ship it first, nearly free.

### 6.2 Native packages — the hard part
**Today: native = edit `src/native/` + `Cargo.toml` + recompile the whole binary.** The Level-2 seam is
statically linked. You **cannot** `chezzi add burn` against an installed Chezzi binary. There is no
mechanism for distributable native packages — this is the real gap, bigger than the userdata variant.

A registry serving native packages needs one of:

| Model | `chezzi add burn` does | Cost |
|---|---|---|
| **A. Recompile-the-world** (Zig-like) | native pkg = vendored Rust crate + glue; `chezzi build` links a **project-specific binary** | ABI-safe, simple; every native dep = a Rust rebuild; needs the Rust toolchain on the user machine |
| **B. Dynamic plugins** (CPython C-ext model) | pkg ships a prebuilt `cdylib` (`.so`); `chezzi` `dlopen`s it at module-init | no user rebuild — but needs a **frozen `repr(C)` ABI** for the seam |
| **C. C-ABI wrapper** (`extern "lib":`) | pkg = manifest → a system `.so` + Chezzi wrapper source | already largely built; scalars, handles, flat structs by value, **sync scalar callbacks**, **pointer-deref `load_*`/`store_*` builtins**, and the **C-buffer alloc layer** (`ffi.alloc`/`alloc_zeroed`/`free` — `qsort`/`bsearch` of a Chezzi list now fully works) today (stored/cross-thread callbacks + variadics deferred) |

### 6.3 The gotcha that decides it: Rust has no stable ABI
Model B's blocker: the `Host`/`NativeRet`/`Arc<dyn Any>` seam is a **Rust** ABI — `String`, `Vec`,
trait objects, none `repr(C)`. A plugin built against `chezzi 0.1` breaks on `0.2`. So Model B requires
first **freezing a narrow `repr(C)` plugin ABI** (an `abi_stable`-style versioned contract). That is its
own milestone.

### 6.4 Recommended staging
```
Phase 1: pure-Chezzi registry (source only)        ← cheap, first
Phase 2: C-ABI packages via extern "lib"           ← finish handles/structs; wraps ANY system .so,
         (pkg = wrapper source + a system .so)         no chezzi rebuild
Phase 3a: first-party Rust bindings (Burn)         ← stay recompile-the-world (Model A): rich Rust
          via vendored static link                     API + handles the C-ABI can't carry; Burn wants
                                                        static linking anyway (monomorphized backend, GPU kernels)
Phase 3b (optional luxury): frozen repr(C) cdylib  ← Model B, only AFTER the value model settles
```

## 7. "But Python has this" — yes, and why it works there

Python's pip = **Model B**, and it works for one reason Chezzi lacks. The pieces:

1. **C-extension modules** — numpy/torch ship compiled `.so`/`.pyd`; CPython `dlopen`s them; they link
   the CPython C-API.
2. **Wheels (PEP 427)** — prebuilt binary bundles; `pip install numpy` downloads one already compiled
   for your platform. No compiler runs on the user machine.
3. **ABI tags + manylinux** — the wheel filename encodes compatibility
   (`numpy-2.0-cp312-cp312-manylinux_x86_64.whl`): `cp312` = "CPython 3.12 ABI"; pip picks the match.
   manylinux = a frozen base-system ABI so one `.so` runs across distros.

**Why Python can and Chezzi can't (yet): C has a stable ABI; Rust does not.** CPython's seam is C — a C
struct layout / signature is stable by language guarantee, so an old `.so` still loads. Chezzi's seam is
Rust — zero ABI stability. To copy pip, Chezzi must first do what CPython did (over ~30 years: C-API +
stable-ABI/PEP 384 + wheels + manylinux): **freeze a `repr(C)` seam.** Not free — a standardization
effort.

**Three honesty caveats — Python's model is messier than it looks:**
1. **No matching wheel → pip builds from source** (sdist) — needs a C compiler. So Python *also* falls
   back to recompile-the-world. Exactly the hybrid above.
2. **torch wheels are GB-huge** — they statically bundle CUDA *into* the wheel rather than dynamic-link
   the system's. Model A vendored inside a Model B wrapper.
3. **The C-API leaks CPython internals** → the cautionary tale. Because it exposes refcounts/object
   layout, alternative Pythons (PyPy) can't run C-extensions natively — they emulate via `cpyext`,
   slow. And numpy 2.0 *broke* ABI → everything built against 1.x needed a rebuild. A "stable" native
   ABI is never perfectly stable, and a **leaky** one freezes your engine.

**The lesson for Chezzi ordering (caveat #3 applied):** a frozen seam ABI that exposes VM/GC internals
means you can **never** NaN-box or JIT without breaking every shipped plugin. So the `repr(C)` seam
must be **narrow** (handles + primitives, no VM internals) and must come **after** the value model
settles. Too early or too wide → you trap yourself exactly as CPython trapped PyPy.

The staged plan *is* the Python model with the Rust-ABI tax made explicit:
```
pure-Chezzi registry        = pip for pure-Python packages
extern "lib" C wrappers      = ctypes/cffi packages
frozen repr(C) cdylib seam   = wheels + C-extensions   ← the prereq Python got from C for free
recompile-from-source        = pip sdist fallback
```

## 8. Ordering — everything wants the value model frozen first

```
NaN-box Value  →  userdata handle variant  →  freeze narrow repr(C) plugin ABI  →  (JIT)
   (8B Value)      (boxed-ptr payload)          (Model B native packages)            against frozen ABI
                                              ↘ Model-A recompile path & pure-Chezzi
                                                registry work the whole time (no ABI needed)
```

- **Userdata is pre-JIT**, because it changes the `Value` layout the JIT would otherwise hardcode, and
  it must be co-designed with NaN-boxing (a handle is one boxed-pointer payload). Userdata and JIT are
  otherwise orthogonal in the hot path — a native call is opaque to the JIT.
- **Model A (recompile-the-world) and the pure-Chezzi registry need none of this** — static linking and
  source distribution sidestep every ABI question, so they can ship first.
- **Model B (the `repr(C)` cdylib seam) is last** — it's the only piece that hard-depends on a frozen
  value model.

## See also
- [`spec.md`](spec.md) — §"Native FFI" (Level-2/3 detail, v1 limits FFI-2/3/7), §"Still deferred".
- [`syntax.md`](syntax.md) §12b — `extern "lib":` user-facing surface.
- [`future.md`](future.md) §4 — perf levers incl. NaN-boxing and the Cranelift JIT end-game.
- `src/native/mod.rs` — the `Host`/`NativeRet`/`NativeFn` seam.
- `src/checker/mod.rs` `seed_stdlib_structs` — the nominal-opaque-type mechanism (`Match`/`Response`).
</content>
</invoke>
