import sys, random
random.seed(int(sys.argv[1]))
if random.random() < 0.5:
    n = random.randint(1, 1000); hi = random.choice([10, 1000, 10**9])  # small -> drives the brute oracle
else:
    n = random.randint(1001, 2 * 10**5); hi = 10**9
print(n)
print(*[random.randint(1, hi) for _ in range(n)])
