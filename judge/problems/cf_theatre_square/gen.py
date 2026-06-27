import sys, random
random.seed(int(sys.argv[1]))
if random.random() < 0.5:
    n = random.randint(1, 10000); m = random.randint(1, 10000); a = random.randint(1, 10000)
else:
    n = random.randint(1, 10**9); m = random.randint(1, 10**9); a = random.randint(1, 10**9)
print(n, m, a)
