import sys
# CSES Counting Rooms: grid up to 1000x1000 of '.' (floor) / '#' (wall); count 4-connected floor regions.
def case(k):
    if k == 0:                                   # single floor cell -> 1 room
        return "1 1\n."
    if k == 1:                                   # single wall cell -> 0 rooms
        return "1 1\n#"
    if k == 2:                                   # all wall -> 0 rooms
        return "5 5\n" + "\n".join(["#####"] * 5)
    if k == 3:                                   # max grid, one giant room -> deep flood fill (stack stress)
        row = "." * 1000
        return "1000 1000\n" + "\n".join([row] * 1000)
    if k == 4:                                   # checkerboard -> every floor cell is its own 1-cell room
        n = 51
        rows = ["".join("." if (r + c) % 2 == 0 else "#" for c in range(n)) for r in range(n)]
        return f"{n} {n}\n" + "\n".join(rows)
    if k == 5:                                   # single row
        return "1 9\n.#..#.###"
    return "9 1\n" + "\n".join(list(".#..#.###"))  # single column

COUNT = 7
if len(sys.argv) < 2:
    print(COUNT)
else:
    print(case(int(sys.argv[1])))
