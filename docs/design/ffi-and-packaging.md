# Chezzi — FFI deepening & package distribution

**Status: proposal, not started** (stored/cross-thread callbacks, the opaque-handle "userdata" value,
and the package registry). The shipped FFI surface is in
[`spec.md`](../reference/spec/01-overview.md) "Native FFI" and the `extern "lib":` chapter of the syntax
reference. The earlier version of this file, with the design notes of the slices that shipped, is
kept verbatim in [`history/designs/ffi-and-packaging-full.md`](../history/designs/ffi-and-packaging-full.md).

## TL;DR

- **The FFI has two seams**, both stateless (value in, value out): Level-2 compiled-in Rust natives,
  and Level-3 `extern "lib":` (dlopen + libffi).
- **Strong libraries (Burn, torch-class) are blocked, even Rust ones.** The blocker is the value
  model, not linking: no `Value` can hold a live foreign object, so a stateful `Tensor`/`Model` has
  nowhere to live.
- **The fix is one feature: an opaque-handle value ("userdata")**, typed through the existing
  `seed_stdlib_structs` nominal-type mechanism. Land it pre-JIT, because it changes the value layout.
- **A package registry splits in two.** Pure-Chezzi packages are source, no recompile. Native
  packages are the hard part: pip/wheels work for Python because C has a stable ABI, and Rust does
  not. Chezzi must first freeze a narrow `repr(C)` plugin seam.

---

## 1. Where FFI stands

| | Level-2 native seam | Level-3 dynamic C-ABI (`extern "lib":`) |
|---|---|---|
| Mechanism | Rust `fn` compiled into the `chezzi` binary, registered in `native_members` | `dlopen` + `dlsym` + `libffi` at module init |
| Lives in | `src/native/` (`Host`/`NativeRet`/`NativeFn`) | `src/native/cffi.rs` |
| Used by | `std.math`/`io`/`os`/`fs`/`time`/`regex`/`request`/`net` | user `extern "lib":` blocks |
| Crosses | `NativeRet`/`NativeArg` (primitives, list, struct, map, `T!E`, `T?`) | C scalars, fixed-width ints, `float32`, `bool` ↔ `_Bool`, `str` → `char*`, opaque `ptr`, flat-scalar structs by value, C varargs, sync scalar callbacks |
| State | none: `NativeFn` is a bare `fn` pointer | none |
| Recompile to add? | yes: statically linked | no: dlopen at runtime |

Both seams follow the CPython built-in-C-module model. Neither can hold a live foreign object across
calls. The checker is the one extern-type resolver: it records the resolved, width-bearing C type of
each param and return, and both backends read that table.

**Shipped slices** (design notes in the history record): sync scalar callbacks; pointer-deref
`ffi.load_*`/`store_*`; the C-buffer layer `ffi.alloc`/`alloc_zeroed`/`free`; C variadics
(TICKET-217); `bool` = C `_Bool`.

## 1b. Open FFI items

### Stored and cross-thread callbacks

A callback that C keeps and calls after the extern call returns, or calls from its own thread.
Event-driven libraries need it (GLib/GTK signals, libuv, libcurl progress, SDL audio, GLFW input,
`signal`, `atexit`).

**Today the deferral is loud, not UB.** When the extern call returns, the trampoline is disarmed and
leaked. A later call from C prints this to stderr and calls `abort()` (SIGABRT, shell status 134):

```
chezzi FFI: callback invoked after the extern call that received it returned; stored/cross-thread callbacks are not supported
```

A call from another thread during the extern call aborts with
`chezzi FFI: callback invoked from a thread other than the one that made the extern call; stored/cross-thread callbacks are not supported`.
Each callback-passing call leaks one trampoline + CIF + userdata (~400 B and a share of a W^X page
pair). When libffi's pool cannot grow, the call fails with the recoverable error
`cannot allocate a callback trampoline for argument N to 'f': the FFI closure pool is exhausted`.
Upgrade path for the leak: cache one trampoline per (closure identity, signature).

**Two new pieces are needed:**

- **A callback registry** that GC-roots the closure and its captures until an explicit
  `unregister`; C gives no "done" signal. `ctypes` leaves this to the user and crashes when the user
  forgets; a registry makes Chezzi safe by construction.
- **Thread-safe VM re-entry.** This is the gating decision. `ctypes` relies on the GIL. The M:N
  engine has no GIL. Options: a mini-GIL (one global lock for all C → Chezzi re-entry), or
  thread-marshalling that moves the call onto the owning fiber's thread (Node N-API
  `threadsafe_function`, JNI `AttachCurrentThread`).

