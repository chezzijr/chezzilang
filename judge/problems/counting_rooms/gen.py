import sys, random
random.seed(int(sys.argv[1]))
# CSES allows up to 1000x1000; capped at 250x250 here to keep both the Python oracle and the Chezzi
# run fast across many generated cases (the 1000x1000 worst case is covered by a committed sample).
n = random.randint(1, 250); m = random.randint(1, 250)
print(n, m)
for _ in range(n):
    print(''.join(random.choice('.#') for _ in range(m)))
