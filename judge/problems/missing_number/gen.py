import sys, random
random.seed(int(sys.argv[1]))
n = random.randint(2, 2*10**5)             # 2 <= n <= 2e5
drop = random.randint(1, n)
vals = [i for i in range(1, n+1) if i != drop]
random.shuffle(vals)
print(n); print(*vals)
