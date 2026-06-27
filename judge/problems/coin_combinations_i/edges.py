import sys
# CSES Coin Combinations I: n<=100 coins (1..1e6), x<=2e5; count ordered coin sequences summing to x, mod 1e9+7.
# k=0..2 keep x<=18 so the reference's brute-force branch stays bounded; k=3,4 drive the DP branch.
def case(k):
    if k == 0:   # x=1, single unit coin -> 1 way
        return "1 1\n1"
    if k == 1:   # single coin exactly equal to x -> 1 way
        return "1 5\n5"
    if k == 2:   # single coin larger than x -> 0 ways
        return "1 5\n7"
    if k == 3:   # max x, unit coin only -> 1 way (DP path, full table)
        return "1 200000\n1"
    # n=100 coins, large x -> stresses the O(x*n) DP and the mod arithmetic
    return "100 200000\n" + " ".join(map(str, range(1, 101)))

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
