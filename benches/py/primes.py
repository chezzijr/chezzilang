# Bench: while + %, primes below 200k (sequential).

def is_prime(n):
    if n < 2:
        return False
    i = 2
    while i * i <= n:
        if n % i == 0:
            return False
        i += 1
    return True

def main():
    c = 0
    n = 2
    while n < 200000:
        if is_prime(n):
            c += 1
        n += 1
    print(c)

main()
