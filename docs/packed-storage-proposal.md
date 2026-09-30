# Proposal: packed data and numeric kernels

**Status:** design draft. This is an additive experiment; it does not change `struct` or `List[T]` semantics.

## Goal

Measure whether storing small struct fields inline improves memory use and sequential processing enough to justify a language feature. Today `List[P]` stores 8-byte handles to separately allocated 64-byte GC slots. The existing two-million-point benchmark uses about 149 MB, versus 67 MB for Go's inline slice of points (`docs/benchmarks.md`).

A local three-run probe of the installed release binary (2026-09-28, process startup included) shows why layout alone is not a complete speed plan:

| Workload | Go median | Chezzi median | Go peak RSS | Chezzi peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Build two million two-int structs, print length | 0.070 s | 0.858 s | 64.0 MB | 150.2 MB |
| Push and sum two million ints | 0.040 s | 0.788 s | 44.7 MB | 39.3 MB |

These are small microbenchmarks on one machine, not an application-level ranking. The struct case shows the allocation and live-object cost; the int case has compact storage already and still spends much more time executing the element loop in the VM. Reproduce with `benches/chz/many_struct.chz`, `benches/chz/list.chz`, and their `benches/go/` twins.

## First experiment: `PackedList[T]`

`PackedList[T]` is a growable sequence whose elements are **copies**, not shared struct objects. It is deliberately a distinct type because Chezzi's ordinary structs are references: `p := xs[0]; p.x = 1` must still mutate an ordinary `List` element, but must not mutate a packed element.

For v1, `T` must be a concrete user struct whose fields are all `int`, `float`, or `bool`. Nested structs, `Any`, protocol values, lists, strings, and reference fields are excluded from this experiment. Empty construction names the type: `PackedList[Point]()`. The checker rejects an ineligible layout at construction.

```chezzi
struct Point:
    x: int
    y: int

points := PackedList[Point]()
p := Point(1, 2)
points.push(p)                 # copy x and y into packed storage
p.x = 9                       # points[0].x remains 1
q := points[0]                # detached Point copy
q.y = 8                       # points[0].y remains 2
points[0] = q                # replace the packed element
```

V1 operations: `len`, `push`, `pop -> Option[T]`, indexed read and replacement, and iteration. Reads and `pop` materialize a fresh ordinary `T`; insertion and replacement copy its fields. `points[i].x = ...` is rejected by the checker because it would mutate a temporary; read, modify, and assign back instead. An ordinary `List[T]` does not convert implicitly to or from `PackedList[T]`.

The compiler must carry the concrete `T` layout to construction; generic type arguments are currently erased. The VM stores a struct type ID, length, and a growable fixed-stride field buffer. A two-int `Point` takes 16 bytes per element in that buffer, with no per-element GC slot. The v1 scalar-only payload needs no GC child tracing. At a task or channel boundary, serialize the logical elements and reconstruct an independent packed sequence, following existing airlock copy rules.

This first experiment targets **memory and allocation count**. Its indexed reads allocate a detached struct, so a loop that reads every element may be slower. That outcome should be measured honestly rather than hidden by a custom benchmark that only appends and counts.

## Benchmarks and decision gate

Compare `List[Point]`, `PackedList[Point]`, and Go's `[]Point` at the same element count. Measure peak RSS, build time, sequential field sum, indexed updates, and mixed reads and writes. Check identical outputs before timing. Include a small input and a two-million-element input; run repeated release builds on the same machine. Add correctness tests for copy behavior, list growth, replacement, bounds, iteration, task crossing, and GC pressure.

If packed storage helps only the append-and-count case while slowing common scans, stop or redesign the read path. If it helps representative workloads, then decide whether to keep the explicit collection or consider Go-like value semantics for `struct`. Do not change the default meaning of `struct` solely on an RSS result.

## Where arena allocation fits

A frame arena can reduce allocation and GC work for temporary structs whose references never escape a call. It does not remove the per-element slots in `many_struct`: those objects live in a list after their constructors return. Measure a temporary-struct workload separately; do not credit an arena with the packed-list benchmark's expected memory gain. The more general escape-analysis and inline-container design is in `docs/design-value-structs.md`.

## SIMD comes after dense numeric storage

The interpreter executes Chezzi loops one value at a time. A SIMD syntax or vector lane type would not make boxed, indirect data contiguous. First provide dense numeric storage and a few whole-buffer operations such as elementwise addition, dot product, and reduction. Implement scalar kernels first; benchmark optimized Rust kernels and platform-specific dispatch only where measurements justify it.

As a small control, replacing the user loop in `benches/chz/list.chz` with the existing `xs.sum()` while keeping construction identical cut the local three-run median from 0.779 s to 0.381 s. This is a whole-buffer operation without a new SIMD language feature; it shows that avoiding per-element interpreter dispatch already matters.

Keep observable behavior stable: integer overflow must still fault, and floating-point reductions must keep a defined order unless an explicitly named fast operation permits reassociation. Keep a scalar fallback for unsupported CPUs and short buffers. Revisit a user-facing SIMD type only if users need operations that whole-buffer APIs cannot express.
