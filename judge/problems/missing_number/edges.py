import sys
# CSES Missing Number: 2 <= n <= 2e5, the line is 1..n with exactly one value dropped (any order).
# Stresses the sum boundary (sum 1..2e5 ~ 2e10 > i32, fits i64) and the n=2 corner.
def case(k):
    if k == 0:   # smallest n, drop the low end
        return "2\n2"
    if k == 1:   # smallest n, drop the high end
        return "2\n1"
    n = 200000
    if k == 2:   # max n, drop first
        vals = range(2, n + 1)
    elif k == 3: # max n, drop last
        vals = range(1, n)
    else:        # max n, drop the middle
        mid = n // 2
        vals = (i for i in range(1, n + 1) if i != mid)
    return f"{n}\n" + " ".join(map(str, vals))

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
