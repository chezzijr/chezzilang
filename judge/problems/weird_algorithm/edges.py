import sys
# Deterministic boundary cases (index protocol: no arg -> count, k -> k-th case).
# Collatz, CSES 1068: 1 <= n <= 1e6. Stresses i64 intermediates (the 3x+1 peak) and chain length.
CASES = [
    "1",        # shortest possible (just prints 1)
    "2",        # one halving step
    "1000000",  # max n
    "999999",   # odd just below max
    "27",       # classic long chain, peak 9232
    "524288",   # 2^19: pure halving all the way down
    "837799",   # max step count under 1e6 -> largest i64 peak (~2.97e6)
]
if len(sys.argv) < 2:
    print(len(CASES))
else:
    print(CASES[int(sys.argv[1])])
