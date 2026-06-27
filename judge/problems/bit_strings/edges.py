import sys
# CSES Bit Strings: 1 <= n <= 1e6; print 2^n mod 1e9+7.
CASES = [
    "1",        # 2
    "2",        # 4
    "30",       # 2^30 > MOD -> exercises the modulo
    "999999",   # near max
    "1000000",  # max n -> full doubling loop
]
if len(sys.argv) < 2:
    print(len(CASES))
else:
    print(CASES[int(sys.argv[1])])
