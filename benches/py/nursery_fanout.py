# Ancestor reference for docs/gaps.md W8-7 / W8-8 — the CPython equivalent of
# examples/primes_parallel.chz: 4 CPU-bound tasks on a ThreadPoolExecutor, identical
# trial-division workload, identical ranges. N (max_workers) comes from argv.
#
# WEAK ORACLE for CPU work: CPython 3.14 still has a GIL by default, so wall time does not scale
# with max_workers. Its value here is the *sys* column (does the runtime thrash as N rises?),
# not the real column.
import sys
from concurrent.futures import ThreadPoolExecutor


def is_prime(n):
    if n < 2:
        return False
    i = 2
    while i * i <= n:
        if n % i == 0:
            return False
        i += 1
    return True


def count_primes(lo, hi):
    return sum(1 for n in range(lo, hi) if is_prime(n))


def main():
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 4
    bounds = [(2, 500000), (500000, 1000000), (1000000, 1500000), (1500000, 2000000)]
    with ThreadPoolExecutor(max_workers=n) as ex:
        total = sum(ex.map(lambda b: count_primes(*b), bounds))
    print(f"primes below 2,000,000: {total}")


main()
