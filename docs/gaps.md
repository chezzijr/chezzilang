# Chezzi — gap ledger

Open gaps only. One row per open item, each **re-verified on the release binary** on the date in the
Verified column. Everything closed — 159 struck rows and all 30 bug-hunt session logs, W1 through
W14 — lives in **[`docs/gaps-archive.md`](gaps-archive.md)**, which is a byte-for-byte carry-over of
this file's previous content, so historical `docs/gaps.md:NNNN` citations in closed tickets and in
`PROGRESS.md` resolve there unchanged.

Rules that govern this file (unchanged):

- A row is closed by striking it in the archive, never by deleting it; a new finding gets a new row,
  not a resurrected old one.
- A fix is judged against the **owning ancestor** — Go for concurrency/interfaces, CPython for
  scripting feel, Rust for enums/errors — **run**, not recalled. There is no second engine to diff
  against.
- Read the wave's session log in the archive before working any row; several share one fix.
- Read a closed row's **prescription** before re-implementing it. Two of them (W8-2, W8-7) were
  measured wrong after filing.

**Where the waves are, in the archive:** wave 14 `:13307` · wave 13 `:12849` · wave 12 `:12616` ·
wave 11 `:12485` · wave 10 `:380` · wave 9 `:409` · dogfood wave 2 `:519` · dogfood wave 1 `:685` ·
serial-engine removal `:1068` · wave 7 `:1459` / `:5499` / `:5969` · wave 6 `:1503` ·
waves 2–5 `:3057`–`:3286` · the 2026-07-14 four-axis audit `:3568`.

---

## TABLE — **8 open rows** (all re-verified 2026-09-20)

| row | P | domain | what | verified 2026-09-20 | archive |
|---|---|---|---|---|---|
| **W8-19** | P2 | affordances | Bundle. Remaining: multi-statement closures (ranked first, own milestone), tuples not `Hashable`, `path.join` rejects `Path`+`str` / no `path.abs`/`path.rel`, `Listener` not selectable in `wait:`, statement-only `recover:`. Global helpers and the `Option` half are DECLINED, do not re-file (the `Result` half closed under TICKET-039). | **OPEN** | `:185` |
| **W11-13** | P3 | airlock | The isolation warning gates on the READ shape. **Deliberately NOT ticketed** — an under-warn is the acceptable direction; re-open only with a measured runtime-derived table, one program per shape. | **OPEN** | `:12510` |
| **W11-14** | P3 | cancel | `Vm::guarded_checkpoint` (`src/vm/exec.rs:385`) has the owner hole TICKET-096 fixed at the other two checkpoints. Condition to re-open: the checkpoint runs per ELEMENT and `MnSched::scope_fault` takes the sched lock, so a rung there needs its own `benches/run.chz` measurement. | **OPEN** | `:12511` |
| **W11-15** | P3 | airlock | The three `RwShared` stores (`Op::NewRwShared`, `RwShared.set`, `RwShared.write`) still split a DAG alias into two copies. Re-opens with a rebuild path sharing one map across the piecewise drains. Pinned by `airlock_rwshared_store_dag_alias_is_a_known_residual`. | **OPEN** | `:12512` |
| **W12-5** | P1 | airlock | Five of six spawn-crossing shapes closed (TICKET-111); the sixth, a **sent closure** (G6), is untouched by design. Pinned by `airlock_closure_over_a_captured_alias_pushed_by_the_receiver_is_a_known_residual`. | **OPEN** | `:12633` |
| **W12-12** | P2 | tooling | Checker + compiler are exponential (~1.8x/level) in nested-`fn`-DECLARATION depth. **Guarded, not fixed** (TICKET-109): `desugar` rejects past `MAX_FN_NESTING` = 16. A real fix must make the nested walk idempotent, which needs the rollback to snapshot four pieces of state; then lift the limit. | **OPEN** | `:12640` |
| **W13-27** | P2 | perf | Introduced by TICKET-131's W13-6 fix. A nursery inside a spawned task costs ~1.8x while its ENCLOSING nursery's body is still open. Identified recovery path, not implemented: farm pool helpers for an outer sched while its body is blocked, not only after `close_body`. **Do not** recover it by restoring the private nested sched at T>=2 — that is W13-6. | **OPEN — not ticketed** | `:12894`, repro `:13233` |
| **W13-28** | P2 | perf/test | DEBUG binary only. A rendezvous ping-pong whose pair is spawned from INSIDE a spawned task runs bimodally at `CHEZZI_THREADS=8`. The broken pin it caused was already replaced 2026-09-18; what stays open is the bimodality itself. | **not re-verified** — needs a debug test binary and a 10-run sample | `:12895` |

### W8-17 — CLOSED (all five sub-items; the row is out of the table)

