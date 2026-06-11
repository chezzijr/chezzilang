# Bench: recursive calls. fib(30) — naive double recursion, no memoization.

def fib(n):
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)

print(fib(30))
