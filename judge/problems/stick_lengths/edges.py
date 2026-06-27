import sys
# CSES Stick Lengths: 1 <= n <= 2e5, 1 <= p_i <= 1e9; min total |p_i - target|.
def case(k):
    if k == 0:
        return "1\n7"                                  # single stick -> 0
    if k == 1:
        return "2\n1 1000000000"                       # two extremes -> 999999999
    n = 200000
    if k == 2:
        return f"{n}\n" + ("1000000000 " * n).strip()  # all equal at max -> 0 (i64 values, zero cost)
    if k == 3:
        return f"{n}\n" + " ".join(map(str, range(1, n + 1)))   # 1..n
    half = ("1 " * (n // 2)) + ("1000000000 " * (n - n // 2))   # half min, half max -> huge i64 cost
    return f"{n}\n" + half.strip()

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
