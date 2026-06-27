import sys
# Independent oracle. Small: build the actual factorial and strip trailing zeros (the literal
# definition — independent of the 5-counting trick). Large: Legendre formula (stress path).
n = int(sys.stdin.read())
if n <= 2000:
    f = 1
    for i in range(2, n + 1):
        f *= i
    z = 0
    while f % 10 == 0:
        z += 1
        f //= 10
    print(z)
else:
    z = 0
    p = 5
    while p <= n:
        z += n // p
        p *= 5
    print(z)
