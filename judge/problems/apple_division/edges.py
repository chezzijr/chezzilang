import sys
# CSES Apple Division: 1 <= n <= 20, 1 <= p_i <= 1e9; min weight difference between the two groups.
def case(k):
    if k == 0:
        return "1\n1000000000"                          # single apple -> the whole weight (no split)
    if k == 1:
        return "2\n5 5"                                 # equal pair -> 0
    if k == 2:
        return "20\n" + " ".join(["1000000000"] * 20)   # n=20, all equal max -> 0 (full 2^20 tree, i64)
    if k == 3:
        return "20\n" + " ".join(str(i) for i in range(1, 21))  # 1..20
    return "3\n1 2 1000000000"                          # one dominating apple -> 999999997

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
