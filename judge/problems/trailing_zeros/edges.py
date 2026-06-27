import sys
# CSES Trailing Zeros: 1 <= n <= 1e9; trailing zeros of n!.
CASES = [
    "1",            # 0! path -> 0 (1! = 1)
    "4",            # below the first factor of 5 -> 0
    "5",            # first 5 -> 1
    "24",           # just below 25 -> 4
    "25",           # second power of 5 kicks in -> 6
    "1000000000",   # max n -> formula
]
if len(sys.argv) < 2:
    print(len(CASES))
else:
    print(CASES[int(sys.argv[1])])
