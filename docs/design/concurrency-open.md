# Concurrency — open items

Known limits and deferred work in the concurrency runtime that no ticket closes yet. The
open-row ledger is [`gaps.md`](../contributing/gaps.md). User-facing rules:
[`docs/reference/concurrency/`](../reference/concurrency/01-model.md).

## Limits that stay by design

| item | behaviour today | why it stays |
|---|---|---|
| Contended shared channel across nested nurseries | 2+ live receivers racing ONE channel across nested `parallel:` scopes get a nondeterministic delivery order, or a `deadlock` fault. It must never panic and never hang (`parallel_cross_nursery_contended_never_panics`). | Concurrent by design; not gated, not special-cased. |
| Blocking op in the inline `parallel:` body | A blocking `recv`/`for v in ch:`/`wait:` issued directly in the inline builder body (not in a `spawn:`) faults "deadlock — no runnable task can send". The cross-nursery fix is wake-side only. | The body has no scheduler frame. The diagnostic points at the fix: put blocking work in a `spawn:`. |
| `main` inside a native re-entry with no possible sender | Blocks in place and hangs; a spawned fiber in the same shape faults `deadlock`. | The verdict declines to judge an unregistered party (an accepted hang is a missing answer, a false fault is a wrong one). |
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
- **N10: timer arm in the inline builder body.** A `wait:` with a live timer arm in the inline body of
  an outermost `parallel:` builder inline-sleeps before it parks, so it can take the timer where a
  worker-thread waiter takes a runnable sibling's `send`. Proper fix: park first, inline-sleep the timer
  only when the quiesce path would idle-deadlock. Details:
  [`concurrency-runtime.md`](concurrency-runtime.md#wait-implementation).
- **Multi-channel `wait:` inside a native callback polls on a backoff.** The demoted waiter has N
  channel condvars and no single one to block on, so it polls on `DEMOTE_POLL_BACKOFF` instead of a
  targeted wake. Sound, lower throughput.
- **Seeded replay is exact only where that is free.** A nursery join in place, a `Shared` guard wait,
  an `Executor` join and job start, and a `recv` or `wait:` inside a native callback replay at a
  measured rate (`gaps.md` W15-10). A fix ships only if unseeded runs pay nothing for it.
- **Deadlock verdict residuals.** An all-joiner cycle, bounded-pool starvation and PARTIAL deadlock (a
  subset stuck while the rest runs on, which Go cannot report either) are not reported (`gaps.md`
  W7-12r / W7-15).
- **The deadlock verdict reaps a parked `Executor` job without settling its handle.** Making the verdict
  a run halt is its own ticket (CHAN4).
- **`std.cancel` depth cost.** `cancelled()` is quadratic in token depth (2360 µs at depth 400, where
  Go's `ctx.Err()` is O(1)), and an uncancelled long-lived parent retains every derived child until it
  is cancelled (no token-drop hook).
- **Killable subprocesses (M25).** A `std.process` call ignores a sibling fault, `os.exit` and
  `chezzi test --timeout` until the child exits. Proposal:
  [`killable-subprocess.md`](killable-subprocess.md).
- **Priority classes** (a BEAM scheduler feature) are deferred.
