#!/usr/bin/env python3
"""Fetch a problem *statement* (not the hidden test data) and scaffold judge/problems/<slug>/.

The hidden test data is the authors' IP and gated (no API; solve-first). But the **statement** is
public — this pulls it, extracts the public sample(s), and lays down the directory so you only have to
write the four reasoning files (reference.py, gen.py, edges.py, solution.chz). It deliberately does
NOT generate those: an independent oracle and an in-domain generator need understanding, not scraping.

What it writes under judge/problems/<slug>/:
    meta.toml           name, source, cses_id/cf_id, url
    statement.md        the statement as plain text (read this to write the four files)
    samples/N.in,N.out  the public sample case(s) from the statement

It never overwrites an existing solution.chz / reference.py / gen.py / edges.py (your work is safe);
statement.md, meta.toml and samples/ are refreshed. Until a solution.chz exists the slug is simply
skipped by run.chz, so a half-scaffolded problem never pollutes a harness run.

Usage:
    python3 judge/fetch_problem.py <url> [slug]
    python3 judge/fetch_problem.py https://cses.fi/problemset/task/1068
    python3 judge/fetch_problem.py https://codeforces.com/problemset/problem/4/A four_a

Sources: CSES (cses.fi) — fully supported. Codeforces (codeforces.com) — best-effort; CF fronts pages
with Cloudflare and may answer 403 to scripted requests from datacenter IPs (works from most home
connections). If CF blocks you, save the page as HTML in a browser and pass the file path as <url>.
"""
import argparse
import html
import os
import re
import sys
import urllib.error
import urllib.request

UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36"
HERE = os.path.dirname(os.path.abspath(__file__))


def load(url):
    """Fetch a URL (browser UA) or read a local .html file path."""
    if os.path.isfile(url):
        with open(url, encoding="utf-8", errors="replace") as f:
            return f.read()
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        if e.code == 403:
            sys.exit(f"HTTP 403 (likely Cloudflare). Save the page as .html in a browser and pass the "
                     f"file path instead of the URL.\n  {url}")
        sys.exit(f"fetch failed ({e.code}): {url}")


def extract_div(text, cls):
    """Return the inner HTML of the first <div class="...cls..."> ... </div>, brace-balanced."""
    m = re.search(r'<div[^>]*class="[^"]*\b' + re.escape(cls) + r'\b[^"]*"[^>]*>', text)
    if not m:
        return None
    i = m.end()
    depth = 1
    for mm in re.finditer(r"<div\b|</div>", text[i:]):
        if mm.group() == "</div>":
            depth -= 1
            if depth == 0:
                return text[i:i + mm.start()]
        else:
            depth += 1
    return text[i:]


def to_text(frag):
    """Lightweight HTML -> readable plain text (keeps block structure as newlines)."""
    frag = re.sub(r"(?is)<(script|style).*?</\1>", "", frag)
    frag = re.sub(r"(?i)<br\s*/?>", "\n", frag)
    frag = re.sub(r"(?i)<li[^>]*>", "\n- ", frag)
    frag = re.sub(r"(?i)</(p|li|div|h1|h2|h3|h4|tr|ul|ol|pre|table)>", "\n", frag)
    frag = re.sub(r"<[^>]+>", " ", frag)
    frag = html.unescape(frag)
    out = []
    for line in frag.split("\n"):
        line = re.sub(r"[ \t]+", " ", line).strip()
        if line == "" and (not out or out[-1] == ""):
            continue
        out.append(line)
    return "\n".join(out).strip()


def pre_blocks(frag):
    """Inner text of each <pre>...</pre>, whitespace-trimmed, newlines preserved."""
    blocks = []
    for raw in re.findall(r"(?is)<pre[^>]*>(.*?)</pre>", frag):
        raw = re.sub(r"(?i)<br\s*/?>", "\n", raw)
        raw = re.sub(r"<[^>]+>", "", raw)
        blocks.append(html.unescape(raw).strip("\n"))
    return blocks


def slugify(title):
    s = re.sub(r"[^a-z0-9]+", "_", title.lower()).strip("_")
    return s or "problem"


def parse_cses(text, url):
    title = ""
    m = re.search(r"(?is)<h1[^>]*>(.*?)</h1>", text)
    if m:
        title = to_text(m.group(1)).strip()
    if not title:
        m = re.search(r"(?is)<title>\s*CSES\s*-\s*(.*?)</title>", text)
        title = m.group(1).strip() if m else "Problem"
    content = extract_div(text, "content") or text
    statement = to_text(content)
    samples = pre_blocks(content)            # CSES: consecutive pre blocks = input, output, input, ...
    pairs = [(samples[i], samples[i + 1]) for i in range(0, len(samples) - 1, 2)]
    pid = re.search(r"/task/(\d+)", url)
    meta = {"id_key": "cses_id", "id_val": pid.group(1) if pid else "", "source": "CSES Problem Set"}
    return title, statement, pairs, meta


