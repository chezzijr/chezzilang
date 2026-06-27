#!/usr/bin/env python3
"""Install full CSES test data for the DSA judge harness into judge/data/<slug>/.

CSES test cases are the authors' intellectual property, so they are NOT committed (judge/data/ is
gitignored). Download a problem's test ZIP yourself from its CSES page — open the task, go to the
"Tests" tab (visible once you've solved it / are logged in), and "Download" the zip — then feed it
here. Only the public *sample* cases (from the statement) live committed under
judge/problems/<slug>/samples/.

Usage:
    python3 judge/fetch_data.py <slug> <tests.zip | tests_dir>

The script extracts the archive (or scans the directory), pairs input/output files by their numeric
stem, and writes them as judge/data/<slug>/N.in and N.out — the layout judge/run.chz expects. It
auto-detects the common conventions:
    *.in  / *.out                (already normalized)
    N     / N.out | N.a | N.ans  (CSES-style: bare-number input + suffixed answer)
    *.in  / *.a   | *.ans | *.exp
Anything it cannot pair is reported and skipped (nothing is guessed silently).
"""

import os
import sys
import shutil
import tempfile
import zipfile

IN_EXTS = {".in", ".txt", ""}
OUT_EXTS = {".out", ".a", ".ans", ".exp", ".expected"}


def gather_files(src):
    """Return a flat list of (name, abspath) for every file in a dir or extracted zip."""
    files = []
    for root, _, names in os.walk(src):
        for n in names:
            files.append((n, os.path.join(root, n)))
    return files


def classify(name):
    """(stem, kind) where kind is 'in' | 'out' | None; stem is the numeric test id."""
    base = os.path.basename(name)
    stem, ext = os.path.splitext(base)
    ext = ext.lower()
    if ext in OUT_EXTS and stem.isdigit():
        return stem, "out"
    if ext in IN_EXTS and stem.isdigit():
        return stem, "in"
    # bare numeric file with no extension (CSES input): splitext leaves ext == ""
    if base.isdigit():
        return base, "in"
    return None, None


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)

    slug, src = sys.argv[1], sys.argv[2]
    here = os.path.dirname(os.path.abspath(__file__))
    out_dir = os.path.join(here, "data", slug)

    if not os.path.exists(src):
        sys.exit(f"no such file or directory: {src}")

    tmp = None
    try:
        if zipfile.is_zipfile(src):
            tmp = tempfile.mkdtemp(prefix="cses_")
            with zipfile.ZipFile(src) as z:
                z.extractall(tmp)
            scan = tmp
        elif os.path.isdir(src):
            scan = src
        else:
            sys.exit(f"not a zip or directory: {src}")

        ins, outs, collisions = {}, {}, set()
        for name, path in gather_files(scan):
            stem, kind = classify(name)
            if kind == "in":
                if stem in ins:
                    collisions.add(stem)
                ins[stem] = path
            elif kind == "out":
                if stem in outs:
                    collisions.add(stem)
                outs[stem] = path

        paired = sorted(set(ins) & set(outs), key=lambda s: (len(s), s))
        if not paired:
            sys.exit("no input/output pairs detected — check the archive layout (see --help).")

        # Clear any prior fetch first, so re-fetching a smaller suite over a larger one cannot leave
        # orphaned N.in/N.out behind for the harness to judge against (stale -> phantom regressions).
        if os.path.isdir(out_dir):
            shutil.rmtree(out_dir)
        os.makedirs(out_dir, exist_ok=True)
        for stem in paired:
            shutil.copyfile(ins[stem], os.path.join(out_dir, f"{stem}.in"))
            shutil.copyfile(outs[stem], os.path.join(out_dir, f"{stem}.out"))

        unpaired = (set(ins) ^ set(outs))
        print(f"installed {len(paired)} case(s) into {os.path.relpath(out_dir, os.getcwd())}/")
        if collisions:
            print(f"warning: {len(collisions)} duplicate numeric stem(s) across subfolders — kept "
                  f"last seen, others dropped: {sorted(collisions)[:10]}")
        if unpaired:
            print(f"warning: {len(unpaired)} unpaired file(s) skipped: {sorted(unpaired)[:10]}")
        print(f"run: ./target/release/chezzi run judge/run.chz {slug}")
    finally:
        if tmp:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
