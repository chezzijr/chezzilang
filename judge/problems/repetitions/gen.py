import sys, random
random.seed(int(sys.argv[1]))
A = "ACGT"
if random.random() < 0.5:
    n = random.randint(1, 1500); alpha = random.choice([1, 2, 4])  # small alphabet -> long runs, drives brute
else:
    n = random.randint(1501, 10**6); alpha = 4
print("".join(random.choice(A[:alpha]) for _ in range(n)))
