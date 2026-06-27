import sys, random
random.seed(int(sys.argv[1]))
n = random.randint(1, 20)                       # CSES: 1 <= n <= 20
hi = random.choice([5, 100, 10**9])            # small weights (many collisions) and large (i64)
print(n)
print(*[random.randint(1, hi) for _ in range(n)])
