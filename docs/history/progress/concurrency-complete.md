# Progress: concurrency feature-complete record (moved verbatim from `PROGRESS.md` lines 13989-14448)

## Concurrency — feature-complete (confirmed 2026-06-12)

Core implemented through **M21** (still evolving; M19 perf in progress); **concurrency shipped through Tier-D (D0–D6c) + M-C**. The surface —
`spawn` / `parallel:` nursery / `Channel[T]` / `Shared[T]` / `Executor`, plus the VM's real OS-thread
engine and the netpoller + `std.net` — is complete and stable. **M-C implicit nurseries shipped
(2026-06-12)** — every function body and the module top level is an implicit nursery; a bare `spawn` is
legal anywhere and joins at `return`/end. ~1592 tests green; the cooperative engine (`--serial`) and the
OS-thread engine stay byte-identical on every `examples/parallel*.chz` + `examples/implicit_nursery.chz`
golden, and the frozen interp is the differential parity oracle for the sequential subset.

**CLI engine selection.** `chezzi run` now defaults to the OS-thread engine; `--serial` selects the
cooperative single-thread VM (the frozen parity oracle), `--parallel` is an accepted no-op alias, and
`--threads=N` (or env `CHEZZI_THREADS`, flag wins; `0`/omitted = all cores) sizes the OS-thread worker
pool via `vm::worker_count()`. `--threads` errors with `--serial`/`--interp` (neither is multi-threaded).

**`std.cancel` — cancellation tokens + `Channel.trip()` SHIPPED (2026-06-15).** A user-level
cooperative cancellation **`Token`** (Go-`context`-inspired, adapted): `cancel.manual()` /
`cancel.timeout(ms)`; methods `cancelled()`, `reason()` (`"cancelled"`/`"timeout"`), `done() ->
Channel[bool]` (a `wait:` arm), `cancel()` (anytime/any task), `deadline_at()`. **Tree propagation
landed** (see the next note). Pure Chezzi
(`std/cancel.chz`) over `Shared[bool]` +
`monotonic()` (deadline checked **at poll time** → timeout is deterministic across engines, no
background canceller) + ONE new native primitive **`Channel.trip()`** — a permanent level-trigger
latch (the manual-cancel fan-out a move-on-send `Channel` lacks; reuses `close()`'s wake fan-out
minus `closed`). Decoupled from the internal nursery cancel flag (so a user `cancelled()`-return runs
`defer`/`recover:` normally). Goldens: `examples/channel_trip.chz`, `cancel_manual.chz`,
`cancel_timeout_wait.chz` (byte-identical on cooperative-VM + interp); `examples/cancel_cpu.chz`
carries **no `.expected`** (manual cancel of a CPU sibling diverges by engine — default preempts,
`--serial`/`--interp` run to completion) and is covered by a Rust `#[test]`. A cross-task
cancel→`wait:` lost-wakeup regression (`MnSched::park`/`park_wait` gap re-check now includes
`done_latch`) is guarded by `cancel_trip_wakes_parked_wait_under_parallel`. Closes the `gaps.md`
cancellation gap (timeouts + manual cancel). See `docs/reference/concurrency.md` §6e/§6c'.

