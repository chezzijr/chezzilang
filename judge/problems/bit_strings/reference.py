import sys
# Independent oracle: fast modular exponentiation (pow), a different algorithm than the doubling loop.
n = int(sys.stdin.read())
print(pow(2, n, 10**9 + 7))
