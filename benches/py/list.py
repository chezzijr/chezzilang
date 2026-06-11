# Bench: push + sum, 2M elements.

def main():
    xs = []
    i = 0
    while i < 2000000:
        xs.append(i)
        i += 1
    total = 0
    for x in xs:
        total += x
    print(total)

main()
