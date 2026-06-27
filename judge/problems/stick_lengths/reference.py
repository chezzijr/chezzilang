import sys
# Independent oracle. Small: try EVERY stick value as the target and take the min total cost
# (the L1 optimum is at a stick value — independent of the sort+median pick). Large: median.
d = sys.stdin.read().split()
n = int(d[0]); p = list(map(int, d[1:1 + n]))
if n <= 1000:
    print(min(sum(abs(v - t) for v in p) for t in p))
else:
    p.sort()
    m = p[n // 2]
    print(sum(abs(v - m) for v in p))
