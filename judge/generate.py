#!/usr/bin/env python3
"""Generate in-domain test cases for the judge harness — no CSES download needed.

For every problem with both a `gen.py` (emits one random input within the problem's stated range/
domain, seeded by argv[1]) and a `reference.py` (an INDEPENDENT Python oracle: stdin -> correct
stdout), this writes `--count` cases into judge/data/<slug>/ (gitignored). Running judge/run.chz then
compares the Chezzi solution against the Python oracle on those inputs — a self-contained
Chezzi-vs-Python differential test. The Python oracle must stay independent of Chezzi (a second Chezzi
impl would share Chezzi's bugs and prove nothing).

Usage:
    python3 judge/generate.py                 # all problems, 20 cases each
    python3 judge/generate.py playlist        # one problem
    python3 judge/generate.py --count 200     # more cases
    python3 judge/generate.py --seed 5000     # different seed base (reproducible)
"""
import argparse
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PROB = os.path.join(HERE, "problems")
DATA = os.path.join(HERE, "data")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("slug", nargs="?", help="one problem slug (default: all)")
    ap.add_argument("--count", type=int, default=20, help="cases per problem")
    ap.add_argument("--seed", type=int, default=1000, help="base seed (case k uses seed+k)")
    args = ap.parse_args()

    slugs = [args.slug] if args.slug else sorted(os.listdir(PROB))
    total = 0
    for slug in slugs:
        pdir = os.path.join(PROB, slug)
        gen, ref = os.path.join(pdir, "gen.py"), os.path.join(pdir, "reference.py")
        if not (os.path.isfile(gen) and os.path.isfile(ref)):
            if args.slug:
                print(f"{slug}: needs both gen.py and reference.py — skipped")
            continue

        out = os.path.join(DATA, slug)
        if os.path.isdir(out):
            shutil.rmtree(out)   # clear stale cases first (no orphans across re-generates)
        os.makedirs(out, exist_ok=True)

        def oracle(inp, label):
            res = subprocess.run(
                [sys.executable, ref], input=inp, capture_output=True, text=True,
            )
            if res.returncode != 0:
                sys.exit(f"{slug}: reference.py failed on {label}:\n{res.stderr}")
            return res.stdout

        for k in range(1, args.count + 1):
            inp = subprocess.run(
                [sys.executable, gen, str(args.seed + k)],
                capture_output=True, text=True,
            ).stdout
            with open(os.path.join(out, f"g{k}.in"), "w") as f:
                f.write(inp)
            with open(os.path.join(out, f"g{k}.out"), "w") as f:
                f.write(oracle(inp, f"random case {k}"))

        # Deterministic boundary cases (optional edges.py: no arg -> count, k -> the k-th edge input).
        # These pin the corners random fuzzing rarely hits: min/max sizes, all-equal, value extremes,
        # exact multiples, empty/full grids. Same oracle, written as e{k}.in/.out alongside the g* cases.
        edges = os.path.join(pdir, "edges.py")
        n_edge = 0
        if os.path.isfile(edges):
            n_edge = int(subprocess.run(
                [sys.executable, edges], capture_output=True, text=True,
            ).stdout.strip())
            for k in range(n_edge):
                inp = subprocess.run(
                    [sys.executable, edges, str(k)],
                    capture_output=True, text=True,
                ).stdout
                with open(os.path.join(out, f"e{k}.in"), "w") as f:
                    f.write(inp)
                with open(os.path.join(out, f"e{k}.out"), "w") as f:
                    f.write(oracle(inp, f"edge case {k}"))

        total += args.count + n_edge
        edge_note = f" + {n_edge} edge" if n_edge else ""
        print(f"{slug}: {args.count} random{edge_note} case(s) -> {os.path.relpath(out, os.getcwd())}/")

    print(f"done: {total} case(s). Run: ./target/release/chezzi run judge/run.chz")


if __name__ == "__main__":
    main()
