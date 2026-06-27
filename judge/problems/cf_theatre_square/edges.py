import sys
# Codeforces 1A Theatre Square: 1 <= n,m,a <= 1e9; tiles ceil(n/a)*ceil(m/a). Stresses the i64 product boundary.
CASES = [
    "1 1 1",                              # minimum everything -> 1
    "1000000000 1000000000 1",           # max field, unit tile -> 1e18 (i64 product boundary)
    "1000000000 1000000000 1000000000",  # tile covers the whole field -> 1
    "12 8 4",                            # a divides both sides exactly -> 3*2 = 6
    "5 5 1",                             # a=1 -> n*m
    "7 7 7",                            # n = m = a -> 1
]
if len(sys.argv) < 2:
    print(len(CASES))
else:
    print(CASES[int(sys.argv[1])])
