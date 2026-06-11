# Bench: int add, 20M iterations.

def main():
    total = 0
    i = 0
    while i < 20000000:
        total += i
        i += 1
    print(total)

main()
