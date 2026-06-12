# Bench: map insert + lookup, 200k inserts + 1M lookups. CPython dict counterpart of
# benches/chz/map.chz. Prints 199999000000.

def main():
    m = {}
    for i in range(200000):
        m[i] = i * 2
    total = 0
    for _ in range(5):
        for j in range(200000):
            total += m[j]
    print(total)

main()