The callback fault rule carries forward: a faulting callback writes a zeroed return value, and the
fault is re-raised as the extern call's own error (`ctypes` prints to stderr and returns 0).

### C buffers

Open: a GC-tracked, auto-freed owned-buffer type (bounds and lifetime tracking), bulk list ↔ buffer
copy helpers (`write_ints`/`read_ints`), and `ffi.realloc`. Today double free, use after free and
out-of-bounds `load_*`/`store_*` are documented UB.

## 2. Why strong libraries are blocked

Linking a Rust crate is easy: add it to `Cargo.toml` and a `native_members` entry. Using it hits three
walls in the value model and the GC boundary (`src/native/mod.rs`):

1. **No opaque-handle `Value`.** A `burn::Tensor<B,D>` fits no `NativeRet` variant. Flattening to
   `List[float]` deep-copies every call and loses zero-copy. Fatal for tensors.
2. **`NativeFn` is a stateless `fn` pointer.** A model, device, autodiff graph or optimizer has
   nowhere to live.
3. **The GC boundary.** Native code never touches a `GcRef`. A foreign object held by a Chezzi value
   must be GC-tracked and `Send + Sync`: every M:N worker can reach it through the run's one heap. A Burn tensor is
   none of these by default (often `!Send` on GPU).

Python's strength is hand-written C-extension modules on the full C-API, not `ctypes`. Chezzi's
equivalent is the Level-2 seam. The route to Burn mirrors PyO3/numpy: a Rust binding crate exposed
through Level-2, once the seam can carry a handle.

## 3. The bridge: an opaque-handle value ("userdata")

1. **Value layer:** a heap object `Obj::Native(Arc<dyn Any + Send + Sync>)`. GC trace is a no-op;
   collection drops the `Arc`, and Rust `Drop` frees the foreign memory.
2. **Seam layer** (`src/native/mod.rs`): `NativeRet::Handle(Arc<dyn Any + Send + Sync>)` and
   `Host::arg_handle(i) -> Result<Arc<dyn Any+Send+Sync>, HostError>`; the native fn downcasts.
3. **Type layer:** reuse `seed_stdlib_structs` (`src/checker/mod.rs`). `Match`/`Response` are
   already synthetic structs with no AST. A `Tensor` handle seeds the same way: nominal, no fields,
   methods only. No new type-system machinery.
4. **Binding crate:** `src/native/tensor.rs`, the PyO3/numpy adapter pattern:
   ```rust
   fn matmul(h: &mut dyn Host) -> Result<NativeRet, HostError> {
       let a = h.arg_handle(0)?.downcast_ref::<Tensor<B,2>>()...;
       let b = h.arg_handle(1)?...;
       Ok(NativeRet::Handle(Arc::new(a.matmul(b))))   // handle in, handle out — never copied
   }
   ```

```chezzi
import Tensor from std.tensor
t = Tensor.from([1.0, 2.0, 3.0, 4.0], [2, 2])
print(t.matmul(t))      # data lives in the Arc handle — zero-copy, no GC churn
```

Stateful APIs (model, device, optimizer) are more handles; no extra mechanism.

**Hazards:** the `Send + Sync` bound is required by the M:N workers. A `!Send` GPU
backend must sit behind a mutex-actor or be gated. Tensor data never marshals to `List[float]`; it
stays in the handle.

## 4. Types: enrich the library, never the language

The language keeps its simple types. Libraries add **nominal opaque types** (names with methods), with
no const generics, typeclasses or dependent types. Burn's `Tensor<Backend, const Dim, Kind>` cannot
be expressed, so:

| | approach | result |
|---|---|---|
| **A. Monomorphize at the boundary** (recommended) | one opaque `Tensor`; backend fixed in the adapter; dtype and shape are runtime facts | the numpy model; a shape mismatch is a runtime error |
| B. Phantom nominal variants | `FloatTensor`/`IntTensor` as separate seeded types | a little more static safety, more boilerplate, no shape checks |

Pick A. It matches `std.json`'s `Json` (a dynamic enum validated at runtime) and every successful
dynamic-language ML binding. Shape and dtype errors depend on data. **Rule:** a library introduces
type names (opaque), never type-system features.

## 5. Bootstrapping: no plan, and FFI sits below the line

There is no self-host plan. The VM is the hot core; a VM written in Chezzi runs on a VM and kills the
perf track. The ceiling is the CPython model: front end self-hostable in theory, runtime native.