Every cosmetic this row listed is closed. (e) and (f) were fixed by later tickets that did not come
back and strike them here; (c), (d) and (g) closed under TICKET-158 on 2026-09-21. Full history:
`docs/gaps-archive.md:151`.

| sub-item | 2026-09-21 | |
|---|---|---|
| (c) one `match`-arm variant typo prints three errors at one position | **CLOSED** | TICKET-158: `E.Alpah` prints ONE error, `enum 'E' has no variant 'Alpah'` with `did you mean 'Alpha'?`, and no derived `non-exhaustive match`. A nested typo and the `Set`/`set` Hashable duplicate print once too; a non-exhaustive match whose ARM BODY or GUARD holds an unrelated error still reports both |
| (d) `unexpected an indented block in expression` | **CLOSED** | TICKET-158: reachable via a stray indent (`x := 1` then an indented `y := 2`); now `unexpected indented block in expression`, CPython's `unexpected indent` twin |
| (e) an unknown type renders as `?` | **CLOSED** | TICKET-145 rewrote the message: `'?' used in a function whose return type is not declared; declare it to return Result or Option (e.g. `-> int?`)` |
| (f) a `match`-pattern arm carets the scrutinee | **CLOSED** | TICKET-149 (W14-35b) gave arms their own span; the typo above now carets line 7, not the `match e:` line |
| (g) a literal's `end_col` is one char | **CLOSED** | TICKET-158: `x: int = "hello there"` → `"col":10,"end_col":23`; `word_end_col` now measures a string or number literal, so the LSP squiggle and the plain-text caret span it too |

### Repros, all run 2026-09-20 on `target/release/chezzi` at `50280d48`

    # W11-13 — warns on `print("{s}")`, silent on `print("{s.v}")`
    struct S:
        v: int
    fn main():
        s := S(1)
        parallel:
            spawn: s.v = 2
        print("{s.v}")        # ok: no type errors   <- the under-warn
    main()

    # W11-14 — the child's fault never cuts the owner's straight-line callback short
    parallel:
        spawn: panic("child boom")
        ys := xs.map(fn(x) -> int: x * 2 + 1)   # 3 000 001 elements
        print("owner finished map after {…}")   # prints, 0.41 s, THEN the fault surfaces

    # W12-12 — depth 16 checks in 0.56 s; depth 17 is refused
    resolve error (n17.chz:17:68): fn 'f16' is nested 17 deep; fn declarations nest at most 16 deep

    # W13-27 — fan_open vs fan_closed, release, 5 runs each (min/median, ms)
    T=8       fan_open  338/377    fan_closed  185/198
    default   fan_open  342/351    fan_closed  185/188

---

## Deferred tickets — 6 open, in `.project/tickets-deferred/`

Filed, triaged, parked. All six re-verified 2026-09-20 on the release binary.

| ticket | what | measured |
|---|---|---|
| **082** | a tuple is not a `Map` key and a `List[tuple]` has no ordering | `{(1,2): "a"}` → `map key type must implement Hashable … found (int, int)` |
| **083** | a runtime-computed field width cannot be formatted; both CPython spellings are rejected | `"{s:<{w}}"` → `format spec: unknown type char '{'`; `s.pad_right(…)` → `has no method 'pad_right'` |
| **084** | `Map(m)` fails where `List(xs)` and `Set(s)` succeed, and no method spelling exists | `List(xs)` → `[1, 2]`, `Set(xs)` → `{1, 2}`, `Map(m)` → `Map() expects an iterable of (key, value) 2-tuples, found element str` |
| **086** | no `findall`-shaped API returning `List[str]`, so the obvious tokenizer is several times CPython while `regex.split` is the workaround | `std/regex.chz` still exposes only `find_all(pat, s) -> Result[List[Match]]`. 2.4 MB / 400 000 tokens: `find_all` + a `.text` loop **740 ms**, `regex.split` **136 ms**, CPython `re.findall` **102 ms** |
| **087** | `std.time` is whole epoch seconds or a process-relative monotonic float; `std.datetime` is second-resolution throughout, so `12:34:56.789` is unreachable | `time.now()` → `1789915422` |
| **090** | Go's block-scoped `:=` was adopted; Go's `declared and not used`, which is what makes that model survivable, was not | the `total := total + x` typo inside a `for` prints `0`, rc=0, `--errors=json` → `[]` |

`.project/tickets-deferred/` also holds three `*.merged-into-*` stubs (078, 080, 088) — not work items.

---

## Pipeline

`.project/tickets/`: 140 done, 3 rejected (031, 127, 133), **0 in flight**. The wave-14 redesigns all
landed: **D1** a deadlock is fatal (TICKET-135), **D2** a received closure reads the running task's
module globals (TICKET-137), **D3** an int never widens into a float slot (TICKET-138).