def parse_codeforces(text, url):
    stmt_div = extract_div(text, "problem-statement") or text
    title = ""
    m = re.search(r'(?is)<div[^>]*class="[^"]*\btitle\b[^"]*"[^>]*>(.*?)</div>', stmt_div)
    if m:
        title = to_text(m.group(1)).strip()
        title = re.sub(r"^[A-Z]\.\s*", "", title)   # drop the "A. " index prefix
    statement = to_text(stmt_div)
    ins = [pre_blocks(d)[0] for d in re.findall(r'(?is)<div[^>]*class="input"[^>]*>(.*?)</div>\s*</div>', text) if pre_blocks(d)]
    outs = [pre_blocks(d)[0] for d in re.findall(r'(?is)<div[^>]*class="output"[^>]*>(.*?)</div>\s*</div>', text) if pre_blocks(d)]
    pairs = list(zip(ins, outs))
    cid = re.search(r"/problem/(\d+)/([A-Za-z0-9]+)", url) or re.search(r"/contest/(\d+)/problem/([A-Za-z0-9]+)", url)
    meta = {"id_key": "cf_id", "id_val": (cid.group(1) + cid.group(2)) if cid else "", "source": "Codeforces"}
    return title or "Problem", statement, pairs, meta


def write_if_absent(path, body):
    if os.path.exists(path):
        return False
    with open(path, "w") as f:
        f.write(body)
    return True


STUBS = {
    "reference.py": "# Independent Python oracle: stdin -> correct stdout. Use a DIFFERENT algorithm\n"
                    "# than solution.chz (ideally an obvious brute force on small inputs + a fast path on\n"
                    "# large). It must NOT share solution.chz's assumptions, or the differential proves nothing.\n"
                    "import sys\n"
                    "d = sys.stdin.read().split()\n"
                    "# TODO: parse, compute, print the answer\n",
    "gen.py": "# Emit ONE random in-domain input, seeded by argv[1]. Mix small (drives reference.py's\n"
              "# brute-force branch -> proves correctness) and large (stresses Chezzi) inputs.\n"
              "import sys, random\n"
              "random.seed(int(sys.argv[1]))\n"
              "# TODO: print a valid random input within the stated constraints\n",
    "edges.py": "# Deterministic boundary cases (index protocol): no arg -> count, argv[1]=k -> k-th input.\n"
                "# Pin the corners gen.py misses: min/max sizes, all-equal, value extremes, 0, exact multiples.\n"
                "import sys\n"
                "CASES = [\n"
                "    # TODO: \"<the input for edge 0>\",\n"
                "]\n"
                "if len(sys.argv) < 2:\n"
                "    print(len(CASES))\n"
                "else:\n"
                "    print(CASES[int(sys.argv[1])])\n",
    "solution.chz": "# Hand-written Chezzi solution UNDER TEST. Read stdin (std.io.read_line), print the answer.\n"
                    "import std.io\n\n"
                    "fn main():\n"
                    "    pass  # TODO: solve\n\n"
                    "main()\n",
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("url", help="problem URL (cses.fi / codeforces.com) or a saved .html file path")
    ap.add_argument("slug", nargs="?", help="directory name (default: derived from the title)")
    args = ap.parse_args()

    text = load(args.url)
    src = args.url.lower()
    if "codeforces" in src:
        title, statement, pairs, meta = parse_codeforces(text, args.url)
    elif "cses" in src or "<title>cses" in text.lower():
        title, statement, pairs, meta = parse_cses(text, args.url)
    else:
        sys.exit("unknown source — supported: cses.fi, codeforces.com (or a saved page from them).")

    slug = args.slug or slugify(title)
    pdir = os.path.join(HERE, "problems", slug)
    os.makedirs(os.path.join(pdir, "samples"), exist_ok=True)

    url_for_meta = args.url if args.url.startswith("http") else meta.get("source", "")
    with open(os.path.join(pdir, "meta.toml"), "w") as f:
        f.write(f'name = "{title}"\n')
        f.write(f'source = "{meta["source"]}"\n')
        if meta["id_val"]:
            f.write(f'{meta["id_key"]} = {meta["id_val"]}\n' if meta["id_val"].isdigit()
                    else f'{meta["id_key"]} = "{meta["id_val"]}"\n')
        if args.url.startswith("http"):
            f.write(f'url = "{args.url}"\n')
    with open(os.path.join(pdir, "statement.md"), "w") as f:
        f.write(f"# {title}\n\nSource: {url_for_meta}\n\n{statement}\n")

    if not pairs:
        print(f"warning: no sample input/output detected — write samples/1.in and 1.out by hand")
    for i, (sin, sout) in enumerate(pairs, 1):
        with open(os.path.join(pdir, "samples", f"{i}.in"), "w") as f:
            f.write(sin.rstrip("\n") + "\n")
        with open(os.path.join(pdir, "samples", f"{i}.out"), "w") as f:
            f.write(sout.rstrip("\n") + "\n")

    created = [name for name, body in STUBS.items()
              if write_if_absent(os.path.join(pdir, name), body)]

    rel = os.path.relpath(pdir, os.getcwd())
    print(f"scaffolded {rel}/  ({title})")
    print(f"  statement.md + meta.toml + {len(pairs)} sample(s)")
    if created:
        print(f"  stubs created (fill these in): {', '.join(created)}")
    skipped = [n for n in STUBS if n not in created]
    if skipped:
        print(f"  kept existing (not overwritten): {', '.join(skipped)}")
    print(f"next: edit the four files, then  python3 judge/generate.py {slug} && ./target/release/chezzi run judge/run.chz {slug}")


if __name__ == "__main__":
    main()
