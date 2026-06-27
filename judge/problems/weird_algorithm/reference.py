import sys
# Collatz is defined BY the simulation, so there is no independent alternative algorithm; this oracle
# pins Chezzi's integer arithmetic (/ and %, values up to ~1.5e10) against CPython's.
n = int(sys.stdin.read().split()[0])
seq = []
while n != 1:
    seq.append(n); n = n//2 if n % 2 == 0 else 3*n + 1
seq.append(1)
print(*seq)
