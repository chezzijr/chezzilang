# Chezzi — FFI deepening & package distribution (design, NOT scheduled)

> Status: **brainstorm / design only.** Nothing here is on the M19 (perf-only) milestone. The language
> is feature-frozen; this is the forward map for *if/when* it unfreezes. It exists so
> [`spec.md`](spec.md) §"Still deferred" points at a real plan instead of just the word "deferred".
> Captures the reasoning from the FFI/packaging design discussion (2026-06).

## TL;DR

- **Today's FFI has two seams.** Level-2 (compiled-in Rust bindings via `NativeFn`/`Host`/`NativeRet`)
  and Level-3 (`extern "lib":` dynamic C-ABI via dlopen+libffi, scalars only). Both are *stateless,
  value-in / value-out*.
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
| Crosses the airlock | `NativeRet`/`NativeArg` (primitives, list, struct, map, Result/Option) | scalars only (int↔long, float↔double, bool↔int, str→`char*`) |
| State | **none** — `NativeFn` is a bare `fn` pointer, no captured state | **none** |
| Recompile to add? | **yes** — statically linked | **no** — dlopen at runtime |

Both seams are the **CPython-built-in-C-module model**: stateless functions, data in / data out. Neither
can hold a live foreign object across calls.

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

1. **Value layer** — a new heap object on each engine:
   - VM: `Obj::Native(Arc<dyn Any + Send + Sync>)`
   - interp: `Value::Userdata(Rc<dyn Any>)`
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
statically linked. You **cannot** `chezzi add burn` against an installed interpreter. There is no
mechanism for distributable native packages — this is the real gap, bigger than the userdata variant.

A registry serving native packages needs one of:

| Model | `chezzi add burn` does | Cost |
|---|---|---|
| **A. Recompile-the-world** (Zig-like) | native pkg = vendored Rust crate + glue; `chezzi build` links a **project-specific binary** | ABI-safe, simple; every native dep = a Rust rebuild; needs the Rust toolchain on the user machine |
| **B. Dynamic plugins** (CPython C-ext model) | pkg ships a prebuilt `cdylib` (`.so`); `chezzi` `dlopen`s it at module-init | no user rebuild — but needs a **frozen `repr(C)` ABI** for the seam |
| **C. C-ABI wrapper** (`extern "lib":`) | pkg = manifest → a system `.so` + Chezzi wrapper source | already half-built; **scalar-only** today (needs handles/structs/callbacks) |

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