```
  Chezzi compiler front-end   ← self-hostable in theory (never hot)
─────────────────────────────  ← bootstrap line
  VM / GC / scheduler          ← native forever
  native seam + handles        ← THE FFI boundary, by definition never self-hosted
```

A Burn binding lives below the line, so binding Rust libraries and bootstrapping never collide.

## 6. Package registry: two kinds, three native models

### 6.1 Pure-Chezzi packages: do first

`.chz` source only. The registry serves source and the resolver imports it. No recompile.

### 6.2 Native packages: the hard part

Today a native module means editing `src/native/` and `Cargo.toml` and recompiling the binary. You
cannot `chezzi add burn` against an installed binary.

| model | `chezzi add burn` does | cost |
|---|---|---|
| **A. Recompile-the-world** (Zig-like) | vendored Rust crate + glue; `chezzi build` links a project-specific binary | ABI-safe and simple; every native dep is a Rust rebuild; needs the Rust toolchain |
| **B. Dynamic plugins** (CPython C-ext) | prebuilt `cdylib`; `chezzi` dlopens it at module init | no user rebuild; needs a frozen `repr(C)` seam ABI |
| **C. C-ABI wrapper** (`extern "lib":`) | manifest → a system `.so` + Chezzi wrapper source | mostly built (see §1); stored/cross-thread callbacks abort (§1b) |

### 6.3 Rust has no stable ABI

The `Host`/`NativeRet`/`Arc<dyn Any>` seam is a Rust ABI (`String`, `Vec`, trait objects). A plugin
built against `chezzi 0.1` breaks on `0.2`. Model B needs a narrow, versioned `repr(C)` plugin ABI
first (an `abi_stable`-style contract). That is its own milestone.

### 6.4 Staging

```
Phase 1: pure-Chezzi registry (source only)        ← cheap, first
Phase 2: C-ABI packages via extern "lib"           ← wraps ANY system .so, no chezzi rebuild
Phase 3a: first-party Rust bindings (Burn)         ← recompile-the-world (Model A); Burn wants
          via vendored static link                     static linking anyway
Phase 3b (optional): frozen repr(C) cdylib         ← Model B, only AFTER the value model settles
```

## 7. Python's model, and why it works there

pip is Model B: C-extension modules, prebuilt wheels (PEP 427), and ABI tags + manylinux
(`numpy-2.0-cp312-cp312-manylinux_x86_64.whl`). It works because **C has a stable ABI and Rust does
not.** CPython took ~30 years (C-API, stable ABI / PEP 384, wheels, manylinux) to get there.

Three caveats:

1. No matching wheel → pip builds from source (sdist) with a C compiler: Python also falls back to
   recompile-the-world.
2. torch wheels bundle CUDA statically: Model A inside a Model B wrapper.
3. The C-API leaks CPython internals, so PyPy emulates it (`cpyext`, slow), and numpy 2.0 broke ABI.

**Lesson:** a seam ABI that exposes VM/GC internals blocks every later value-model or JIT change. Keep
the `repr(C)` seam narrow (handles + primitives) and freeze it only after the value model settles.

```
pure-Chezzi registry        = pip for pure-Python packages
extern "lib" C wrappers      = ctypes/cffi packages
frozen repr(C) cdylib seam   = wheels + C-extensions   ← the prereq Python got from C for free
recompile-from-source        = pip sdist fallback
```

## 8. Ordering

```
8B Value (done)  →  userdata handle variant  →  freeze narrow repr(C) plugin ABI  →  (JIT)
                                               ↘ Model-A recompile path & pure-Chezzi
                                                 registry work the whole time (no ABI needed)
```

- **Userdata is pre-JIT:** it adds a `Value` kind that the JIT would otherwise hard-code. A native call
  is opaque to the JIT, so they are otherwise independent.
- **Model A and the pure-Chezzi registry need no ABI work** and can ship first.
- **Model B is last:** it is the only piece that needs a frozen value model.

## See also

- [`spec.md`](../reference/spec/01-overview.md): "Native FFI" (Level-2/3 detail, v1 limits FFI-2/3/7) and
  "Still deferred".
- [`roadmap.md` §4](roadmap.md#4-optimizations-open-levers): perf levers and the Cranelift JIT.
- `src/native/mod.rs`: the `Host`/`NativeRet`/`NativeFn` seam.
- `src/checker/mod.rs` `seed_stdlib_structs`: the nominal opaque-type mechanism.
