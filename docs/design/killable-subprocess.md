## 6h. M25 — killable subprocesses (`std.process` + cancellation) — **SPEC, not yet implemented**

**Problem.** Cancellation in Chezzi is *cooperative* (§6e): a `Token` signals, and the callee polls.
That works for Chezzi code, and it does not work at all for a thread parked inside a blocking native.
Measured on the release binary, a nursery task running `process.cmd("sleep 5")`:

| halt | what happens |
|---|---|
| sibling task faults (trips the scope cancel) | runs the full 5013 ms |
| `os.exit(3)` from another task | runs the full 5015 ms |
| `chezzi test --timeout=500` | runs the full 5008 ms |

Every `Kind::Blocking` native is offloaded to the dirty pool *precisely so it never pins a core
worker*; the price is that a thread inside the libc call has no cancellation checkpoint until it
returns (`docs/reference/stdlib.md` → *Blocking calls cannot be interrupted*). Nothing in the VM can fix that
from the outside — the fix has to be to **end the thing being waited on**.

For a subprocess we own the PID, so we can. This is exactly Go's answer: `exec.CommandContext` kills
the child when its context is done, and Go 1.20 added `Cmd.Cancel` / `Cmd.WaitDelay` to choose the
signal and a grace period.

### Scope — one module, deliberately

| module | killable? | decision |
|---|---|---|
| **`std.process`** | ✅ we own the child PID | **in scope — this milestone** |
| `std.request` | ❌ a signal is the wrong tool | out of scope; wants a client-side request timeout, same `Token`, separate work |
| `std.fs`, `std.io` file seams | ❌ not signal-interruptible | **out of scope permanently — Go does not cancel these either** (`os.ReadFile` ignores `context`), so today's behaviour already matches the ancestor |

Extending this to `fs`/`io` would be drift *away* from Go, not toward it. Say so at review time when
somebody asks why the milestone stops at one module.

### Three implementation traps, all measured in this tree

**1. Kill the process GROUP, never the PID.** `cmd`/`run`/`run_bytes` shell out through `sh -c`
(`src/native/process.rs:51`), so the real workload is frequently a *grandchild*. Measured:

```
sh -c '<victim> | cat'        sh=1959641   victim=1959643
kill -TERM 1959641
→ victim 1959643 SURVIVED
```

And the survivor is worse than an orphan: it still holds the write end of the stdout pipe, and the
parent is blocked reading that pipe — so killing only the shell **does not even unblock us**. The fix
is `std::os::unix::process::CommandExt::process_group(0)` at spawn plus `kill(-pgid)`, which is
correct whether or not the shell `exec`-optimizes into a single process.

**2. `.output()` has to go.** It consumes the child and only returns on exit, so there is no handle to
kill. `spawn_shell` (`src/native/process.rs:49`) must become `.spawn()` + a retained `Child`, with the
pipe draining moved onto that handle. This is a refactor of the module's core, not a new parameter —
budget it as such.

**3. `TERM` → grace → `KILL`, not bare `KILL`.** Go's default is `Kill`, but `WaitDelay` exists because
that is too blunt; since we shell out to real tools, give them a chance to flush and clean up. Default
grace on the order of a few hundred ms, overridable.

### The one deliberate divergence from Go: ambient by default

Go requires an explicit `ctx` on every call because Go has **no structured concurrency** — there is no
ambient scope to inherit. Chezzi has one: **the nursery IS the context.**

So a `std.process` call is cancelled by, in order of precedence:

1. an **explicit** `Token` passed to the call (Go's model, for the cases that want a narrower scope);
2. otherwise the **ambient scope cancel** of the task that made the call.

Ambient-by-default is the whole point: a sibling fault, `os.exit`, and `chezzi test --timeout` already
trip the scope cancel, and today none of them reach `sh`. Explicit-only would leave the `--timeout`
case — the one that motivated this milestone — still broken.

### Semantics to pin before writing code

- **A killed child surfaces as the CANCELLATION, not as a command failure.** If it returns
  `!"exited with signal 15"` a `recover:` swallows it and the task keeps running, which defeats the
  feature. It must carry the cancel/exit marker the surrounding halt already uses, so the existing
  precedence (`Exit` > hard fault > fault > deadlock) applies unchanged.
- **`defer` interaction.** A `defer:` body that itself shells out must not be truncated mid-cleanup —
  the same rule `W7-57` settled for the CPU rungs (`in_defer()` suppresses the halt at the shared
  funnel). Decide explicitly whether a *cleanup* subprocess is killable; the default should be no.
- **Idempotence + reaping.** Kill must be safe to call twice and after natural exit, and must always
  `wait()` the group so no zombie survives — including on the `os.exit` path, where the run is ending
  anyway.
- **Non-unix.** `process_group`/`kill(-pgid)` are unix-only. Decide the Windows story (job objects) or
  state the platform limit in `docs/reference/stdlib.md`; do not leave it implied.

### Phases

| phase | deliverable | runnable proof |
|---|---|---|
| **M25a** | `spawn_shell` → `spawn()` + retained `Child` + `process_group(0)`; pipe draining on the handle | every existing `std.process` test unchanged, no behaviour change yet |
| **M25b** | group kill on the **ambient** scope cancel, TERM→grace→KILL | the three rows in the table above abort in ~ms instead of running to completion; `chezzi test --timeout` reports a real abort, not the `W7-57` post-hoc `TIMED-OUT` |
| **M25c** | explicit `Token` parameter, precedence over ambient | a `cancel.timeout(200)` threaded into a `cmd` kills it; a derived child token cascades (§6e) |
| **M25d** | docs + the `std.request` follow-on decision | `docs/reference/stdlib.md`'s *Blocking calls cannot be interrupted* note narrows to `fs`/`io`/`request`; `std.cancel` §6e gains the effective-cancellation case |

### Verification bar

Same bar the `W7-47`→`W7-57` chain was held to, because this is the same subsystem:

- **The ancestor is the reference.** Write the Go program (`exec.CommandContext` + `signal.NotifyContext`)
  and cite its measured output for every semantic in question — not a description of it.
- **Grandchild proof, not PID proof.** Every kill test must use a *pipeline* (`… | cat`) so a
  PID-only implementation fails it. A test that only spawns a simple command will pass on a broken fix.
- **No zombies.** Assert the group is reaped, looped ≥30×.
- **False-kill fences.** A subprocess that finishes normally under an *uncancelled* token, a nursery
  that completes, and a `defer`-body subprocess must all be untouched — looped ≥30×, since this
  subsystem's bugs have historically been 2-in-30 rather than deterministic (`docs/contributing/gaps.md` `W7-12`).
- **Adversarial review** before landing, per the four fixes in this chain: it caught 12 defects a fully
  green 4000-test suite did not, including two hangs and a half-run `defer`.

### What this does NOT close

`std.fs` / `std.io` file seams stay uninterruptible, by the ancestor argument above. `std.request`
stays uninterruptible until its own follow-on. Both remain documented in `docs/reference/stdlib.md`; neither is
a residual of this milestone.
