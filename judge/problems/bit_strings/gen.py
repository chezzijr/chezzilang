import sys, random
random.seed(int(sys.argv[1]))
print(random.randint(1, 10**6))            # CSES: 1 <= n <= 1e6
