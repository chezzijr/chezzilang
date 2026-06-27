import sys
lines = sys.stdin.read().splitlines()
n, m = map(int, lines[0].split())
g = [lines[1 + i] for i in range(n)]
# Independent of the solution's stack flood-fill: count connected floor regions by union-find.
parent = list(range(n * m))
def find(x):
    while parent[x] != x:
        parent[x] = parent[parent[x]]; x = parent[x]
    return x
def union(a, b):
    ra, rb = find(a), find(b)
    if ra != rb: parent[ra] = rb
for r in range(n):
    for c in range(m):
        if g[r][c] == '.':
            if r + 1 < n and g[r + 1][c] == '.': union(r*m + c, (r+1)*m + c)
            if c + 1 < m and g[r][c + 1] == '.': union(r*m + c, r*m + c + 1)
roots = {find(r*m + c) for r in range(n) for c in range(m) if g[r][c] == '.'}
print(len(roots))
