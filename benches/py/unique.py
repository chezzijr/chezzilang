# Bench: list(dict.fromkeys(xs)), 500000 ints, 250000 distinct (W8-34). Prints 250000.

def main():
    xs = [i % 250000 for i in range(500000)]
    print(len(list(dict.fromkeys(xs))))

main()
