import sys, random
random.seed(int(sys.argv[1]))
if random.random() < 0.5:
    n = random.randint(1, 1500); hi = random.choice([5, 50, 10**9])
else:
    n = random.randint(1501, 2*10**5); hi = 10**9
print(n)
print(*[random.randint(1, hi) for _ in range(n)])
