#!/usr/bin/env python3
# TICKET-230 oversubscription driver: more runner threads than permits must not cost CPU.
# Usage: oversub_pair.py <bin> <n> <ref-prog> <threads>... -- <prog>...
# For each thread count, runs <ref-prog> (the same work with at most N runner threads) and every
# <prog> interleaved n times on ONE binary, and prints one row per <prog>:
# | prog | T | n | ref user s / wall s | user s / wall s | user ratio | wall ratio | ok or SLOW |
# `ok` when both medians are at most 1.15x the reference's. The bound does not use a base binary:
# before TICKET-230 these shapes were fast only because they ran more than N runners.
import os, statistics, subprocess, sys, time

BOUND = 1.15
binary, n, ref = os.path.abspath(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
sep = sys.argv.index("--")
threads, progs = sys.argv[4:sep], sys.argv[sep + 1:]


def run(prog, t):
    t0 = time.perf_counter()
    p = subprocess.Popen(
        [binary, "run", f"--threads={t}", prog],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    _, status, ru = os.wait4(p.pid, 0)
    if status != 0:
        print(f"  {prog} T={t} exit status {status}", flush=True)
    return ru.ru_utime + ru.ru_stime, time.perf_counter() - t0


print("loadavg", open("/proc/loadavg").read().strip(), flush=True)
for t in threads:
    res = {p: [] for p in [ref] + progs}
    for _ in range(n):
        for p in [ref] + progs:
            res[p].append(run(p, t))
    ru = statistics.median(u for u, _ in res[ref])
    rw = statistics.median(w for _, w in res[ref])
    for p in progs:
        u = statistics.median(u for u, _ in res[p])
        w = statistics.median(w for _, w in res[p])
        verdict = "ok" if u <= BOUND * ru and w <= BOUND * rw else "SLOW"
        print(
            f"| {os.path.basename(p)} | {t} | {n} | {ru:.2f} / {rw:.2f} | {u:.2f} / {w:.2f}"
            f" | {u / ru:.2f}x | {w / rw:.2f}x | {verdict} |",
            flush=True,
        )
