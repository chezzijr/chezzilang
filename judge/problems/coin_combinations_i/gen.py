import sys, random
random.seed(int(sys.argv[1]))
if random.random() < 0.5:
    x = random.randint(1, 18); n = random.randint(1, 5)
    coins = random.sample(range(1, 19), n)        # small distinct coins, enumeration stays bounded
else:
    x = random.randint(1, 2*10**5); n = random.randint(1, 100)
    coins = random.sample(range(1, 10**6 + 1), n)
print(n, x)
print(*coins)
