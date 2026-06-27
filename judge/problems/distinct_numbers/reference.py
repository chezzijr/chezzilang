import sys
d = sys.stdin.read().split()
n = int(d[0]); vals = list(map(int, d[1:1+n]))
if n <= 1500:   # brute force: count first occurrences, no hash set (independent of the Set solution)
    print(sum(1 for i, v in enumerate(vals) if v not in vals[:i]))
else:
    print(len(set(vals)))
