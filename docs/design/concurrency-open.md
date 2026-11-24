# Concurrency — open items

Known limits and deferred work in the concurrency runtime that no ticket closes yet. The
open-row ledger is [`gaps.md`](../contributing/gaps.md). User-facing rules:
[`docs/reference/concurrency/`](../reference/concurrency/01-model.md).

## Limits that stay by design

| item | behaviour today | why it stays |
|---|---|---|
| Contended shared channel across nested nurseries | 2+ live receivers racing ONE channel across nested `parallel:` scopes get a nondeterministic delivery order, or, once every task is blocked, the deadlock report. It must never panic and never hang (`parallel_cross_nursery_contended_never_panics`). | Concurrent by design; not gated, not special-cased. |
| A wait under a host frame (an FFI callback, a map/set probe hook) with no possible sender | Demotes, waits in place and hangs, in `main` and in a spawned task (`gaps.md` W23-1). Every other wait parks its fiber, so its deadlock is reported (plan `2026-11-18-every-wait-parks`). | A wait in place counts awake in the run's deadlock count (owner, 2026-11-17), so every task is never asleep. A probe hook runs inside a map/set lookup on the Rust stack, and an FFI callback has a C frame under it, as in Go's cgo. |
| Partial deadlock | Some tasks stuck while another runs on (a loop, a ticker, a server) is not reported: the run hangs. `chezzi test --timeout` is the backstop. | Go's rule: `checkdead` fires only when every goroutine is asleep (owner, 2026-11-17). |
| `Shared.update` hold-and-wait | A closure that waits on something only another user of the same box can give deadlocks. | Won't-fix by design (Elixir `Agent` rule: keep the closure short). |
| Restart / supervision for `Executor` | An `Executor` runs and reaps tasks; it does not restart them. | Out of scope. |

## Owner decisions, not yet built (2026-10-22)

| Item | Decision | Ancestor |
|---|---|---|
| Custom heap | No user heap or arena API: `Shared[MyStruct]` gives a custom write interface over one value of the run's heap. `RwShared.read` runs on the live value with no copy; add `Frozen[T]` (build once, all tasks read in place, no lock) only when a benchmark shows the read guard's cost | Erlang `persistent_term`, Rust `Arc` |

## Open, would be fixed if a program needs it

- **`cancel.current()`.** A running task cannot read its own cancel token to poll it. The owner decided it
  (2026-10-22) and left it to a separate ticket; `t.cancel()` and `ex.cancel()` cut a job at its next wait or
  loop back-edge without it.
- **Seeded replay is exact only where that is free.** A nursery join in place, a `Shared` guard wait,
  an `Executor` join and job start, and a `recv` or `wait:` inside a native callback replay at a
  measured rate (`gaps.md` W15-10). A fix ships only if unseeded runs pay nothing for it.
- **`std.cancel` depth cost.** `cancelled()` is quadratic in token depth (2360 µs at depth 400, where
  Go's `ctx.Err()` is O(1)), and an uncancelled long-lived parent retains every derived child until it
  is cancelled (no token-drop hook).
- **Killable subprocesses (M25).** A `std.process` call ignores a sibling fault, `os.exit` and
  `chezzi test --timeout` until the child exits. Proposal:
  [`killable-subprocess.md`](killable-subprocess.md).
- **Priority classes** (a BEAM scheduler feature) are deferred.
