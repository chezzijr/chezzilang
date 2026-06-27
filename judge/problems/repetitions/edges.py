import sys
# CSES Repetitions: string of A/C/G/T, 1 <= n <= 1e6; length of the longest single-char run.
def case(k):
    if k == 0:
        return "A"                       # single char -> 1
    if k == 1:
        return "ACGT"                    # all distinct -> 1
    if k == 2:
        return "A" * 1000000             # max length, all same -> 1e6
    if k == 3:
        return "AAATTTTGG"               # longest run is the four T's -> 4
    return "A" * 999999 + "C"            # near-max run then a break -> 999999

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
