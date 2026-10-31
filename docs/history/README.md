# History

Records of finished work. The text in these files is kept as it was written: a file split from an
older document starts with one line that names its source, and the rest is unchanged. Links and
`file:line` citations inside a record can point at paths that moved later. The current state is in
[`PROGRESS.md`](../../PROGRESS.md) and the live docs.

Entries dated before 2026-08-16 that measure `--serial`, compare serial against M:N, or call `--serial`
the parity oracle describe the engine of that time. Since 2026-08-16 there is one engine.

## progress/

The dated log that `PROGRESS.md` held until 2026-10-16. Newest entries come first in each file.

| file | contents |
|---|---|
| [2026-11.md](progress/2026-11.md) | November 2026: refactor fact 2 |
| [2026-10.md](progress/2026-10.md) | October 2026: refactor phases, TICKET-188..262 |
| [2026-09.md](progress/2026-09.md) | September 2026 |
| [2026-08.md](progress/2026-08.md) | August 2026 (ends with one 2026-08-18 GC entry that sat after the July block) |
| [2026-07.md](progress/2026-07.md) | July 2026 and older dated entries |
| [bug-hunt-w11.md](progress/bug-hunt-w11.md) | the wave 11 summary section |
| [current-focus-archive.md](progress/current-focus-archive.md) | the old "Current focus" section: ticket blocks, JIT entry rule of 2026-09-23, the paused type-conversion track, the perf batches |
| [concurrency-complete.md](progress/concurrency-complete.md) | concurrency feature-complete record (2026-06-12): Tier-D, B3, M-C, standing decisions |
| [done.md](progress/done.md) | the old "Done" list, newest to oldest |
| [notes.md](progress/notes.md) | the old file header, stdlib and syntax batches of 2026-06, the old roadmap, known friction |

Write new dated entries in `progress/YYYY-MM.md` for the current month.

## bug-hunts/

Closed ledger rows, bug-hunt session logs and root-cause records. Files `closed-rows.md` to `w14.md` are
the old `gaps-archive.md`, split on its `##` headings. A `docs/gaps.md:NNNN` citation in an
old ticket pointed into that archive; search for the row ID to find it now.

| file | contents |
|---|---|
| [closed-rows.md](bug-hunts/closed-rows.md) | the archive header and the table of every struck row |
| [w01-w05.md](bug-hunts/w01-w05.md) | session logs 2026-07-16..24: waves 2 to 4, stdlib gap-fill, small fixes |
| [2026-07-14-audit.md](bug-hunts/2026-07-14-audit.md) | the 2026-07-14 audit: bugs B1..B5, root causes R1..R4 |
| [categories.md](bug-hunts/categories.md) | category sections: language and concurrency, stdlib, language features, tooling, CLI |
| [audited-residuals.md](bug-hunts/audited-residuals.md) | residuals N1..N10 of the Tier-0 gate, wave 5 residuals, dependency versions |
| [2026-07-checker-and-sendability.md](bug-hunts/2026-07-checker-and-sendability.md) | checker item and the sendability backlogs of 2026-07-21 |
| [w06.md](bug-hunts/w06.md) | wave 6: W6-1..W6-19 |
| [w07.md](bug-hunts/w07.md) | wave 7: W7-1..W7-38, with the 2026-08-06 protocol-embeds log |
| [2026-08-10-external-review.md](bug-hunts/2026-08-10-external-review.md) | external review batch 1: W7-39..W7-55 |
| [2026-08-16-serial-removal.md](bug-hunts/2026-08-16-serial-removal.md) | what the serial-engine removal found |
| [w08.md](bug-hunts/w08.md) | wave 8: the external dogfood passes of 2026-08-17/18, W8-43 |
| [w09-w10.md](bug-hunts/w09-w10.md) | waves 9 and 10 |
| [w11.md](bug-hunts/w11.md) to [w14.md](bug-hunts/w14.md) | waves 11 to 14, one file each |
| [w16.md](bug-hunts/w16.md) to [w23.md](bug-hunts/w23.md) | root causes of waves 16 to 23 (JIT sweeps #1 to #8) |
| [bug-discovery-records.md](bug-hunts/bug-discovery-records.md) | records moved out of `docs/contributing/bug-discovery.md` |
| [lessons-incidents.md](bug-hunts/lessons-incidents.md) | the untrimmed `lessons.md`, with each incident in full |
| [special-cases-audit.md](bug-hunts/special-cases-audit.md) | the untrimmed special-cases audit of 2026-10-11 |
| [backlog-closed.md](bug-hunts/backlog-closed.md) | closed items of the old root `gaps.md` design backlog |

## designs/

Design documents of shipped work, and the full text of design docs before they were trimmed.

| file | contents |
|---|---|
| [concurrency-b3.md](designs/concurrency-b3.md) | B3: the OS-thread multicore plan and landing notes |
| [concurrency-tier-d.md](designs/concurrency-tier-d.md) | Tier-D: the M:N scheduler and async I/O |
| [cross-nursery-flat-scheduler.md](designs/cross-nursery-flat-scheduler.md) | cross-nursery wakeups: the flat-scheduler fix |
| [concurrency-roadmap.md](designs/concurrency-roadmap.md) | the old concurrency roadmap and deferred backlog |
| [future-shipped.md](designs/future-shipped.md), [future-shipped-2.md](designs/future-shipped-2.md) | shipped sections of the old `docs/future.md` |
| `*-full.md` | the untrimmed text of a design doc that `docs/design/` now holds in short form |

## benchmarks/

Dated benchmark records, one file per month (`2026-06.md` to `2026-11.md`). Current figures are in
[`docs/contributing/benchmarks.md`](../contributing/benchmarks.md).
