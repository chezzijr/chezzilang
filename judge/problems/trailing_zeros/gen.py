import sys, random
random.seed(int(sys.argv[1]))
if random.random() < 0.5:
    print(random.randint(1, 2000))          # drives the true-factorial brute oracle
else:
    print(random.randint(2001, 10**9))      # drives the formula (stresses i64)
