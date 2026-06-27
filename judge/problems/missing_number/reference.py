import sys
d = sys.stdin.read().split()
n = int(d[0]); vals = list(map(int, d[1:n]))
# Independent of the solution's sum-formula: find the absent value by set membership. O(n), all sizes.
present = set(vals)
for i in range(1, n + 1):
    if i not in present:
        print(i); break
