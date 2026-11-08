# Chezzi — Design and Ergonomics Backlog

Open limitations found while writing real programs: missing surface and deferred scope. Live BUG rows
are in [`gaps.md`](gaps.md). Closed entries are in git. Speculative
tracks are in [`roadmap.md`](../design/roadmap.md); perf numbers are in [`benchmarks.md`](benchmarks.md).

Legend: 🔴 blocks apps · 🟡 friction · ⚪ latent (unreachable) · 🟢 works.

Each entry was checked against the code on 2026-10-16.

## Type system and runtime

- **⚪ Module-scoped type error strings leak the qualified key** (verified 2026-06-20). A few defensive
  errors interpolate the raw `mod::Point` key instead of the bare display name. No well-typed program
  reaches them (the checker gates index, slice and iteration on the protocol method). The VM sites
  cited first (`vm/mod.rs:12121`) moved; `Vm::type_name` (`src/vm/display.rs`) prints `struct`, and
  several VM sites already call `crate::base::words::bare_display`. Fix: wrap each remaining key in
  `bare_display(...)`.

## Language-level

- No `byte`/`u8` scalar (`i64` only).
- No non-UTF-8 codecs (latin1, utf16).
- No `tuple()` constructor (`bool()` exists).
- No `type()`/`typeof`.
- `bignum` stays a non-goal. `**` and `//` are absent by design (no base operator).

## Surface ergonomics / DX

From a fresh-dev audit (2026-06-23). None of these blocks an app, but each is a first-hour stumble.

1. **Method vs free-function split** (highest friction). The str half is fixed: `ends_with`,
   `replace`, `repeat`, `reverse`, `pad_left`, `index_of`, `count`, `strip_prefix`, `strip_suffix`,
   `split_lines` and `strip` are str methods. The list half is open:
   - `map`, `filter`, `fold`, `sum`, `sort`, `contains`, `index_of`, `concat`, `min`, `max` are List
     methods.
   - `enumerate`, `zip`, `any`, `all`, `find`, `flatten`, `reduce`, `take`, `drop` are `std.iter` free
     functions (`std/iter.chz`).
   - Two-argument `min`, `max` and `clamp` are `std.cmp` free functions (`std/cmp.chz`).
   - Fix: (a) add the common `std.iter`/`std.cmp` operations as receiver methods (thin forwarders), or
     (b) document one hard rule.
2. **Docs clarity.** `/` on ints is integer division, and `%` and `/` truncate toward zero
   (`-7 % 3 == -1`, `-7 / 2 == -3`). This is correct by design but conflicts with the "Python-feel"
   label (`7/2 == 3.5`, `-7 % 3 == 2` in Python). Add a loud one-line note to the syntax reference
   operators section so that a reader sees it before a silent wrong result.

## Tooling (ecosystem)

No `chezzi repl`, no `chezzi fmt`, no package manager or registry, no debugger, no doc comments or
docgen. The LSP exists (`src/bin/chezzi-lsp.rs`). Details: [`roadmap.md` §3](../design/roadmap.md#3-language-and-tooling-gaps).

## Performance

The ranked list is in [`roadmap.md` §4](../design/roadmap.md); current numbers are in
[`benchmarks.md`](benchmarks.md). Open items carried from this backlog:

- **GC contract before the JIT.** Generational or incremental GC needs write barriers and safepoint
  placement in codegen. Lock the GC contract before codegen.
- **HOF borrow-release clone.** `map`/`filter`/`fold` clone the list to release the heap borrow before
  `invoke_value`. Fix: split `Vm` into `&mut ExecState` + `&Heap`. Structural refactor.
- **`for`-loop snapshot (`ListClone`) and per-char allocation.** Blocked by the observable snapshot
  semantics of the `for` loop.
- **String concat / `split` builder.** `join` already buffers; `+` and `split` are not benched.
- **Cranelift method-JIT** (end game, roadmap §4 #6): the only path to match CPython on compute.

## Deferred: real work, lower urgency

- **FFI surface** (`std.ffi`, `src/native/cffi.rs`; spec.md §FFI). Shipped: scalars, fixed-width
  ints, `ptr`, `owned_str`, `str?`, flat and nested structs by value, `float32`, varargs, sync scalar
  callbacks, `load_*`/`store_*`, `alloc`/`free`. Still deferred: `str` struct fields, stored or
  cross-thread callbacks, a Rust `Box<dyn Any>` userdata handle, a custom user-named deallocator.
  Design: [`ffi-and-packaging.md` §1b](../design/ffi-and-packaging.md).
- **Graceful shutdown of accept loops**, plus a per-connection handler -> acceptor signal channel.
- **Reduction-constant tuning** (D3): choose `CONTEXT_REDS` and per-op vs per-back-edge accounting.

## By-design non-goals (revisit only if it bites)

- **`Shared.update` same-box hold-and-wait.** `update(f)` holds `update_lock` across `f`, so a `recv`
  that needs the SAME box deadlocks. This is the general hold-and-wait class (Go #13759, Rust
  `clippy::await_holding_lock`; BEAM has no shared locks). Rule: do not block on a value that needs the
  same box. `update` stays: it is the only atomic RMW, and dropping it brings back the lost-update
  race. A future option is a `share` binding modifier plus a lint.
- **BEAM-flavored concurrency:** priority classes, restart/supervision (out of scope, C5); a contended
  shared channel (concurrent-divergent by design), inline-body blocking recv, eager-nursery cross-wake.
- **`int32`/unsigned C ints as scalars:** none; FFI widens at the boundary.
- **Defaults and named args on built-in methods** (`map`, `push`, `len`): by design. User methods,
  functions and constructors have them.
