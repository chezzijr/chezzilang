#!/usr/bin/env python3
# TICKET-211 base-vs-fixed driver: interleaved runs, wall ms by perf_counter, max RSS by wait4.
# Usage: ab_pair.py <base-bin> <fixed-bin> <n> <prog>:<threads> ...   (threads 0 = default count)
# AB_CPUS (optional) pins every run with `taskset -c $AB_CPUS`.
# Row: | prog | threads | base median (spread) / max RSS MiB | fixed ... | fixed/base | ok or SLOW |
# `ok` when the fixed median is at most the base median plus the base spread (max - min).
import os, sys, time, statistics, subprocess

BIN = {"base": os.path.abspath(sys.argv[1]), "fixed": os.path.abspath(sys.argv[2])}
n = int(sys.argv[3])
cells = [c.rsplit(":", 1) for c in sys.argv[4:]]
pin = os.environ.get("AB_CPUS")
print("loadavg", open("/proc/loadavg").read().strip(), flush=True)
if pin:
    print("AB_CPUS", pin, flush=True)
for prog, t in cells:
    res = {"base": [], "fixed": []}
    rss = {"base": 0, "fixed": 0}
    for _ in range(n):
        for side in ("base", "fixed"):
            args = ["taskset", "-c", pin] if pin else []
            args += [BIN[side], "run"]
            if t != "0":
                args.append(f"--threads={t}")
            args.append(prog)
            t0 = time.perf_counter()
            p = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            _, status, ru = os.wait4(p.pid, 0)
            ms = (time.perf_counter() - t0) * 1000
            if status != 0:
                print(f"  {side} {prog} T={t} exit status {status}", flush=True)
            res[side].append(ms)
            rss[side] = max(rss[side], ru.ru_maxrss // 1024)
    b, f = res["base"], res["fixed"]
    bm, fm, bs = statistics.median(b), statistics.median(f), max(b) - min(b)
    verdict = "ok" if fm <= bm + bs else "SLOW"
    print(
        f"| {os.path.basename(prog)} | {t} | {bm:.0f} ({bs:.0f}) / {rss['base']}"
        f" | {fm:.0f} ({max(f) - min(f):.0f}) / {rss['fixed']}"
        f" | {fm / bm:.2f}x | {verdict} |",
        flush=True,
    )
