// Ancestor reference for docs/gaps.md W8-7 / W8-8 — the Go equivalent of
// examples/primes_parallel.chz: 4 CPU-bound tasks in one WaitGroup, identical trial-division
// workload, identical ranges. Sweep GOMAXPROCS to compare against chezzi's --threads.
package main

import (
	"fmt"
	"sync"
)

func isPrime(n int) bool {
	if n < 2 {
		return false
	}
	for i := 2; i*i <= n; i++ {
		if n%i == 0 {
			return false
		}
	}
	return true
}

func countPrimes(lo, hi int) int {
	c := 0
	for n := lo; n < hi; n++ {
		if isPrime(n) {
			c++
		}
	}
	return c
}

func main() {
	bounds := [][2]int{{2, 500000}, {500000, 1000000}, {1000000, 1500000}, {1500000, 2000000}}
	out := make(chan int, len(bounds))
	var wg sync.WaitGroup
	for _, b := range bounds {
		wg.Add(1)
		go func(lo, hi int) {
			defer wg.Done()
			out <- countPrimes(lo, hi)
		}(b[0], b[1])
	}
	wg.Wait()
	close(out)
	total := 0
	for c := range out {
		total += c
	}
	fmt.Printf("primes below 2,000,000: %d\n", total)
}