**`std.cancel` TREE PROPAGATION — parent/child derivation SHIPPED (2026-06-17).** `Token.derive()`
(and the free-fn `cancel.derive(parent)`) builds a **child** token (Go `context.WithCancel`):
cancelling or timing-out a parent cancels every transitively-derived child, recursively root-to-leaves,
while cancelling a child **never** touches the parent (one-directional). The link is **live** — a
parent flip is observed by an already-derived child, *including one that crossed the
`spawn`/`parallel:`/`Channel` airlock* — because the link is the parent's `Shared` flag plus a `Shared`
registry of descendant `done()` channels, which cross as live cores exactly like the flat token's `flag`
(so the feature is automatically three-engine consistent — **zero Rust changes**, no checker change:
`sendable_rec` already permits the self-referential `parent: Token?` field + `Shared`/`Channel`/`Option`
arms). A child inherits the **tightest** deadline (soonest absolute of itself + ancestors; an
already-elapsed-timeout parent yields a child cancelled at once with reason `"timeout"`, its `done()`
ready via its own timer armed to 0 ms). `done()` cascades **transitively**: `derive()` registers a
child's `done()` channel into **every ancestor's** registry (walking the parent chain to the root, each
insert an atomic `Shared.update()` so concurrent siblings don't lose updates), so a manual `cancel()` at
ANY depth above trips the descendant's `done()` directly — a grandchild parked in `wait: leaf.done()`
wakes on a grandparent cancel, not just on its immediate parent. `reason()` is nearest-cause-wins
(self's own cause, else inherited). Goldens: `examples/cancel_tree.chz` + `.expected` (byte-identical on
`run`/`--serial`/`--interp`; `golden_cancel_tree_via_run_file` VM + `golden_cancel_tree_chz` interp
twin), plus eight VM unit tests (`cancel_child_*`, `cancel_transitive_grandchild`,
`cancel_grandchild_done_ready_after_grandparent_cancel` + `cancel_great_grandchild_done_ready_after_root_cancel`
— the transitive-`done()` guards, `cancel_token_sendable_with_parent` — the cross-airlock live-link
guard). **Known v1 limit:** the per-ancestor registry only **grows** (no token-drop hook); tokens are
request-scoped/short-lived, a future prune-on-cancel could clear it. Closes the `gaps.md`
tree-propagation gap. See `docs/reference/concurrency.md` §6e.

> **RE-SHAPED TO GO'S REGISTRATION MODEL (2026-08-18).** The every-ancestor registry above is **gone**;
> the paragraph is kept as history, and its "the link is the parent's `Shared` flag plus a `Shared`
> registry of descendant `done()` channels" is **no longer how it works**. `derive()` now registers the
> child into its **immediate parent only**, via a `kids: Channel[Token]` field (an O(1) `send`), and
> `cancel()` cascades **DOWN** — an explicit work-list that drains each node's registry, marks each
> descendant, and recurses. That is Go `context.WithCancel`'s shape, and the divergence from it was a
> real cost: `derive()` was **cubic** in chain depth (100 → 40 ms, 400 → **6 020 ms**, 10 000
> unreachable, against Go's 0.15 / 0.08 / 2.9 ms) because every `Shared.update()` copied the whole
> registry list across the wire, once per ancestor. Now: 400 → **2 ms**, 1 000 → 19 ms; 5 000-wide
> fan-out → 23 ms (was ~1.5 s); with `parent` restored (below) 400 → **16 ms**, 1 000 → **85 ms**,
> fan-out unchanged. The `parent: Token?` field was **deleted and then restored the same day**
> (adversarial review A1/A2 — the cascade `try_recv()`s children OUT of the registry, so a cancelling
> task torn down mid-cascade loses that subtree PERMANENTLY: `leaf.cancelled()` false 3/3 on a 500-deep
> chain, still false after a second `cancel()`, 387 of 501 tokens lost). `cancelled()`/`reason()` fall
> back to it, so it is O(depth) again (0.30 µs at depth 0, +~0.29 µs per ancestor) and the airlock
> encoder walks the chain again: a token from a tree deeper than **4 999** hits `maximum structural
> depth (10000) exceeded` when it crosses a `spawn` (v1's boundary was ~9 990 — one structural level
> per link there, two now). `derive()` stays O(1) because the registry holds a **parent-less twin** of
> the child sharing its `flag`/`dl`/`kids` cores; registering the child itself measured 25.6 s for 300
> derives. Transitive `done()`,
> one-directionality, tightest-deadline inheritance and the `+1 ms` `done()`-margin all survive
> unchanged (`examples/cancel_tree.expected` is byte-for-byte identical). `reason()` keeps v1's
> nearest-cause-wins (own state first, else the ancestor's — the `parent` fallback is back), and gains
> a LATCH: the cause that set a token's own state stays its answer. A child of a later-deadline parent
> that is manually cancelled reads `"cancelled"` forever, where v1 flipped to `"timeout"` once that
> deadline passed; a token whose OWN deadline has already elapsed reads `"timeout"` whether the cascade
> reaches it or `cancel()` is called on it directly (`_mark`'s guard skips `flag.set`, and `cancel()`
> routes SELF through `_mark` rather than setting the flag unconditionally — adversarial-review finding
> B3, which measured `cancel.timeout(10)` + sleep + `cancel()` reporting `"cancelled"`). That matches
> the owning ancestor — Go 1.26 returns `context.Canceled` in both halves of the first case and
> `context.DeadlineExceeded` in the second (`context deadline exceeded`, measured on the paired
> program). Pinned by `reason_latches_the_first_cause` in `tests/chz/stdlib/cancel_test.chz`.
> Two ordering rules make it race-free: `cancel()` marks self (`_mark`, the work-list's first pop)
> **before** it drains, and `derive()` sends **before** it re-checks self's own state — measured 0 lost tokens over 30 × 400 concurrent derives, against
> 11 505 with the derive-side check removed. A THIRD ordering rule was missed on the first cut and fixed the
> same day: `_mark` tripped `done()` **before** setting the cancel bit, so a task woken by a cascaded
> descendant's `done()` could read `cancelled() == false` — C5, the Go-context invariant the file's own
> `derive()` comment quotes. v1 was structurally safe (the flag was set on the ancestor before any
> `trip()`, and `cancelled()` recursed up to it); pushing the flag DOWN inverted the order. Measured on
> the release binary, root→mid→leaf, 400 rounds, one task parked in `wait: leaf.done().recv()`
> re-reading `leaf.cancelled()`: **141/400 violations at `CHEZZI_THREADS=8`, 67/400 at `=4`, 0/400 at
> `=2`**; the fix is the two-line swap, and the same shape is 0/400 at `=4` and `=8` after it. New
> guards: `tests/chz/stdlib/cancel_test.chz` (8 tests,
> incl. the two anti-quadratic bounds, the race, the C5 cascade, the `reason()` latch and the
> interrupted-cascade fallback) and `cancel_cascade_crosses_the_airlock` (a far-side
> `derive()` reached by a near-side root `cancel()` — the registry is live across the airlock in **both**
> directions). C5's gate needs a PINNED worker count — the suite's two runs are the default and `=2`,
> and `=2` shows **zero** violations, so on a 1-2 core box the default IS 2 and the gate would vanish:
> `cancel_c5_gate_at_eight_workers` (`tests/chezzi_threads_cli.rs`) runs that one file at
> `CHEZZI_THREADS=8` in its own process. Retention is narrower, not gone: a `cancel()` drains what it walks, but an **uncancelled**
> long-lived parent still retains its children's handles. See `docs/contributing/benchmarks.md`.

> **`Channel.recv_timeout(ms)` — attempted then reverted (2026-06-12).** A bounded-wait `recv` was
> implemented with a **demote-always** shortcut (reuse `demote_recv_block` + a deadline) to avoid the
> heavier park+timer machinery. The review panel found it **unsound at `native_reentry == 0`**: (1) a
> top-level M:N `recv_timeout` demotes the worker, and a later reduction-budget yield strands the fiber →
> **silent hang**; (2) the cooperative park path reused `park_recv` (built for 0-arg `recv`) but
> `recv_timeout` has `argc=1` → **stack corruption** on resume; (3) cooperative-nursery no-producer faults
> `deadlock` not `None`, and demote-failure faults (not total). Reverted (commit `653dfd2`). **Lesson: the
> correct design is the heavier one** — at `native_reentry == 0`, snapshot-park on a timer (claim-flag +
> a `MnSched::timeout_wake` racing `send_wake`, like the socket-timeout `poll_timed_out` path), demote
> only at `native_reentry > 0`; cooperative needs a recv_timeout-aware quiesce (resolve-to-`None`, not
> fault) or accept the documented deadlock-fault divergence. Checker `Ty::Int → Option[elem]` sig + interp
> poll-once arm were correct; the VM scheduler integration is the hard part. A proper follow-up, not a
> drop-in. (`Atomic[T]` + `timer(ms)` have since **shipped** — see `concurrency.md` §6b/§6c,
> `examples/atomic.chz`. `wait` — Chezzi's `select` — is **designed + locked** (`concurrency.md` §6d),
> not deferred for lack of a design; it just awaits implementation as its own focused milestone.)

> **Concurrency follow-ups — `Atomic[T]` + `timer(ms)` LANDED, `recv_timeout` DROPPED, `wait` designed
> (2026-06-13).** Brainstormed the deferred trio and shipped two of three; `recv_timeout` is dropped as
> redundant.
> - **`Atomic[T]`** (commit `07ae080`) — generic atomic box mirroring `Shared[T]` (Mutex-backed, sendable
>   handle, value-first `Atomic(v)`): `load`/`store`/`exchange`/`cas` for any `T`, `add`/`sub` on numeric
>   `T` (checked-overflow like `+`/`-`). Two-engine parity; `--parallel` add/cas atomicity stress tests
>   (300-thread exact sum, 200-fiber CAS-retry). See `docs/reference/concurrency.md §6b`.
> - **`timer(ms) -> Channel[bool]`** (commit `cd1673e`) — one-shot, **level-triggered** timeout channel.
>   Delivery is scheduled **at `recv` time in the receiver's own scheduler** (NOT at construction — a
>   top-level timer can be recv'd in a `--parallel` child): `--parallel` schedules a background `send` +
>   parks (accounted `inflight` so no false deadlock); cooperative VM / interp / callbacks inline-sleep to
>   the deadline (like their `sleep_ms`). 3-engine parity. Adversarial review (Reality Checker + Code
>   Reviewer) found **no Critical/Important** — sound park-gap (reuses `MnSched::park`'s queue re-check),
>   no inflight leak (job holds Arcs + always `fetch_sub`s), no double-schedule (queue-first on re-run).
>   Known v1 limitation: `timer.recv()` inside a native callback pins a worker (no demote). `docs §6c`.
> - **`recv_timeout` DROPPED** — `wait` + `timer` subsume it (`ch.recv_timeout(500)` ≡ `wait` over `ch`
>   and `timer(500)`), and it was the unsound/reverted one. No separate primitive.
> - **`wait` (select) — SHIPPED on ALL THREE engines (2026-06-13; M:N blocking park landed 2026-06-13).**
>   Full design + grammar + per-engine semantics in **`docs/reference/concurrency.md §6d`** (cheat row in
>   `docs/reference/syntax.md §11b`; `examples/wait_select.chz`). A `wait:` compound statement races channel
>   `recv`s — arms `v := ch.recv():` (`:=`/`=`/`_` targets), optional non-blocking `else:` (last), `timer`
>   arms, recv-only (unbounded channels → sends never block); source-order priority; closed+empty arm
>   **skipped**; all-closed+no-`else` faults. **Done:** lexer→parser (`parse_wait`)→checker (`check_wait`)
>   →interp (`exec_wait`, the parity oracle)→cooperative VM (`Op::WaitPoll` + `compile_wait`), incl. the
>   **cooperative multi-channel park** (one fiber filed under N keys via `wait_suspend`/`run_child`, swept
>   out of the other buckets on resume — `vm_wait_blocks_then_wakes_on_second_channel` +
>   `vm_wait_sweeps_other_buckets_after_waking`). **M:N `--parallel` blocking park — LANDED:** a blocking
>   `wait` now parks under `--parallel` instead of faulting. ONE `WaitPark { fiber, keys, claimed }` held
>   behind an `Arc`, with a `ParkedEntry::Wait(token)` filed in every arm's `MnSched.parked[key]` bucket
>   (`MnSched::park_wait`, the N-key generalization of `park`); the first waker CASes `claimed`, takes the
>   fiber, and sweeps the stale token out of all other buckets under one core-lock hold
>   (`send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock` all token-aware). Routed via
>   `Disp::WaitPark(Vec<(key, core)>)` captured while the fiber heap is live (mirrors `Disp::Park`). The
>   1-key recv park stays the cheaper `ParkedEntry::Recv` case (alloc-free, byte-identical —
>   `vm_wait_single_arm_recv_park_unchanged_under_parallel`). Deadlock accounting: a wait-parked fiber is
>   `parked_n += 1` (ONE fiber, regardless of arm count) so the `is_deadlocked` predicate stays sound
>   (`vm_wait_lone_blocked_parallel_deadlocks`; a live sibling vetoes —
>   `vm_wait_sibling_send_vetoes_deadlock_parallel`). **`native_reentry > 0` (wait inside a native
>   callback):** can't snapshot-park → `demote_wait_block` blocks in place, polling all N arm queues
>   source-order on a bounded `DEMOTE_POLL_BACKOFF` (the N-arm analogue of `demote_recv_block`;
>   lower-throughput-but-sound **v1 limitation** — there are N channel condvars, no single one to block on).
>   All three engines byte-identical on `examples/wait_select.chz`; 150× + 4×80× stress loops clean (no
>   lost-wakeup). **Fixed in passing (a pre-existing two-engine parity bug exposed by the edge tests):**
>   the peephole optimizer did not relocate `Op::WaitPoll`'s `arm_targets`/`else_target` through its
>   fold/fuse index remap, so a multi-arm `wait` whose arm body fused a binop (`x + w`) jumped PAST the
>   bind prologue (VM 65 vs interp 66). Now `WaitPoll`'s targets are marked + relocated like `Jump`/
>   `MatchArm` (`relocates_waitpoll_arm_and_else_targets_past_a_fold`,
>   `vm_wait_arm_body_outer_local_in_binop_matches_interp`).

### Tier-D — complete (D0–D6c)

Designed in [`docs/reference/concurrency.md §10`](docs/reference/concurrency.md); the full per-phase TDD breakdown lives in
**[`docs/history/designs/concurrency-tier-d.md`](docs/history/designs/concurrency-tier-d.md)**. Landed, in one summary:

- **D0** — O(N²)→O(N·logN) cooperative ready-queue (per-nursery `ready` set + parked-index buckets).
- **D1** — lazy module snapshot: a shared read-only `Arc<ModuleSnapshot>` faulted into each worker heap
  on first access, killing the per-task module-graph rebuild.
- **D2a/D2b** — true **M:N work-stealing scheduler**: lightweight share-nothing fibers (own heap, carried
  in a swappable `FiberCtx`) multiplexed over the bounded pool, **parking on `recv` instead of pinning OS
  threads**; the joining thread runs an inline shell that alone guarantees completion (decision B).
- **D3** — BEAM-style **reduction-counting preemption** (`reds` budget, yield at exhaustion to the run
  queue's tail) so a CPU-bound fiber can't starve siblings.
- **D4** — Go-style per-worker local run queues + shared global overflow + random-victim work-stealing +
  periodic global check; runnable-gated park wake (a true `cv.wait` when `runnable==0`, bounded backoff +
  re-steal when `>0` — the mutex *is* the StoreLoad barrier, no Go-style fence needed).
- **D5** — **dirty/blocking pool**: a blocking off-heap-safe native (`read_file`/`write_file`, `fs.*`,
  `request`, `process`, `sleep_ms`) suspends the fiber and hands the call to a growable pool instead of
  pinning a core worker; an `inflight` fiber-state vetoes a false deadlock. A process-wide timer thread
  (later folded into the poll thread) parks sleepers on a deadline min-heap. *Path C* demotes the worker
  (one raw replacement OS thread, Go-`handoffp`-style) for a blocking `recv`/`sleep`/socket op reached
  *inside a native callback* (`native_reentry > 0`, host-stack loop frame, unsnapshotable).
- **D6a/b** — **netpoller** (`src/vm/poller.rs`, epoll/kqueue via `polling`): a would-block socket op
  becomes a cheap fiber-park. `std.net` (`Obj::Socket`/`Obj::Listener` over `Arc` cores) — non-blocking
  `connect`/`listen`/`accept`/`read`/`write`/`close`/`addr`; `connect` is true non-blocking via
  `socket2`. Drain-on-fault re-injects socket-parked fibers so a net server can share a nursery with a
  fallible sibling; one poll thread serves both socket readiness and sleeps.
- **D6c** — **per-socket read/accept/write timeout** (`--parallel`): `conn.read(n, timeout_ms)` /
  `sock.write(s, timeout_ms)` / `server.accept(timeout_ms)` return `Err("timeout")`; `0` polls once, a
  negative saturates. Reuses D6b's deadline-bounded poll, no new thread/heap/job (`poller::Parked` gains
  a `deadline`, a `fire_due_socket_timeouts` pass sets a per-fiber `poll_timed_out` marker). Checker
  gained optional trailing-arg arity. `examples/socket_timeout.chz`.

**Per-connection `spawn`** also landed — an **eager injectable nursery** (`--parallel` M:N, ≥2 cores): a
`spawn` in a *nested* `parallel:` runs concurrently with the rest of the body instead of queueing for the
join, so the canonical server shape (accept-loop `spawn`s a `handle(conn)` per connection) works. The
nested nursery is eager (`EnterNursery` builds the `MnSched` immediately + spawns one dedicated raw
drainer thread); a `spawn` injects a live fiber straight into it; a `body_open` flag holds termination
open and vetoes the deadlock predicate while the body may still inject. **v1 limits (documented):** needs
≥2 hw threads; bounded accept loops only (an unbounded `while true:` server never reaches the join —
graceful shutdown is future work); a handler talking back to the acceptor via a Channel is a cross-nursery
wakeup. `examples/echo_server_spawn.chz`.

**Cross-nursery flat scheduler — M:N (`--parallel`) DONE, cooperative DEFERRED.** The circular
outer-sibling cross-nursery deadlock (`examples/parallel_cross_nursery_circular.chz`: `inner()` spawns a
nested nursery while `main`'s outer `parallel:` still has an un-run sibling `O`; the inner owner used to
drain only its private queue and could never RUN `O` → `deadlock` fault) is **fixed under `--parallel`**:
- **One VM-global `MnSched`** with `SchedCore.scopes: Vec<JoinScope>` (replacing the scalar
  `{done,total,body_open}`) + a flat `slots` vec. Each nested nursery is a SCOPE enlisted into the SAME
  global run queue; `Fiber` carries a `scope_id`. The inline owner returns on a **scope-scoped stop**
  (`Take::Stop` when ITS scope's `done==total`, having drained the GLOBAL queue meanwhile — so it ran the
  cross-nursery sibling), while farmed helpers drain until global `terminate` (a `SENTINEL_SCOPE` owner id).
- A nested builder **early-enlists** the outer nursery's still-pending siblings (so the nested owner can
  run them — the cross-nursery wake) but **DEFERS** each enlisted scope's output flush to its OWN
  `JoinNursery` (`mn_scopes` records the scope; `mn_enlist_sched` holds the sched alive until the last
  enlisted scope joins). This preserves the **per-nursery-join flush order**, so three-engine parity for
  non-blocking nested spawns is byte-identical (`implicit_nursery_nested_functions` etc. unchanged).
  Outer scopes are enlisted **before** any helper worker is farmed, so a multi-task inner nursery can't
  trip the global deadlock predicate before the outer sibling is seeded (caught + regression-guarded by
  `examples/parallel_cross_nursery_fanout.chz` — a 2-task inner nursery, looped under a watchdog).
- The deadlock predicate + `finish`/`flag_deadlock`/`cancel_drain` went **global over scopes** (fault only
  when SOME scope is incomplete and nothing can progress anywhere); per-scope **cancel** Arcs (the shell's
  `self.cancel` re-pointed to the running fiber's scope cancel on each `run_one_fiber` swap-in;
  `cancel_drain(scope_id)` requeues only that scope's parked fibers) keep an inner fault from cancelling
  outer siblings (structured concurrency preserved). Genuine no-sender deadlocks still fault
  (`golden_parallel_deadlock_still_faults`, 30s watchdog).
- **Output order note:** because `O` (outer) and `I` (inner) live in DIFFERENT nurseries with different
  join points, the M:N flush order is `I` (inner join) then `O` (outer join) — i.e.
  `I got 1\nO got 1\ndone` — NOT the case-C single-nursery order (`O got 1\nI got 1`). Both complete; the
  ordering follows the parity-preserving per-nursery flush.
- **Eager nurseries keep their private sched (OPTION A), + child→parent wake routing (gaps.md B5):** the
  per-connection eager nursery keeps its OWN sched + dedicated drainer (single-scope fast path). But a
  `send`/`close` inside an eager body only scanned that private sched's park set, so a receiver parked in
  the PARENT nursery on a shared channel was never woken → a spurious `deadlock` (an uncontended nested
  `send`→outer `recv`). Fixed by `MnSched::parent_wake` (the eager sched points at the activating parent
  sched; `send_wake`/`close_wake` walk that chain child→parent, strictly upward, and requeue the parent's
  parked receiver onto its home sched). `is_deadlocked` unchanged — genuine no-sender quiesce still faults.
  Golden `parallel_cross_nursery_nested_send_to_outer_recv.chz` (serial==M:N). **Residual (documented):**
  parent→child (receiver parked INSIDE an eager body, sender in an ancestor) + sibling-eager→sibling-eager
  stay timing-divergent (complete or deadlock-fault cleanly).
- **Cooperative (`run --serial`) + `--interp`:** still serialize nested nursery levels → the same program
  **still faults `deadlock`** there. The cooperative-engine flatten is a **separate, later commit**.
  Workaround on `run`: siblings in ONE nursery (doc case C). Golden is M:N-only (no coop/interp leg),
  watchdog-wrapped — mirrors `golden_channel_block`.
- **Post-review hardening (the first cut was REJECTED by the adversarial panel — 3 blocking; now fixed):**
  - **Inline outer-body `send`/`close` routing (charges #1/#2):** the inline `parallel:` builder runs with
    `self.mn == None` (sched only in `mn_enlist_sched`), so its own `send`/`close` used to bypass the
    global park set and never wake an enlisted, parked sibling → false `deadlock`. `channel_send_wire` +
    the `close` arm now route through `self.mn.or(self.mn_enlist_sched)`. Guards:
    `..._inline_send.chz`, `..._inline_close.chz`.
  - **`awaiting_builder` deadlock veto:** an early-enlisted scope is marked `awaiting_builder` (the live
    builder body is its feeder); `is_deadlocked` vetoes only while EVERY incomplete scope is awaiting the
    builder (`all_incomplete_awaiting_builder`). A genuine NESTED deadlock keeps a non-awaiting scope
    incomplete → still faults (`parallel_cross_nursery_genuine_nested_deadlock_still_faults`).
  - **Late spawn after enlist (charge #3):** a `spawn:` issued after `early_enlist_outer` drained the
    nursery vec used to be silently dropped at the join. `join_nursery` now runs the refilled tasks on
    the HELD flat sched (`mn_enlist_sched`) as a fresh trailing scope — `register_scope` is append-only
    (slots stay contiguous) and un-latches a stale global `terminate` so the inline owner runs the late
    task instead of stopping on the prior-scopes-all-done flag (no clobber of the held sched, no `index
    out of bounds` panic, no drop); `drain_escaped_nursery` reports them on an escape. Guards:
    `..._late_spawn.chz`, `parallel_cross_nursery_late_spawn_into_middle_runs`,
    `parallel_cross_nursery_late_spawn_escape_reports_pending`.
  - **Atomic enlist (charge #4):** `early_enlist_outer` now validates (prepares workers from clones)
    BEFORE consuming the nursery / registering a scope, so a `prepare_worker` `Err` (checker-gated
    backstop) can't leave an unseeded scope (hang) or a half-state — it unwinds cleanly.
  - **2+ enlisting levels — limit LIFTED (independent/normal nesting now RUNS):** the old blanket gate in
    `early_enlist_outer` ("2+ enlisting levels … aren't supported") was TOO BROAD — it regressed ordinary
    multi-level nesting (independent nested `parallel:` blocks with sibling/late `spawn:`s) that has no
    shared channel and never parks. The gate is GONE. Any depth of nested `parallel:` now matches the
    cooperative engine under `--parallel`. Only the genuinely-CONTENDED case (2+ live receivers racing ONE
    channel across nested scopes) remains divergent — and it is NOT gated: concurrent-divergent BY DESIGN
    (delivery order may differ, or it deadlock-faults; suspendable concurrency is VM-only/divergent), it
    only must never PANIC and never HANG. Guards: `parallel_cross_nursery_independent_3level_runs_all`,
    `parallel_cross_nursery_late_spawn_into_middle_runs`, `parallel_cross_nursery_contended_never_panics`,
    golden `examples/parallel_cross_nursery_multilevel.chz`.
    A late `spawn:` into a middle nursery runs on the HELD flat sched as a fresh trailing scope via
    `register_scope_seeded` — register + seed atomically under one core lock (mirrors `inject`), closing a
    `runnable==0` TOCTOU window where a SENTINEL helper could have falsely deadlock-faulted a parked outer
    receiver. Guard: `parallel_cross_nursery_late_spawn_parked_matches_coop`.
  - **Out of scope (documented separate limits):** the inline-body *blocking* recv (case B — wake-side
    fix only) and eager (per-connection) nurseries' private sched.

**`Channel.close()` + closed-channel semantics + `try_send` + `for v in ch:`** landed (both engines) —
the headline consumer-side feature giving clean producer→consumer termination (was: a consumer looping
`recv` after the producer was done could only deadlock-fault):
- `for v in ch:` — blocking iteration, drains buffered + future values, ends cleanly once
  closed-and-drained (Go's `for v := range ch`).
- `ch.close()` — idempotent, no args, wakes every parked/demoted receiver.
- `send` after close → faults; `recv` on closed-and-empty → faults (drains buffered first).
- `ch.try_send(v) -> bool` — the safe partner of `send` (`false` = closed; channels are unbounded, so
  closed is `send`'s only failure mode). `try_recv` unchanged (`None` on closed).
- Comprehension-over-channel (`[v for v in ch]`) is **rejected by the checker** (it would diverge — VM
  drains, interp oracle can't).

**Pending-`spawn`-drop on early `parallel:` escape → cancel-and-report** landed (both engines): a
`parallel:` body escaping via `?`/`return`/`break`/`continue` before the join now **cancels** unstarted
tasks (the same end-state a started sibling reaches under cancellation) and emits one byte-identical
stdout report line. VM routes a `drain_escaped_nursery` through four reclaim sites (`do_return`, the
recover-catch fault path, a net-new `Op::ReclaimNursery` for break/continue, and the `do_try` recover-
scoped-`?` short-circuit, which drains the escaped body's defers to its floor *before* the report so
interp order is restored).

### Group B (B3.0–B3.6) — the OS-thread multicore epic, complete

Decomposed and documented in **[`docs/history/designs/concurrency-b3.md`](docs/history/designs/concurrency-b3.md)** (validated
shared-nothing architecture, decisions A–G, risk register). Summary of the landing:

- **B3.0–B3.2** — a `WireValue` airlock (`src/vm/wire.rs`) replaced `deep_clone`; `Channel`/`Shared`/
  `Executor` cores moved out of the GC heap into `Arc<…Core>` (`src/vm/core.rs`); `program` went
  `Rc<Program>` → `Arc<Program>`; `Vm::spawn_worker`/`run_task_isolated` build an isolated worker `Vm`
  with its own heap and cross args/captures/result by wire (cross-heap safety enforced via
  `ensure_crossable`). All single-thread, behavior byte-identical.
- **B3.3** — `str` crosses by value (`WireValue::Str`); the **G1 module-globals checker gate** (mutating
  a module global reachable from a `spawn` task is a type error, *"use Shared[T]"* — scope-aware,
  transitive over the free-fn call graph); worker module-graph reconstruction (read-only `home` snapshot
  + method tasks); then **real OS threads behind `--parallel`** (bounded pool, parent participates inline,
  per-core condvar `recv`, `Shared.update` lock).
- **B3.4** — cooperative **cancellation** + cross-thread `os.exit` (per-nursery `cancel` flag, first
  fault/exit trips it; `os.exit` wins over any sibling fault; cancel bypasses `recover:` but still runs
  `defer`s). Single-level only — nested-nursery cancel propagation is documented/deferred.
- **B3.5** — nursery-local **deadlock detection** under threads (barrier-confirm detector; later retired
  in favour of D2b's exact single-coordinator predicate).
- **B3.6** — `Executor` on the pool + the **A3b `submit`-capture sendability gate** (checker). A
  submitted closure crosses **by value on BOTH engines** (`WireValue::Closure` via `wire_callable`),
  isolating captures at submit + running the ref/generator airlock enforcement identically on serial and
  M:N. (Originally the cooperative engine crossed by handle to mirror the tree-walk `interp` oracle;
  that oracle was removed and the by-handle branch was pure serial-vs-M:N divergence — retired, see
  "Executor.submit coop==M:N by value".)

### M-C — implicit nurseries (shipped 2026-06-12)

Every function body and the module top level is an implicit nursery that joins at its `return`/end
(module top joins at program exit); a bare `spawn` is legal anywhere, dropping the explicit `parallel:`
requirement. `parallel:` is demoted to an explicit *inner* sub-nursery for earlier joins. Design:
[`docs/reference/concurrency.md §10`](docs/reference/concurrency.md). Concurrency is now feature-complete (no Tier-E).

- **Join-on-exit.** `return <value>`, fall-through end, and `?` early-return are all join points —
  spawned tasks run FIFO, *then* control leaves; `defer`s run after the join (tasks, then cleanup). A
  `return`/`?` that escapes an *inner* `parallel:` still cancels-and-reports that inner nursery while
  joining the function's implicit one. An uncaught body fault cancels-and-reports the implicit nursery
  (abnormal exit) — identical to an explicit `parallel:` escape.
- **Single join site + zero-overhead gate.** Compiler pre-scans a body for a bare `spawn`
  (`compiler::block_has_bare_spawn`, stops at `parallel:`/nested-fn/`spawn:`-block); if present it emits
  one opening `Op::EnterNursery` and sets `Proto::has_implicit_nursery`. The VM's `do_return` joins it
  (cancel-inner-then-join-implicit, before defers) for `return`/`?`/end. Bodies with no bare spawn emit
  byte-identical bytecode to pre-M-C — perf benches (no spawns) unchanged.
- **Implicit nursery sites.** Function bodies, the module top level, **`spawn:` blocks, and `defer:`
  blocks** each get their own implicit nursery (each runs in its own frame; a bare `spawn` inside binds
  to *that* body's nursery). Joins at the body's own `return`/end.
- **Three-engine parity.** Interp (`call`/`run_block_task`/`eval_top_level` push an implicit nursery +
  `leave_implicit_nursery` join/cancel), cooperative VM, and `--parallel` are byte-identical. Tests:
  `vm::tests::implicit_nursery_*` (3-engine, incl. `_try_preserves_error_value` +
  `_spawn_in_defer_block` review-panel regressions), `interp::tests::implicit_nursery_*`, golden
  `examples/implicit_nursery.chz`. Checker `spawn_at_function_scope_ok` / `spawn_in_plain_fn_ok` /
  `spawn_at_module_toplevel_ok` (the old `spawn_outside_parallel_rejected` flipped); dead
  `nursery_depth` checker field removed.
- **RESOLVED (2026-06-12) — uncaught-fault cancel-report parity:** an *uncaught* fault with un-run
  nursery tasks now prints the cancel-report on the VM's stdout too, matching the interp and the
  `--parallel` engine. Three coordinated fixes in `src/vm/mod.rs`: (1) `unwind_deferred` gained a
  `report_escaped: bool` param — on a genuine fault (passed `true` from the fault-unwind arm; `false`
  from the two B3.4-cancel paths) it now cancels-and-reports each discarded frame's escaped nurseries
  **before** that frame's `defer`s run, matching the interp order (`exec_parallel` /
  `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs defers); the old
  `_ => return Err(rte)` uncaught arm reported nothing. (2) `drain_escaped_nursery` now reports
  **per-nursery** (innermost-first), not one combined line — two stacked nurseries → two lines, not
  `2 pending` (also fixed a latent recover-caught combine divergence). (3) the MODULE top-level
  nursery is preserved (`nursery_len + 1` floor): an uncaught *top-level* fault stays silent on both
  engines (it joins only on clean program exit). Review-panel (SRE) caught a defer/report interleave
  divergence the first cut missed; cold pass verified the shared `unwind_deferred` interactions.
  Tests: `vm::tests::uncaught_fault_reports_implicit_nursery` / `_explicit_parallel` /
  `_each_nursery_separately` / `_reports_before_frame_defers` / `_interleaves_report_and_defer_per_frame`
  / `_uncaught_toplevel_fault_does_not_report_module_nursery`, plus `recover_caught_fault_reports_*`.
  Full suite green (1600), three-engine parity.

### Standing decisions & contracts (do not re-litigate)

> **DECISION — do NOT build interp B1/B2 (suspendable tree-walker). Deliberate non-goal.** The interpreter
> stays frozen at the sequential concurrency subset and serves as the differential-testing parity oracle
> for the non-blocking surface (its real value: catching VM / GC / compiler bugs). Suspendable execution
> would need stackful coroutines or a full CPS `eval` rewrite — large, risky, covering a slice the oracle
> does not need. **The VM is the sole concurrent engine.**

- **Parity contract (narrowed, intentional):** the engines agree on the **sequential subset** — all
  *non-blocking* `parallel:` / `spawn` / `Channel` / `Shared` / `Executor` programs (byte-identical,
  parity-tested). **Suspendable concurrency (blocking `recv`) is VM-only by design**: under `--interp` a
  blocking `recv` faults `deadlock` (pinned by an interp test vs the VM golden). This divergence is the
  stated contract, not a bug.
- **Known VM v1 limits (acceptable; not parity issues):** a blocking `recv` reached inside a native
  callback (list HOFs, `sort`, `compare`/`hash`/`str` hooks, `Shared.update`, executor drain, a `defer`red
  call) faults `deadlock` *unless* Path C demotion applies (`recv`/`sleep`/socket under `--parallel`); a
  fiber blocked in an outer nursery *is* woken (D0 cross-level wake-marking, common case works); the narrow
  circular case (its unblocker is an outer sibling the inner scheduler must run) is **RESOLVED under
  `--parallel`** by the M:N flat scheduler (see the cross-nursery section above) but **still faults
  `deadlock` on the cooperative `run`/`--interp`** engines (the cooperative flatten is a separate, later
  commit). Independent/normal multi-level nesting (no shared channel) RUNS under `--parallel` and matches
  coop (the old "2+ enlisting levels" gate is gone). Residual M:N limits: a genuinely-CONTENDED shared
  channel across nested nurseries (2+ live receivers racing ONE channel) is concurrent-divergent BY DESIGN
  (delivery order may differ, or it deadlock-faults — never panics/hangs); the inline outer-body's
  *blocking* recv (case B — wake-side fix only; put blocking work in a `spawn:`); and eager
  (per-connection) nurseries' private sched.
  Fix design + resolution in [`docs/history/designs/cross-nursery-flat-scheduler.md`](docs/history/designs/cross-nursery-flat-scheduler.md);
  correct cooperative pattern in `examples/parallel_cross_nursery_ok.chz`.
  Documented residuals: a narrow parked-sibling false-positive under multi-demote; the `Shared.update`
  same-box recv hazard; a saturated-pool queued-task counted live (no-false-positive choice).
- **Use `iter.map`/`iter.filter`/`iter.fold`/`iter.reduce` (chezzi source, `std/iter.chz`)** if a
  callback may block under `--parallel` — they run through VM frames so a blocking `recv` parks. The
  native `xs.map(f)` is the faster non-blocking path (and demotes via Path C if a `recv` blocks in it).

**Permanent non-goals:** interp B1/B2 (above); variadic args, bignum (`i64`-only — every overflow is a
recoverable fault; binary work → the `bytes` (immutable) + `bytearray` (mutable) *sequence* types, both **shipped** — no `byte`/`u8` scalar). **Level-3 dynamic
C-ABI FFI is NO LONGER a non-goal — v1 shipped** (`extern "lib":` scalar calls via dlopen+libffi,
**plus opaque C `void*` handles** via the `ptr` type — `Obj::Ptr`/`Value::Ptr`, `std.ffi.null()`/
`is_null`, untyped + manual-free, `examples/ffi_ptr.chz`; **plus the return-only `str` opt-ins
`owned_str`** (copy + libc `free`, no leak) **and `str?`** (`NULL` → `None`, `examples/ffi_str.chz`);
**plus bidirectional fixed-width integers `int8`..`uint64`** (bind C `int32_t`/`uint32_t`/…;
truncate-on-param / sign-or-zero-extend-on-return, **imported per-name from `std.ffi`** — Chezzi's
first type imports, `examples/ffi_int.chz`);
**plus flat-scalar structs by value** (a Chezzi `struct` of scalar fields ↔ a C struct passed/returned
by value, `examples/ffi_struct.chz`);
**plus `bool` ↔ C `_Bool`** (1 byte — params/returns/struct fields; int-returning predicates like
`isdigit` bind `-> int` + test `!= 0`);
**plus sync scalar callbacks (#4)** (a `fn(scalars) -> scalar` extern param → a libffi closure
trampoline C calls back synchronously, same-thread, scalars only; faults caught + re-raised; both
engines + `--parallel` parity; `src/native/cffi.rs` `CType::Callback` + `Host::invoke_callback`);
nested structs / `str` struct fields / **the rest of callbacks (#4 — stored/cross-thread + pointer-deref
builtins)** / **varargs (#5)** (with design notes + the callback feasibility ladder +
a varargs fixed-arity workaround in `docs/design/ffi-and-packaging.md §1b`),
a custom user-named deallocator, C-spelling int aliases (`c_int`), and the rich Rust
`Box<dyn Any>` userdata handle still deferred — see "Done" below; forward design for the Rust
userdata Value + the package registry is in
[`docs/design/ffi-and-packaging.md`](docs/design/ffi-and-packaging.md)). **`yield`/generators are likewise
no longer a non-goal — complete VM-only support shipped** (see below).

> **`yield`/generators — complete, VM-only (landed on `feat/yield-generators`).** No longer a
> non-goal: a `fn` declaring `-> Iterator[T]` may `yield`; the call returns a suspendable generator
> (a one-shot cooperative coroutine — its own private frame/stack swapped into the VM, resumed by an
> intrinsic `.next()` that the `for`-loop step drives). VM-only: the frozen interpreter rejects
> `yield` (it cannot suspend a native Rust call), so **two-engine parity is waived** for generators.
> `defer`/`spawn`/`parallel:`/`wait:` are checker-forbidden inside a generator. See
> `examples/generators_basic.chz`, the `vm_generator_*` tests, and the `generator_*` checker tests.
> The adapter-struct model over `Iterator[T]` (`examples/iter_adapters.chz`) stays the parity-clean,
> recommended way to write lazy sequences.

---
