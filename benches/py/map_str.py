# Bench: string-keyed map get-then-increment, 1000 distinct keys x 1000 rounds.
# CPython dict counterpart of benches/chz/map_str.chz. Prints 1000000.

def main():
    keys = []
    i = 0
    while i < 1000:
        keys.append("key-" + str(i))
        i += 1

    m = {}
    n = 0
    while n < 1000:
        m[keys[n]] = 0
        n += 1

    r = 0
    while r < 1000:
        j = 0
        while j < 1000:
            k = keys[j]
            m[k] = m[k] + 1
            j += 1
        r += 1

    total = 0
    p = 0
    while p < 1000:
        total += m[keys[p]]
        p += 1
    print(total)

main()
