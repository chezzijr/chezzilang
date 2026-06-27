import sys
d = sys.stdin.read().split()
n = int(d[0]); vals = list(map(int, d[1:1+n]))
if n <= 1500:   # brute force: try every start, extend while distinct (independent of the sliding window)
    best = 0
    for i in range(n):
        seen = set(); j = i
        while j < n and vals[j] not in seen:
            seen.add(vals[j]); j += 1
        best = max(best, j - i)
    print(best)
else:
    last = {}; start = 0; best = 0
    for i, v in enumerate(vals):
        if v in last and last[v] >= start: start = last[v] + 1
        last[v] = i; best = max(best, i - start + 1)
    print(best)
