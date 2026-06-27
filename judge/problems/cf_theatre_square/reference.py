import sys
n, m, a = map(int, sys.stdin.read().split())
if max(n, m) <= 10000:   # brute force: step a-by-a across each side and count (independent of the formula)
    rows = 0; y = 0
    while y < n: rows += 1; y += a
    cols = 0; y = 0
    while y < m: cols += 1; y += a
    print(rows * cols)
else:
    print(((n + a - 1)//a) * ((m + a - 1)//a))
