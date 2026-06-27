import sys
# Independent oracle: iterative subset-sum reachability (set DP), a different shape than the
# recursive include/exclude split. Every reachable group-1 sum -> min |total - 2*sum|.
d = sys.stdin.read().split()
n = int(d[0]); w = list(map(int, d[1:1 + n]))
total = sum(w)
sums = {0}
for x in w:
    sums |= {s + x for s in sums}
print(min(abs(total - 2 * s) for s in sums))
