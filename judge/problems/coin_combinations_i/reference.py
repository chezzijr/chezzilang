import sys
sys.setrecursionlimit(100000)
d = sys.stdin.read().split()
n = int(d[0]); x = int(d[1]); cs = list(map(int, d[2:2+n]))
MOD = 10**9 + 7
if x <= 18:   # brute force: count ordered coin sequences summing to x, NO DP table (def. of the count)
    def cnt(rem):
        return 1 if rem == 0 else sum(cnt(rem - c) for c in cs if c <= rem)
    print(cnt(x) % MOD)
else:
    w = [0]*(x+1); w[0] = 1
    for s in range(1, x+1):
        a = 0
        for c in cs:
            if c <= s: a = (a + w[s-c]) % MOD
        w[s] = a
    print(w[x])
