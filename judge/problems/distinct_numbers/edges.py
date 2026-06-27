import sys
# CSES Distinct Numbers: 1 <= n <= 2e5, values 1..1e9. Count of distinct values.
def case(k):
    if k == 0:                       # single element
        return "1\n7"
    n = 200000
    if k == 1:                       # max n, all equal -> answer 1
        return f"{n}\n" + ("5 " * n).strip()
    if k == 2:                       # max n, all distinct -> answer n
        return f"{n}\n" + " ".join(map(str, range(1, n + 1)))
    if k == 3:                       # extreme values, all equal at the max
        return "3\n1000000000 1000000000 1000000000"
    return "2\n1 1000000000"         # min and max value together -> answer 2

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
