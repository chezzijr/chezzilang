## 6c. `timer(ms)` — the one-shot timeout channel

`timer(ms)` returns a `Channel[bool]` that becomes ready (`recv()` → `?true`) once `ms` milliseconds have
elapsed. It is the **composable timeout primitive**: instead of a bespoke timeout argument on `recv`, a
timeout is another channel you can receive from — and race against real channels in a `wait:`
([§6d](#6d-wait--racing-multiple-channel-receives)).

`timer` is an ordinary `std.time` function: `time.timer(ms)` after `import std.time`, or
`import timer from std.time` (an alias works, and `f := time.timer` is a function value).

```chezzi
import timer from std.time
t := timer(500)
print(t.recv())            # true — blocks ~500ms; recv gives `?true`, which prints as `true`
```

It is **level-triggered**: any `recv` at or after the deadline yields `?true` (the typical use recvs it
once). Delivery is handled at `recv` time, in the receiver's own engine, so a `timer` created at the top
level can be `recv`'d inside a spawned task. A fiber receiver parks and a background job (on the
netpoller timer thread) `send`s `true` at the deadline, accounted so it can't trip a false deadlock. A
waiter that owns its OS thread — an `Executor` job, or the top-level `main` thread, **including inside
a native callback** — does **not** park: it blocks with the timer as one more arm, so a sibling value
that arrives first wins.

**One shape still inline-sleeps.** A `wait:` reached in the INLINE body of an outermost `parallel:`
builder has no worker loop under it to drive a park, so a live timer arm there sleeps to the deadline
and takes it **without re-polling siblings** — a runnable sibling that could satisfy another arm is
stranded until the timer fires. [`gaps.md`](../../contributing/gaps.md) **N10** records this.

---

## 6d. `wait` — racing multiple channel receives

`wait` is Chezzi's `select`: block until **whichever of several channels is ready first**, run that
arm. Both directions are supported — **recv-arms** (`x := ch.recv():`, bind the received value) and
**send-arms** (`ch.send(v):`, no `:=`/`=`, run the body once the send goes through). A send-arm is ready
when the channel can accept the value: a **bounded** channel with a free slot, an **unbounded** channel
(always), or a **closed** channel — a send-arm on a closed channel is *selected and faults*
`"send on a closed channel"` (Go's panic-on-send-to-closed), never skipped. Combined with `timer`,
`wait` subsumes a bounded-wait `recv` (`ch.recv_timeout(500)` ≡ a `wait` over `ch` and `timer(500)`),
which is why no separate `recv_timeout` exists. See `examples/wait_select.chz` and
`examples/wait_send.chz`.

```chezzi
wait:
    v := orders.recv():        handle(v)        # recv-arm: arm-local binding `v: T` (the element type)
    result = cancels.recv():   result = "x"     # `=` assigns an existing outer lvalue instead
    outbox.send(next):         sent += 1        # SEND-arm: fires once `outbox` can accept `next`; binds nothing
    _ := timer(500).recv():    on_timeout()      # `_` discards; a timer arm is just a recv
    else:                      poll_miss()       # optional, non-blocking; if no arm is ready, run this
```

**Selection is deterministic SOURCE ORDER** (first ready arm wins, recv OR send), **not** Go's
uniform-random `select` fairness. This is a deliberate divergence from Go: a `wait:` over
already-ready arms gives the same answer on every run and at every worker count, which makes a
`wait:`-using program goldenable. The one shape that is still schedule-sensitive is a live `timer` arm
reached in an inline outermost-`parallel:` body (N10, above). All arm channel handles and send values
are evaluated **once**, top to bottom, on entry (Go's rule). `else` runs only if **no** arm is ready —
so a `wait` containing an unbounded or closed send-arm never blocks (that arm is always ready).

**Surface & grammar.** A compound statement (sibling to `match`/`parallel`):
`wait : NEWLINE INDENT <arm>+ DEDENT`. An arm is one of:

- a **recv-arm** `<target> ( ":=" | "=" ) <chanExpr> ".recv()" ":" <block>` — the RHS **must** be a
  `.recv()` on an expression of type `Channel[T]` (a non-`.recv()` RHS like `v := fn():` is a compile
  error). The `<chanExpr>` can be any expression (`ch`, `chans[i]`, `get_chan()`, `timer(500)`),
  evaluated **once**. The target is `:=` (a fresh arm-scoped binding), `=` (assign an existing outer
  lvalue — arm bodies are lexical sub-scopes, not closures, so outer mutation is normal), or `_`
  (discard).
- a **send-arm** — a *bare* `<chanExpr> ".send(" <value> ")" ":" <block>` (no `:=`/`=`). The grammar
  accepts any bare `<expr> <block>`; the **checker** enforces the exact `chan.send(value)` shape with
  `chan: Channel[T]`, `value: T` (a bare arm that isn't a `.send()` — e.g. `try_send`, an arbitrary
  call — is rejected with the legal-forms error). A send-arm binds nothing.
- `else:` — optional, at most one, and must be **last**.

**Guards.** A recv-arm or send-arm may end its head with `if <guard>` before the `:` —
`j := jobs.recv() if not paused:`, `n = c.recv() if ok:`, `_ := c.recv() if g:`,
`out.send(v) if ready:`. `else` takes no guard (`else if c:` is a parse error). A false guard switches
the arm **off** for this `wait:`, the way Go switches a `select` case off with a `nil` channel; the
rule is tokio `select!`'s precondition (`, if cond`) and Ada's `when` guard.

- The guard is a `bool` (`wait guard must be bool` otherwise). It is checked before the arm binds, so
  it cannot see the arm's own binding (`j := c.recv() if j > 0:` is `unknown name 'j'`).
- Guards run **once** on entry, top to bottom, interleaved with the operands: arm 1's guard, then
  (only if true) arm 1's channel and value, then arm 2's guard, and so on. An off arm's channel and
  value expressions do not run.
- An off arm is never ready, and it is **not** a closed arm: `"wait: all channels closed"` judges the
  enabled arms only.
- With every arm off: an `else` runs; with no `else`, the `wait:` faults at once, before any park,
  `"wait: every arm is off (its guard is false) and there is no else"` — guards are not re-checked,
  so nothing could ever finish it (tokio panics here too). The fault is recoverable, like the
  all-closed fault.

```chezzi
paused := false
while true:
    wait:
        j := jobs.recv() if not paused: run(j)
        p := ctl.recv():                paused = p
```

**`break` and `continue` in an arm** act on the enclosing loop, as in Rust's `select!`: a `wait:` is
not a loop, so `while true: wait: _ := c.recv(): break` leaves the `while`. With no enclosing loop,
`break` is the `break outside loop` type error. Go's `break` in a `select` case leaves only the
`select`, so a Go port drops a `break` that ends a case.

**Type-check.** A recv-arm's `chanExpr: Channel[T]` → the target binds/assigns `T`; a send-arm's
`chan: Channel[T]` → its `value` must be assignable to `T` (same check as a plain `ch.send(v)`). `wait`
is **not** exhaustive (it's a runtime race, not a type match); ≥1 recv or send arm is required.

**Runtime semantics.**

1. Evaluate each arm's guard, then (if it is true) its channel handle (and, for a send-arm, its
   value) once, in source order. A false guard switches the arm off (see **Guards**).
2. Poll arms in **source order**: the first **ready** arm wins. A recv-arm is ready with a queued
   value → pop, bind, run its block. A send-arm is ready when the channel can accept the value
   (bounded-with-space / unbounded / closed) → enqueue (or, if closed, *fault*
   `"send on a closed channel"`), run its block, bind nothing.
3. A **closed + empty** channel's *recv*-arm is **skipped**; a closed *send*-arm is instead *selected
   and faults* (asymmetric — Go's panic-on-send-to-closed). If *every* recv-arm is closed+empty,
   there's no ready send-arm, and there's no `else`, the `wait` faults `"wait: all channels closed"`.
   Off arms do not count; with every arm off and no `else` it faults
   `"wait: every arm is off (its guard is false) and there is no else"` instead.
4. If no arm is ready: with an `else`, run it (non-blocking); otherwise **block** — park on *all* live
   arm channels and re-poll on the first wake. A recv-arm wakes on a **sender**; a bounded send-arm
   wakes on a **receiver** freeing a slot.

**Where a blocking `wait:` waits.** The mode depends on the waiter (full table:
[06-blocking.md](06-blocking.md#blocking-context-table)):

| waiter | untimed `wait:` | `wait:` with a live timer arm |
|---|---|---|
| a spawned fiber | parks on every arm's channel | parks; the timer is one more bucket |
| a fiber inside a native callback (`Shared.update` closure, list HOF) | demotes and blocks in place | demotes; the deadline bounds the wait |
| a thread that owns itself (`main`, an `Executor` job) | blocks in place | blocks in place, clamped to the deadline |
| the inline body of an outermost `parallel:` builder | refuses (deadlock fault) | sleeps to the deadline, then takes the timer arm (N10) |

- A value that arrives before the deadline wins the wait; the deadline wins only if nothing else did.
- A send-arm that cannot move its value inside a native callback demotes and blocks on its offer, as
  Go blocks — like a plain full `send` there.
- `main` inside a native callback blocks in place for any `wait:`. Nothing judges that context for a
  deadlock, so a `wait:` there that nothing can satisfy hangs (a deliberate difference from Go, see
  [06-blocking.md](06-blocking.md#blocking-context-table)).

Implementation notes (`Op::WaitPoll`, the multi-channel park, the timer arm):
[`concurrency-runtime.md`](../../design/concurrency-runtime.md#wait-implementation).
