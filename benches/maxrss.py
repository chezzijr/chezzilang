#!/usr/bin/env python3
# Peak-RSS of a child via os.wait4 (per-process ru_maxrss, KB on Linux). Median of N runs.
import os, sys, subprocess, statistics
def run_once(cmd):
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r); os.dup2(w, 1)  # stdout -> pipe
        os.execvp(cmd[0], cmd)
        os._exit(127)
    os.close(w)
    out = os.read(r, 65536); os.close(r)
    _, status, ru = os.wait4(pid, 0)
    return ru.ru_maxrss / 1024.0, out.decode(errors="replace").strip()  # MB, stdout
def main():
    n = int(os.environ.get("RUNS", "3"))
    cmd = sys.argv[1:]
    rss, out = [], None
    for _ in range(n):
        m, o = run_once(cmd); rss.append(m); out = o
    print(f"{statistics.median(rss):8.1f}  {' '.join(cmd)}  -> {out[:40]}")
if __name__ == "__main__": main()
