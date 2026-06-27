import sys
# Independent oracle. Small: brute-force extend every start while the char repeats (O(n^2),
# too dumb to be subtly wrong). Large: single forward pass (stress path).
s = sys.stdin.read().strip()
n = len(s)
if n <= 2000:
    best = 0
    for i in range(n):
        j = i
        while j < n and s[j] == s[i]:
            j += 1
        best = max(best, j - i)
    print(best)
else:
    best = cur = 0
    prev = ""
    for c in s:
        cur = cur + 1 if c == prev else 1
        prev = c
        best = max(best, cur)
    print(best)
