import sys
# CSES Playlist 1141: 1 <= n <= 2e5, values 1..1e9. Longest run of distinct values (sliding window).
def case(k):
    if k == 0:                       # single song
        return "1\n42"
    n = 200000
    if k == 1:                       # all equal -> longest distinct run is 1
        return f"{n}\n" + ("9 " * n).strip()
    if k == 2:                       # all distinct -> answer n
        return f"{n}\n" + " ".join(map(str, range(1, n + 1)))
    if k == 3:                       # max value present
        return "2\n1000000000 1000000000"
    return "6\n1 2 1 2 1 2"          # tight alternation -> answer 2

COUNT = 5
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
